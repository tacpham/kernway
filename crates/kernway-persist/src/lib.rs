//! # kernway-persist
//!
//! Make in-memory state **durable** without paying for it on the hot path — the
//! snapshot + append-log model Redis uses for RDB + AOF, as a small reusable engine.
//!
//! The store keeps its data in memory (reads never touch disk). Every *mutation* is
//! also appended to a write-ahead log; periodically the whole state is written as a
//! snapshot and the log truncated. On startup the snapshot is loaded and the log
//! replayed, so a restart loses nothing. It is single-node (each process owns its
//! files) — the fast, dependency-free middle ground between pure in-memory (lost on
//! restart) and a shared store like Redis (a network hop, but shared across
//! instances).
//!
//! ## Not on the core (thread-per-core safe)
//!
//! `fsync` is blocking I/O and must never run on a request core. So the engine owns a
//! dedicated **writer thread**; a mutation sends its record over a channel and
//! `await`s a [`Durable`] — awaiting *yields the core* until the writer has synced,
//! rather than blocking it. Under [`Fsync::EveryWrite`] the future resolves only after
//! the data is on disk (zero loss); under [`Fsync::Batched`] it resolves as soon as
//! the record is buffered.
//!
//! ## Records must be idempotent
//!
//! A checkpoint writes the snapshot and *then* truncates the log; a crash in that
//! window leaves a few already-snapshotted records in the log, which are replayed on
//! top of the snapshot. That is harmless only if applying a record twice equals
//! applying it once — so design ops as `set`/`insert`/`remove`, not `increment`.
//!
//! ## Format
//!
//! - `snapshot` — the store's full-state bytes, written via a temp file + atomic
//!   rename so a partial snapshot never exists.
//! - `wal` — a sequence of records, each a little-endian `u32` length then that many
//!   bytes. A torn tail (a half-written final record from a crash) is ignored on load.
//!
//! The engine is byte-oriented: the store owns how it encodes a record and its
//! snapshot, so kernway-persist needs no serde and no knowledge of the domain.

#![forbid(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// When the writer forces data to disk with `fsync`.
#[derive(Clone, Copy, Debug)]
pub enum Fsync {
    /// `fsync` after every record — zero loss on crash, one disk sync per mutation.
    /// The right default for rarely-written state (a ban list); for per-request
    /// writes prefer [`Batched`](Fsync::Batched).
    EveryWrite,
    /// `fsync` at most once per interval — near-free, but a crash can lose up to the
    /// last interval of writes (Redis's `everysec`).
    Batched(Duration),
    /// Never `fsync` explicitly; rely on the OS to flush eventually. Fastest, least
    /// durable — a crash can lose whatever the OS had not yet written.
    Never,
}

/// A persistence failure — a disk error, or the writer thread having stopped.
#[derive(Debug, Clone)]
pub struct PersistError(pub String);

impl std::fmt::Display for PersistError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "persist error: {}", self.0)
    }
}

impl std::error::Error for PersistError {}

impl From<io::Error> for PersistError {
    fn from(e: io::Error) -> Self {
        PersistError(e.to_string())
    }
}

/// What [`Persister::open`] recovered from disk: the last snapshot (if any) and the
/// records logged since it. The store decodes the snapshot, then applies the records
/// in order, to rebuild its in-memory state.
pub struct Loaded {
    /// The full-state bytes of the last checkpoint, or `None` if there was none.
    pub snapshot: Option<Vec<u8>>,
    /// The records appended since that checkpoint, oldest first.
    pub records: Vec<Vec<u8>>,
}

// The cross-thread completion cell shared by an in-flight write and its writer-side
// acknowledgement. The writer sets `done` and wakes; the `Durable` future reads it.
#[derive(Default)]
struct Shared {
    done: Option<Result<(), PersistError>>,
    waker: Option<Waker>,
}

/// The writer's half of a pending write. Completing it (or dropping it) resolves the
/// caller's [`Durable`] — the `Drop` guard guarantees a caller never hangs even if
/// the writer dies mid-op.
struct Ack(Arc<Mutex<Shared>>);

impl Ack {
    fn complete(self, result: Result<(), PersistError>) {
        let mut shared = self.0.lock().unwrap();
        shared.done = Some(result);
        if let Some(waker) = shared.waker.take() {
            waker.wake();
        }
    }
}

impl Drop for Ack {
    fn drop(&mut self) {
        let mut shared = self.0.lock().unwrap();
        if shared.done.is_none() {
            shared.done = Some(Err(PersistError("persist writer dropped the write".into())));
            if let Some(waker) = shared.waker.take() {
                waker.wake();
            }
        }
    }
}

/// The future a write returns: `Ready` once the record is durable (per the
/// [`Fsync`] policy) or a [`PersistError`] if it could not be written. Awaiting it
/// yields the core to other tasks rather than blocking on disk.
pub struct Durable(Arc<Mutex<Shared>>);

impl Future for Durable {
    type Output = Result<(), PersistError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut shared = self.0.lock().unwrap();
        if let Some(result) = shared.done.take() {
            return Poll::Ready(result);
        }
        shared.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

// The commands the writer thread services.
enum Cmd {
    Append { record: Vec<u8>, ack: Ack },
    Checkpoint { snapshot: Vec<u8>, ack: Ack },
    Flush { ack: Ack },
    Shutdown,
}

/// A handle to a durable store. Cheap to share behind an `Arc`; `append`/`checkpoint`
/// hand work to the writer thread and return a [`Durable`] to await.
pub struct Persister {
    tx: Mutex<Sender<Cmd>>,
    handle: Option<JoinHandle<()>>,
}

impl Persister {
    /// Open (creating if needed) the store in `dir`, recovering any prior state.
    /// Returns the handle and the recovered [`Loaded`] — apply that to your in-memory
    /// state before serving.
    pub fn open(dir: impl AsRef<Path>, fsync: Fsync) -> io::Result<(Persister, Loaded)> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;

        // Recover: the snapshot, then the records logged after it.
        let snapshot = match std::fs::read(dir.join("snapshot")) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let records = read_wal(&dir.join("wal"))?;

        // The append handle the writer thread owns from here on.
        let wal = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(dir.join("wal"))?;
        let writer = Writer {
            dir: dir.clone(),
            wal,
            fsync,
            last_sync: Instant::now(),
            dirty: false,
        };

        let (tx, rx) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("kernway-persist".into())
            .spawn(move || writer.run(&rx))?;

        Ok((
            Persister {
                tx: Mutex::new(tx),
                handle: Some(handle),
            },
            Loaded { snapshot, records },
        ))
    }

    /// Append one record. Under [`Fsync::EveryWrite`] the returned future resolves
    /// only once the record is on disk.
    #[must_use]
    pub fn append(&self, record: Vec<u8>) -> Durable {
        self.dispatch(|ack| Cmd::Append { record, ack })
    }

    /// Write a full snapshot and truncate the log. Call it periodically (or at
    /// shutdown) to bound recovery time and log size.
    #[must_use]
    pub fn checkpoint(&self, snapshot: Vec<u8>) -> Durable {
        self.dispatch(|ack| Cmd::Checkpoint { snapshot, ack })
    }

    /// Force a `fsync` now (useful under [`Fsync::Batched`] before a critical point).
    #[must_use]
    pub fn flush(&self) -> Durable {
        self.dispatch(|ack| Cmd::Flush { ack })
    }

    fn dispatch(&self, make: impl FnOnce(Ack) -> Cmd) -> Durable {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let ack = Ack(Arc::clone(&shared));
        if self.tx.lock().unwrap().send(make(ack)).is_err() {
            // The writer is gone; complete the future with an error rather than hang.
            shared.lock().unwrap().done = Some(Err(PersistError("persist writer stopped".into())));
        }
        Durable(shared)
    }
}

impl Drop for Persister {
    fn drop(&mut self) {
        // Ask the writer to flush and stop, then wait for it — so a dropped Persister
        // guarantees everything buffered is on disk.
        let _ = self.tx.lock().unwrap().send(Cmd::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

// The dedicated writer: the only thing that touches the files.
struct Writer {
    dir: PathBuf,
    wal: File,
    fsync: Fsync,
    last_sync: Instant,
    dirty: bool,
}

impl Writer {
    fn run(mut self, rx: &Receiver<Cmd>) {
        while let Ok(cmd) = rx.recv() {
            match cmd {
                Cmd::Append { record, ack } => ack.complete(self.append(&record)),
                Cmd::Checkpoint { snapshot, ack } => ack.complete(self.checkpoint(&snapshot)),
                Cmd::Flush { ack } => ack.complete(self.sync()),
                Cmd::Shutdown => {
                    let _ = self.sync();
                    break;
                }
            }
        }
    }

    fn append(&mut self, record: &[u8]) -> Result<(), PersistError> {
        let len =
            u32::try_from(record.len()).map_err(|_| PersistError("record exceeds 4 GiB".into()))?;
        self.wal.write_all(&len.to_le_bytes())?;
        self.wal.write_all(record)?;
        self.dirty = true;
        self.maybe_sync()
    }

    fn maybe_sync(&mut self) -> Result<(), PersistError> {
        match self.fsync {
            Fsync::EveryWrite => self.sync(),
            Fsync::Batched(interval) if self.last_sync.elapsed() >= interval => self.sync(),
            Fsync::Batched(_) | Fsync::Never => Ok(()),
        }
    }

    fn sync(&mut self) -> Result<(), PersistError> {
        if self.dirty {
            self.wal.sync_data()?;
            self.dirty = false;
            self.last_sync = Instant::now();
        }
        Ok(())
    }

    fn checkpoint(&mut self, snapshot: &[u8]) -> Result<(), PersistError> {
        // Write the new snapshot to a temp file and atomically rename it into place,
        // so a crash never leaves a half-written snapshot.
        let tmp = self.dir.join("snapshot.tmp");
        let final_path = self.dir.join("snapshot");
        {
            let mut file = File::create(&tmp)?;
            file.write_all(snapshot)?;
            file.sync_data()?;
        }
        std::fs::rename(&tmp, &final_path)?;

        // The snapshot now supersedes the log; truncate it.
        self.wal.set_len(0)?;
        self.wal.seek(SeekFrom::Start(0))?;
        self.wal.sync_data()?;
        self.dirty = false;
        self.last_sync = Instant::now();
        Ok(())
    }
}

// Read every complete length-prefixed record from the WAL. A torn tail — a final
// record only partly written before a crash — is silently dropped.
fn read_wal(path: &Path) -> io::Result<Vec<Vec<u8>>> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;

    let mut records = Vec::new();
    let mut pos = 0;
    while pos + 4 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        let start = pos + 4;
        let end = match start.checked_add(len) {
            Some(end) if end <= bytes.len() => end,
            _ => break, // torn tail — stop at the last complete record
        };
        records.push(bytes[start..end].to_vec());
        pos = end;
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive a `Durable` to completion. The writer runs on another thread, so re-poll
    /// (yielding) until it resolves — the noop waker is fine since we spin.
    fn block_on<F: Future>(fut: F) -> F::Output {
        let mut fut = Box::pin(fut);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        // A unique-enough dir under the OS temp root; cleaned at the end of each test.
        let base = std::env::temp_dir().join(format!("kernway-persist-{name}"));
        let _ = std::fs::remove_dir_all(&base);
        base
    }

    #[test]
    fn records_replay_in_order_after_a_reopen() {
        let dir = temp_dir("replay");
        {
            let (p, loaded) = Persister::open(&dir, Fsync::EveryWrite).unwrap();
            assert!(
                loaded.snapshot.is_none() && loaded.records.is_empty(),
                "empty to start"
            );
            block_on(p.append(b"one".to_vec())).unwrap();
            block_on(p.append(b"two".to_vec())).unwrap();
            block_on(p.append(b"three".to_vec())).unwrap();
        } // drop flushes + joins the writer — a clean "restart".

        let (_p, loaded) = Persister::open(&dir, Fsync::EveryWrite).unwrap();
        assert!(loaded.snapshot.is_none());
        assert_eq!(
            loaded.records,
            vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_checkpoint_writes_the_snapshot_and_truncates_the_log() {
        let dir = temp_dir("checkpoint");
        {
            let (p, _) = Persister::open(&dir, Fsync::EveryWrite).unwrap();
            block_on(p.append(b"a".to_vec())).unwrap();
            block_on(p.append(b"b".to_vec())).unwrap();
            // Fold the state into a snapshot, then log one more record on top.
            block_on(p.checkpoint(b"STATE={a,b}".to_vec())).unwrap();
            block_on(p.append(b"c".to_vec())).unwrap();
        }

        let (_p, loaded) = Persister::open(&dir, Fsync::EveryWrite).unwrap();
        assert_eq!(loaded.snapshot.as_deref(), Some(b"STATE={a,b}".as_slice()));
        assert_eq!(
            loaded.records,
            vec![b"c".to_vec()],
            "only the post-checkpoint record remains"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_torn_tail_record_is_ignored() {
        let dir = temp_dir("torn");
        std::fs::create_dir_all(&dir).unwrap();
        // One good record, then a length header promising 10 bytes with only 3 present.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(b"ok!");
        bytes.extend_from_slice(&10u32.to_le_bytes());
        bytes.extend_from_slice(b"cut");
        std::fs::write(dir.join("wal"), &bytes).unwrap();

        let (_p, loaded) = Persister::open(&dir, Fsync::EveryWrite).unwrap();
        assert_eq!(
            loaded.records,
            vec![b"ok!".to_vec()],
            "the torn final record is dropped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn batched_writes_are_recovered_on_a_clean_shutdown() {
        let dir = temp_dir("batched");
        {
            let (p, _) = Persister::open(&dir, Fsync::Batched(Duration::from_secs(3600))).unwrap();
            // Even without an fsync per write, dropping the Persister flushes on exit.
            block_on(p.append(b"x".to_vec())).unwrap();
            block_on(p.append(b"y".to_vec())).unwrap();
        }
        let (_p, loaded) = Persister::open(&dir, Fsync::Never).unwrap();
        assert_eq!(loaded.records, vec![b"x".to_vec(), b"y".to_vec()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
