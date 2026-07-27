//! File-backed [`SessionStore`](crate::session::SessionStore) (feature = `persist`).
//!
//! The registry stays in memory (per-request `get` is an `RwLock` read, no disk), but
//! every write — login, touch, logout — is appended to a local log, and the whole
//! registry is snapshotted periodically. A restart replays the log onto the snapshot,
//! so sessions survive it with **no Redis**. Single-node: each process owns its files.
//!
//! Unlike the ban list, sessions write on the hot path — `touch` runs on every
//! authenticated request. Under [`Fsync::EveryWrite`] that is an fsync per such
//! request; for a busy app prefer [`Fsync::Batched`] (Redis's `everysec` trade). Login
//! and logout surface a disk error (a login that is not durable should fail loudly);
//! `touch` is best-effort — a transient disk hiccup must not log a user out.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use kernway_core::layer::BoxFuture;
use kernway_persist::{Fsync, Loaded, PersistError, Persister};

use crate::session::{MemorySessionStore, SessionRecord, SessionStore, StoreError};

const TAG_INSERT: u8 = 1;
const TAG_TOUCH: u8 = 2;
const TAG_REMOVE: u8 = 3;
const TAG_REMOVE_USER: u8 = 4;

/// Snapshot once this many writes have been logged. Sessions touch on every
/// authenticated request, so this bounds the log under steady traffic.
const CHECKPOINT_EVERY: usize = 2000;

/// A [`SessionStore`] whose in-memory registry is mirrored to a local snapshot + log.
/// Drop-in for `MemorySessionStore` in a `SessionManager`, but durable across a
/// restart without a server.
///
/// ```rust,ignore
/// let store = FileBackedSessionStore::open("data/sessions", Fsync::Batched(Duration::from_secs(1)))?;
/// let manager = SessionManager::new(Box::new(store), key, SessionConfig::default());
/// ```
pub struct FileBackedSessionStore {
    inner: MemorySessionStore,
    persist: Arc<Persister>,
    since_checkpoint: Arc<AtomicUsize>,
}

impl FileBackedSessionStore {
    /// Open (creating if needed) the store in `dir`, replaying prior sessions into
    /// memory.
    pub fn open(dir: impl AsRef<Path>, fsync: Fsync) -> io::Result<Self> {
        let (persist, loaded) = Persister::open(dir, fsync)?;
        let inner = MemorySessionStore::new();
        replay(&inner, &loaded);
        Ok(Self {
            inner,
            persist: Arc::new(persist),
            since_checkpoint: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// The full registry, framed as length-prefixed INSERT records for a snapshot.
    fn snapshot_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (sid, record) in self.inner.snapshot() {
            let rec = insert_record(&sid, &record);
            out.extend_from_slice(&(rec.len() as u32).to_le_bytes());
            out.extend_from_slice(&rec);
        }
        out
    }

    /// Append a write, then checkpoint if enough have accrued.
    async fn log(&self, record: Vec<u8>) -> Result<(), StoreError> {
        self.persist.append(record).await.map_err(backend)?;
        if self.since_checkpoint.fetch_add(1, Ordering::Relaxed) + 1 >= CHECKPOINT_EVERY {
            self.since_checkpoint.store(0, Ordering::Relaxed);
            self.persist
                .checkpoint(self.snapshot_bytes())
                .await
                .map_err(backend)?;
        }
        Ok(())
    }
}

impl SessionStore for FileBackedSessionStore {
    fn insert(&self, sid: &str, record: SessionRecord) -> BoxFuture<'_, Result<(), StoreError>> {
        let rec = insert_record(sid, &record);
        let sid = sid.to_string();
        Box::pin(async move {
            self.inner.insert(&sid, record).await?;
            self.log(rec).await // a non-durable login fails loudly
        })
    }

    fn get(&self, sid: &str) -> BoxFuture<'_, Result<Option<SessionRecord>, StoreError>> {
        self.inner.get(sid) // a pure read — in memory, no disk
    }

    fn touch(&self, sid: &str, at: u64) -> BoxFuture<'_, Result<(), StoreError>> {
        let rec = touch_record(sid, at);
        let sid = sid.to_string();
        Box::pin(async move {
            self.inner.touch(&sid, at).await?;
            // Best-effort: a disk hiccup on an idle-timeout advance must not fail the
            // request (the in-memory last_seen is already updated).
            let _ = self.log(rec).await;
            Ok(())
        })
    }

    fn remove(&self, sid: &str) -> BoxFuture<'_, Result<(), StoreError>> {
        let rec = record(TAG_REMOVE, |out| put_str(out, sid));
        let sid = sid.to_string();
        Box::pin(async move {
            self.inner.remove(&sid).await?;
            self.log(rec).await
        })
    }

    fn remove_user(&self, user: &str) -> BoxFuture<'_, Result<(), StoreError>> {
        let rec = record(TAG_REMOVE_USER, |out| put_str(out, user));
        let user = user.to_string();
        Box::pin(async move {
            self.inner.remove_user(&user).await?;
            self.log(rec).await
        })
    }

    fn sessions_of(
        &self,
        user: &str,
    ) -> BoxFuture<'_, Result<Vec<(String, SessionRecord)>, StoreError>> {
        self.inner.sessions_of(user) // a pure read
    }

    fn len(&self) -> BoxFuture<'_, Result<usize, StoreError>> {
        self.inner.len() // a pure read
    }
}

/// Replay the snapshot then the logged writes, in order, into the registry. The
/// futures are in-memory (ready on the first poll), so a trivial poll drives them.
fn replay(inner: &MemorySessionStore, loaded: &Loaded) {
    if let Some(snapshot) = &loaded.snapshot {
        for rec in framed(snapshot) {
            apply(inner, rec);
        }
    }
    for rec in &loaded.records {
        apply(inner, rec);
    }
}

fn apply(inner: &MemorySessionStore, rec: &[u8]) {
    let Some((&tag, mut payload)) = rec.split_first() else {
        return;
    };
    // The in-memory store is infallible, so the replayed op's Ok is discarded.
    match tag {
        TAG_INSERT => {
            if let Some((sid, record)) = decode_insert(&mut payload) {
                let _ = ready(inner.insert(&sid, record));
            }
        }
        TAG_TOUCH => {
            if let (Some(sid), Some(at)) = (take_str(&mut payload), take_u64(&mut payload)) {
                let _ = ready(inner.touch(&sid, at));
            }
        }
        TAG_REMOVE => {
            if let Some(sid) = take_str(&mut payload) {
                let _ = ready(inner.remove(&sid));
            }
        }
        TAG_REMOVE_USER => {
            if let Some(user) = take_str(&mut payload) {
                let _ = ready(inner.remove_user(&user));
            }
        }
        _ => {}
    }
}

/// Resolve an in-memory store future that is ready on the first poll.
fn ready<T>(fut: BoxFuture<'_, T>) -> T {
    use std::task::{Context, Poll, Waker};
    let mut fut = fut;
    let waker = Waker::noop();
    match fut.as_mut().poll(&mut Context::from_waker(waker)) {
        Poll::Ready(value) => value,
        Poll::Pending => unreachable!("the in-memory session store resolves synchronously"),
    }
}

fn backend(e: PersistError) -> StoreError {
    StoreError::Backend(e.0)
}

// ── record encoding (length-prefixed, serde-free — the session store's own style) ──

fn insert_record(sid: &str, r: &SessionRecord) -> Vec<u8> {
    record(TAG_INSERT, |out| {
        put_str(out, sid);
        put_str(out, &r.user);
        out.extend_from_slice(&r.created.to_le_bytes());
        out.extend_from_slice(&r.last_seen.to_le_bytes());
        put_str(out, &r.meta);
    })
}

fn touch_record(sid: &str, at: u64) -> Vec<u8> {
    record(TAG_TOUCH, |out| {
        put_str(out, sid);
        out.extend_from_slice(&at.to_le_bytes());
    })
}

fn decode_insert(b: &mut &[u8]) -> Option<(String, SessionRecord)> {
    let sid = take_str(b)?;
    let user = take_str(b)?;
    let created = take_u64(b)?;
    let last_seen = take_u64(b)?;
    let meta = take_str(b)?;
    Some((
        sid,
        SessionRecord {
            user,
            created,
            last_seen,
            meta,
        },
    ))
}

fn record(tag: u8, fill: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut out = vec![tag];
    fill(&mut out);
    out
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn take_str(b: &mut &[u8]) -> Option<String> {
    let len = take_u32(b)? as usize;
    if b.len() < len {
        return None;
    }
    let (head, tail) = b.split_at(len);
    *b = tail;
    String::from_utf8(head.to_vec()).ok()
}

fn take_u32(b: &mut &[u8]) -> Option<u32> {
    if b.len() < 4 {
        return None;
    }
    let (head, tail) = b.split_at(4);
    *b = tail;
    Some(u32::from_le_bytes(head.try_into().unwrap()))
}

fn take_u64(b: &mut &[u8]) -> Option<u64> {
    if b.len() < 8 {
        return None;
    }
    let (head, tail) = b.split_at(8);
    *b = tail;
    Some(u64::from_le_bytes(head.try_into().unwrap()))
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
        let dir = std::env::temp_dir().join(format!("kernway-filesess-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn rec(user: &str, meta: &str) -> SessionRecord {
        SessionRecord {
            user: user.to_string(),
            created: 1000,
            last_seen: 1000,
            meta: meta.to_string(),
        }
    }

    #[test]
    fn sessions_and_touches_survive_a_restart() {
        let dir = temp_dir("restart");
        {
            let s = FileBackedSessionStore::open(&dir, Fsync::EveryWrite).unwrap();
            block_on(s.insert("sid-a", rec("alice", "laptop"))).unwrap();
            block_on(s.insert("sid-b", rec("bob", "phone"))).unwrap();
            block_on(s.touch("sid-a", 1234)).unwrap();
            block_on(s.remove("sid-b")).unwrap(); // bob logs out
        }
        let s = FileBackedSessionStore::open(&dir, Fsync::EveryWrite).unwrap();
        let a = block_on(s.get("sid-a"))
            .unwrap()
            .expect("alice's session recovered");
        assert_eq!(a.user, "alice");
        assert_eq!(a.last_seen, 1234, "the touch was replayed");
        assert!(
            block_on(s.get("sid-b")).unwrap().is_none(),
            "bob's logout was replayed"
        );
        assert_eq!(block_on(s.len()).unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn logout_everywhere_survives_a_restart() {
        let dir = temp_dir("logout-all");
        {
            let s = FileBackedSessionStore::open(&dir, Fsync::EveryWrite).unwrap();
            block_on(s.insert("s1", rec("carol", "phone"))).unwrap();
            block_on(s.insert("s2", rec("carol", "laptop"))).unwrap();
            block_on(s.remove_user("carol")).unwrap();
        }
        let s = FileBackedSessionStore::open(&dir, Fsync::EveryWrite).unwrap();
        assert_eq!(
            block_on(s.sessions_of("carol")).unwrap().len(),
            0,
            "both sessions revoked"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkpoint_folds_the_log_and_still_recovers() {
        let dir = temp_dir("checkpoint");
        {
            let s = FileBackedSessionStore::open(
                &dir,
                Fsync::Batched(std::time::Duration::from_secs(3600)),
            )
            .unwrap();
            // Many touches to one session drives past CHECKPOINT_EVERY (a snapshot),
            // leaving a few post-checkpoint records.
            block_on(s.insert("sid", rec("dave", "tablet"))).unwrap();
            for t in 0..(CHECKPOINT_EVERY + 5) {
                block_on(s.touch("sid", 2000 + t as u64)).unwrap();
            }
        }
        let s = FileBackedSessionStore::open(&dir, Fsync::EveryWrite).unwrap();
        let d = block_on(s.get("sid"))
            .unwrap()
            .expect("session recovered across a checkpoint");
        assert_eq!(
            d.last_seen,
            2000 + (CHECKPOINT_EVERY + 4) as u64,
            "the latest touch stands"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
