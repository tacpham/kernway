//! File-backed [`Activity`](crate::activity::Activity) (feature = `persist`).
//!
//! The live "who's on the site" map stays in memory, but every recorded request is
//! also appended to a local log and the map snapshotted periodically, so the view
//! survives a restart with **no Redis**. Single-node: each process owns its files.
//!
//! Activity records on every request (like a session `touch`), so under
//! [`Fsync::EveryWrite`] that is an fsync per request — prefer [`Fsync::Batched`] for a
//! busy site. Recording is best-effort at the middleware anyway (a lost activity
//! record never affects the response), so a disk error here is surfaced but the caller
//! is free to ignore it, exactly as with the in-memory and Redis backends.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use kernway_core::layer::BoxFuture;
use kernway_persist::{Fsync, Loaded, PersistError, Persister};

use crate::activity::{decode_visitor, encode_visitor, ActiveVisitor, Activity, InMemoryActivity};
use crate::session::StoreError;

/// Snapshot once this many records have been logged, to bound the log under steady
/// per-request traffic.
const CHECKPOINT_EVERY: usize = 2000;

/// An [`Activity`] store whose in-memory map is mirrored to a local snapshot + log.
/// Drop-in for `InMemoryActivity`, but durable across a restart without a server.
///
/// ```rust,ignore
/// let activity = Arc::new(FileBackedActivity::open("data/activity", Duration::from_secs(300), Fsync::Batched(Duration::from_secs(1)))?);
/// app.layer(ActivityTracking::new(activity));
/// ```
pub struct FileBackedActivity {
    inner: InMemoryActivity,
    persist: Arc<Persister>,
    since_checkpoint: Arc<AtomicUsize>,
}

impl FileBackedActivity {
    /// Open (creating if needed) the store in `dir`, replaying prior records into the
    /// in-memory map. `window` is the active window; `fsync` the durability trade.
    pub fn open(dir: impl AsRef<Path>, window: Duration, fsync: Fsync) -> io::Result<Self> {
        let (persist, loaded) = Persister::open(dir, fsync)?;
        let inner = InMemoryActivity::new(window);
        replay(&inner, &loaded);
        Ok(Self { inner, persist: Arc::new(persist), since_checkpoint: Arc::new(AtomicUsize::new(0)) })
    }

    /// The whole map, framed as length-prefixed records for a snapshot.
    fn snapshot_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for visitor in self.inner.snapshot() {
            let rec = encode_visitor(&visitor);
            out.extend_from_slice(&(rec.len() as u32).to_le_bytes());
            out.extend_from_slice(&rec);
        }
        out
    }
}

impl Activity for FileBackedActivity {
    fn record(&self, visitor: ActiveVisitor) -> BoxFuture<'_, Result<(), StoreError>> {
        let record = encode_visitor(&visitor);
        Box::pin(async move {
            self.inner.record(visitor).await?;
            self.persist.append(record).await.map_err(backend)?;
            if self.since_checkpoint.fetch_add(1, Ordering::Relaxed) + 1 >= CHECKPOINT_EVERY {
                self.since_checkpoint.store(0, Ordering::Relaxed);
                self.persist.checkpoint(self.snapshot_bytes()).await.map_err(backend)?;
            }
            Ok(())
        })
    }

    fn active(&self, now: u64) -> BoxFuture<'_, Result<Vec<ActiveVisitor>, StoreError>> {
        self.inner.active(now) // a pure read (prunes in memory only)
    }

    fn count(&self, now: u64) -> BoxFuture<'_, Result<usize, StoreError>> {
        self.inner.count(now) // a pure read
    }
}

/// Replay the snapshot then the logged records, in order, into the map.
fn replay(inner: &InMemoryActivity, loaded: &Loaded) {
    if let Some(snapshot) = &loaded.snapshot {
        for rec in framed(snapshot) {
            apply(inner, rec);
        }
    }
    for rec in &loaded.records {
        apply(inner, rec);
    }
}

fn apply(inner: &InMemoryActivity, rec: &[u8]) {
    if let Some(visitor) = decode_visitor(rec) {
        let _ = ready(inner.record(visitor));
    }
}

/// Resolve an in-memory store future that is ready on the first poll.
fn ready<T>(fut: BoxFuture<'_, T>) -> T {
    use std::task::{Context, Poll, Waker};
    let mut fut = fut;
    let waker = Waker::noop();
    match fut.as_mut().poll(&mut Context::from_waker(waker)) {
        Poll::Ready(value) => value,
        Poll::Pending => unreachable!("the in-memory activity store resolves synchronously"),
    }
}

fn backend(e: PersistError) -> StoreError {
    StoreError::Backend(e.0)
}

/// Iterate the length-prefixed records packed into a snapshot blob.
fn framed(blob: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut pos = 0;
    std::iter::from_fn(move || {
        if pos + 4 > blob.len() {
            return None;
        }
        let len = u32::from_le_bytes(blob[pos..pos + 4].try_into().unwrap()) as usize;
        let start = pos + 4;
        let end = start.checked_add(len).filter(|&e| e <= blob.len())?;
        pos = end;
        Some(&blob[start..end])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::net::IpAddr;
    use std::path::PathBuf;
    use std::task::{Context, Poll, Waker};

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
        let dir = std::env::temp_dir().join(format!("kernway-fileact-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn visitor(id: &str, path: &str, last_seen: u64) -> ActiveVisitor {
        ActiveVisitor {
            id: id.to_string(),
            authenticated: !id.starts_with("anon-"),
            ip: "10.0.0.1".parse::<IpAddr>().ok(),
            user_agent: Some("test/1.0".to_string()),
            path: path.to_string(),
            method: "GET".to_string(),
            last_seen,
        }
    }

    #[test]
    fn the_live_map_survives_a_restart() {
        let dir = temp_dir("restart");
        {
            let a = FileBackedActivity::open(&dir, Duration::from_secs(300), Fsync::EveryWrite).unwrap();
            block_on(a.record(visitor("alice", "/dashboard", 1000))).unwrap();
            block_on(a.record(visitor("anon-9", "/pricing", 1005))).unwrap();
            block_on(a.record(visitor("alice", "/settings", 1010))).unwrap(); // she moved
        }
        let a = FileBackedActivity::open(&dir, Duration::from_secs(300), Fsync::EveryWrite).unwrap();
        let live = block_on(a.active(1020)).unwrap();
        assert_eq!(live.len(), 2, "two identities recovered: {live:?}");
        let alice = live.iter().find(|v| v.id == "alice").unwrap();
        assert_eq!(alice.path, "/settings", "her latest page survived");
        assert_eq!(alice.ip, "10.0.0.1".parse::<IpAddr>().ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkpoint_folds_the_log_and_still_recovers() {
        let dir = temp_dir("checkpoint");
        {
            let a = FileBackedActivity::open(&dir, Duration::from_secs(3600), Fsync::Batched(Duration::from_secs(3600))).unwrap();
            // One visitor recorded many times drives past CHECKPOINT_EVERY.
            for t in 0..(CHECKPOINT_EVERY + 5) {
                block_on(a.record(visitor("alice", "/p", 1000 + t as u64))).unwrap();
            }
        }
        let a = FileBackedActivity::open(&dir, Duration::from_secs(3600), Fsync::EveryWrite).unwrap();
        let live = block_on(a.active(u64::from(CHECKPOINT_EVERY as u32) + 2000)).unwrap();
        assert_eq!(live.len(), 1, "still one row after folding the log");
        assert_eq!(live[0].last_seen, 1000 + (CHECKPOINT_EVERY + 4) as u64, "latest record stands");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
