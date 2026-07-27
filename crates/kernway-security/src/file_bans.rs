//! File-backed persistence for the ban list (feature = `persist`).
//!
//! The per-request ban check stays in memory ([`Bans`]) — reads never touch disk. Each
//! ban/unban is also appended to a local write-ahead log, and the whole list is
//! snapshotted periodically, so a restart replays the log onto the snapshot and loses
//! nothing. Unlike [`RedisBanStore`](crate::redis_bans), this needs **no server**: the
//! files live next to the app. The trade is that it is single-node — each process owns
//! its own files, so bans are not shared across instances (that is Redis's job).
//!
//! Ban/unban are rare (admin actions), so the default [`Fsync::EveryWrite`] costs
//! effectively nothing while giving zero-loss durability. The log lives in the given
//! directory as `wal`, the snapshot as `snapshot` (see [`kernway_persist`]).

use std::io;
use std::net::IpAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use kernway_persist::{Fsync, Loaded, PersistError, Persister};

use crate::tracking::{BanRuleView, Bans};

// One tag byte per record; the payload is the rest of the record (UTF-8). The ban/
// unban tags are what a mutation logs; a snapshot logs only the "ban" tags (the
// current rules), never an unban.
const TAG_BAN_IP: u8 = 1;
const TAG_UNBAN_IP: u8 = 2;
const TAG_BAN_SUBNET: u8 = 3;
const TAG_UNBAN_SUBNET: u8 = 4;
const TAG_BAN_UA_CONTAINS: u8 = 5;
const TAG_UNBAN_UA_CONTAINS: u8 = 6;
const TAG_BAN_UA_EXACT: u8 = 7;

/// Snapshot once this many mutations have been logged, to bound the log and recovery
/// time. Bans churn slowly, so this is rarely reached in practice.
const CHECKPOINT_EVERY: usize = 256;

/// A ban list whose in-memory state is mirrored to a local snapshot + append log.
/// Hand [`bans`](Self::bans) to the `BanFilter`, register the store as a bean, and an
/// admin handler bans/unbans through it — each call updates memory *and* disk, so the
/// live check is instant and the ban outlives a restart, with no Redis.
///
/// ```rust,ignore
/// let store = FileBackedBans::open("data/bans", Fsync::EveryWrite)?;
/// app.layer(BanFilter::new(store.bans())).register(store.clone());
/// // in an admin handler: store.ban_ip(addr).await?;
/// ```
#[derive(Clone)]
pub struct FileBackedBans {
    bans: Bans,
    persist: Arc<Persister>,
    since_checkpoint: Arc<AtomicUsize>,
}

impl FileBackedBans {
    /// Open (creating if needed) the store in `dir`, replaying any prior bans into
    /// memory. `fsync` sets the durability/speed trade — [`Fsync::EveryWrite`] for
    /// zero loss (the right default for a ban list).
    pub fn open(dir: impl AsRef<Path>, fsync: Fsync) -> io::Result<Self> {
        let (persist, loaded) = Persister::open(dir, fsync)?;
        let bans = Bans::new();
        replay(&bans, &loaded);
        Ok(Self {
            bans,
            persist: Arc::new(persist),
            since_checkpoint: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// The in-memory list to hand to `BanFilter` for the per-request check.
    #[must_use]
    pub fn bans(&self) -> Bans {
        self.bans.clone()
    }

    /// Ban an IP in memory and on disk.
    pub async fn ban_ip(&self, ip: IpAddr) -> Result<(), PersistError> {
        self.bans.ban_ip(ip);
        self.log(TAG_BAN_IP, &ip.to_string()).await
    }

    /// Unban an IP in memory and on disk.
    pub async fn unban_ip(&self, ip: IpAddr) -> Result<(), PersistError> {
        self.bans.unban_ip(ip);
        self.log(TAG_UNBAN_IP, &ip.to_string()).await
    }

    /// Ban a subnet in memory and on disk.
    pub async fn ban_subnet(&self, cidr: &str) -> Result<(), PersistError> {
        self.bans.ban_subnet(cidr);
        self.log(TAG_BAN_SUBNET, cidr).await
    }

    /// Unban a subnet in memory and on disk.
    pub async fn unban_subnet(&self, cidr: &str) -> Result<(), PersistError> {
        self.bans.unban_subnet(cidr);
        self.log(TAG_UNBAN_SUBNET, cidr).await
    }

    /// Ban a User-Agent phrase in memory and on disk.
    pub async fn ban_user_agent_containing(&self, phrase: &str) -> Result<(), PersistError> {
        self.bans.ban_user_agent_containing(phrase);
        // Persist the lowercased form, matching how the in-memory rule is stored.
        self.log(TAG_BAN_UA_CONTAINS, &phrase.to_ascii_lowercase())
            .await
    }

    /// Unban a User-Agent phrase in memory and on disk.
    pub async fn unban_user_agent_containing(&self, phrase: &str) -> Result<(), PersistError> {
        self.bans.unban_user_agent_containing(phrase);
        self.log(TAG_UNBAN_UA_CONTAINS, &phrase.to_ascii_lowercase())
            .await
    }

    /// Append a mutation, then checkpoint if enough have accrued.
    async fn log(&self, tag: u8, payload: &str) -> Result<(), PersistError> {
        self.persist.append(record(tag, payload)).await?;
        if self.since_checkpoint.fetch_add(1, Ordering::Relaxed) + 1 >= CHECKPOINT_EVERY {
            self.since_checkpoint.store(0, Ordering::Relaxed);
            self.persist.checkpoint(self.snapshot_bytes()).await?;
        }
        Ok(())
    }

    /// The whole current list, framed as length-prefixed records for the snapshot.
    fn snapshot_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for view in self.bans.rules() {
            let rec = view_record(&view);
            out.extend_from_slice(&(rec.len() as u32).to_le_bytes());
            out.extend_from_slice(&rec);
        }
        out
    }
}

/// Rebuild the in-memory list from the snapshot then the logged mutations, in order.
fn replay(bans: &Bans, loaded: &Loaded) {
    if let Some(snapshot) = &loaded.snapshot {
        for rec in framed(snapshot) {
            apply(bans, rec);
        }
    }
    for rec in &loaded.records {
        apply(bans, rec);
    }
}

/// Apply one `[tag][payload]` record to the in-memory list.
fn apply(bans: &Bans, rec: &[u8]) {
    let Some((&tag, payload)) = rec.split_first() else {
        return;
    };
    let Ok(value) = std::str::from_utf8(payload) else {
        return;
    };
    match tag {
        TAG_BAN_IP => {
            if let Ok(ip) = value.parse() {
                bans.ban_ip(ip);
            }
        }
        TAG_UNBAN_IP => {
            if let Ok(ip) = value.parse() {
                bans.unban_ip(ip);
            }
        }
        TAG_BAN_SUBNET => bans.ban_subnet(value),
        TAG_UNBAN_SUBNET => bans.unban_subnet(value),
        TAG_BAN_UA_CONTAINS => bans.ban_user_agent_containing(value),
        TAG_UNBAN_UA_CONTAINS => bans.unban_user_agent_containing(value),
        TAG_BAN_UA_EXACT => bans.ban_user_agent_exact(value),
        _ => {} // an unknown tag from a newer version — skip, don't corrupt state
    }
}

fn record(tag: u8, payload: &str) -> Vec<u8> {
    let mut rec = Vec::with_capacity(1 + payload.len());
    rec.push(tag);
    rec.extend_from_slice(payload.as_bytes());
    rec
}

fn view_record(view: &BanRuleView) -> Vec<u8> {
    match view {
        BanRuleView::Ip(ip) => record(TAG_BAN_IP, &ip.to_string()),
        BanRuleView::Subnet(cidr) => record(TAG_BAN_SUBNET, cidr),
        BanRuleView::UserAgentExact(ua) => record(TAG_BAN_UA_EXACT, ua),
        BanRuleView::UserAgentContains(phrase) => record(TAG_BAN_UA_CONTAINS, phrase),
    }
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

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kernway-filebans-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn bans_survive_a_restart_on_disk() {
        let dir = temp_dir("restart");
        {
            let store = FileBackedBans::open(&dir, Fsync::EveryWrite).unwrap();
            block_on(store.ban_ip(ip("203.0.113.7"))).unwrap();
            block_on(store.ban_subnet("198.51.100.0/24")).unwrap();
            block_on(store.ban_user_agent_containing("EvilBot")).unwrap();
            assert!(store.bans().is_banned(Some(ip("203.0.113.7")), None));
        } // drop flushes + joins the writer — a real "restart".

        let store = FileBackedBans::open(&dir, Fsync::EveryWrite).unwrap();
        let bans = store.bans();
        assert!(
            bans.is_banned(Some(ip("203.0.113.7")), None),
            "IP ban recovered"
        );
        assert!(
            bans.is_banned(Some(ip("198.51.100.42")), None),
            "subnet ban recovered"
        );
        assert!(
            bans.is_banned(None, Some("has EVILBOT in it")),
            "UA ban recovered (case-insensitive)"
        );
        assert!(
            !bans.is_banned(Some(ip("8.8.8.8")), Some("Mozilla/5.0")),
            "a clean request is fine"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unban_survives_a_restart() {
        let dir = temp_dir("unban");
        {
            let store = FileBackedBans::open(&dir, Fsync::EveryWrite).unwrap();
            block_on(store.ban_ip(ip("10.0.0.1"))).unwrap();
            block_on(store.ban_ip(ip("10.0.0.2"))).unwrap();
            block_on(store.unban_ip(ip("10.0.0.1"))).unwrap();
        }
        let store = FileBackedBans::open(&dir, Fsync::EveryWrite).unwrap();
        assert!(
            !store.bans().is_banned(Some(ip("10.0.0.1")), None),
            "the unban was replayed"
        );
        assert!(
            store.bans().is_banned(Some(ip("10.0.0.2")), None),
            "the other ban stands"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_checkpoint_folds_the_log_and_still_recovers() {
        let dir = temp_dir("checkpoint");
        {
            let store = FileBackedBans::open(&dir, Fsync::EveryWrite).unwrap();
            // Drive well past CHECKPOINT_EVERY so at least one snapshot is written,
            // then leave a couple of post-checkpoint records in the log.
            for n in 0..(CHECKPOINT_EVERY + 3) {
                block_on(store.ban_ip(ip(&format!("10.1.{}.{}", n / 256, n % 256)))).unwrap();
            }
        }
        let store = FileBackedBans::open(&dir, Fsync::EveryWrite).unwrap();
        let bans = store.bans();
        assert!(
            bans.is_banned(Some(ip("10.1.0.0")), None),
            "an early ban (from the snapshot)"
        );
        let last = CHECKPOINT_EVERY + 2;
        assert!(
            bans.is_banned(
                Some(ip(&format!("10.1.{}.{}", last / 256, last % 256))),
                None
            ),
            "a late ban (from the post-checkpoint log)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
