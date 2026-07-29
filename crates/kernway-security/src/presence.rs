//! Presence — *who is actually online right now*, distinct from *who has a valid
//! session*.
//!
//! A session lives for hours; it says a user *may* return, not that they are here.
//! Online is a liveness signal on a much shorter clock: "beat within the last N
//! seconds". So presence is its own concern, not a field on the session — the
//! client sends a periodic **heartbeat** (an htmx poll, an SSE keepalive, a
//! WebSocket ping), and a user counts as online while their last beat is inside the
//! window. Miss a couple of beats and they fall out automatically — no explicit
//! "went offline" event.
//!
//! Two backends, same shape as the session store: [`InMemoryPresence`] for one
//! instance, and [`RedisPresence`] (feature = `redis`) — a Redis sorted set scored
//! by last-beat timestamp — shared across instances. Errors reuse the session
//! [`StoreError`](crate::session::StoreError).

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Duration;

use kernway_core::layer::BoxFuture;

use crate::session::StoreError;

/// Tracks liveness: a heartbeat marks a user present, and `online`/`is_online`/
/// `count` report who beat within the window. `now` is unix seconds, passed in so
/// the caller controls the clock (and tests can too).
pub trait Presence: Send + Sync {
    /// Record that `user` is alive as of `now`.
    fn heartbeat(&self, user: &str, now: u64) -> BoxFuture<'_, Result<(), StoreError>>;
    /// The users whose last beat is within the window of `now`, sorted.
    fn online(&self, now: u64) -> BoxFuture<'_, Result<Vec<String>, StoreError>>;
    /// Whether `user` has beaten within the window of `now`.
    fn is_online(&self, user: &str, now: u64) -> BoxFuture<'_, Result<bool, StoreError>>;
    /// How many users are online within the window of `now`.
    fn count(&self, now: u64) -> BoxFuture<'_, Result<usize, StoreError>>;
}

/// In-memory presence: `user -> last-beat`. For a single instance; a heartbeat is a
/// map write, and reads prune anything past the window as they go.
pub struct InMemoryPresence {
    window_secs: u64,
    beats: RwLock<HashMap<String, u64>>,
}

impl InMemoryPresence {
    /// A tracker whose window is `window` — a user is online for that long after
    /// their last beat.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window_secs: window.as_secs(),
            beats: RwLock::new(HashMap::new()),
        }
    }

    /// The oldest beat that still counts as online at `now`.
    fn since(&self, now: u64) -> u64 {
        now.saturating_sub(self.window_secs)
    }
}

impl Presence for InMemoryPresence {
    fn heartbeat(&self, user: &str, now: u64) -> BoxFuture<'_, Result<(), StoreError>> {
        let user = user.to_string();
        Box::pin(async move {
            self.beats.write().unwrap().insert(user, now);
            Ok(())
        })
    }

    fn online(&self, now: u64) -> BoxFuture<'_, Result<Vec<String>, StoreError>> {
        Box::pin(async move {
            let since = self.since(now);
            let mut beats = self.beats.write().unwrap();
            beats.retain(|_, last| *last >= since); // prune stale as we read
            let mut users: Vec<String> = beats.keys().cloned().collect();
            users.sort();
            Ok(users)
        })
    }

    fn is_online(&self, user: &str, now: u64) -> BoxFuture<'_, Result<bool, StoreError>> {
        let user = user.to_string();
        Box::pin(async move {
            let since = self.since(now);
            Ok(self
                .beats
                .read()
                .unwrap()
                .get(&user)
                .is_some_and(|last| *last >= since))
        })
    }

    fn count(&self, now: u64) -> BoxFuture<'_, Result<usize, StoreError>> {
        Box::pin(async move {
            let since = self.since(now);
            Ok(self
                .beats
                .read()
                .unwrap()
                .values()
                .filter(|last| **last >= since)
                .count())
        })
    }
}

/// One online user and how many devices they are on. Yielded by
/// [`UserPresence::online`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OnlineUser {
    /// The user identity (e.g. an email).
    pub user: String,
    /// How many of their devices have beaten within the window.
    pub devices: usize,
}

/// One live device — a single (user, device) that beat within the window, with the
/// caller-defined `meta` recorded at its last heartbeat. Yielded by
/// [`UserPresence::sessions`], the flat per-device view (e.g. for an admin device list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSession {
    /// The owning user (e.g. an email).
    pub user: String,
    /// The stable per-device id.
    pub device: String,
    /// Whatever the caller passed to [`heartbeat`](UserPresence::heartbeat) — opaque to
    /// the tracker (e.g. a JSON blob of the client IP and user-agent).
    pub meta: String,
    /// Unix seconds of this device's last beat.
    pub last_seen: u64,
}

/// Presence tracked **per user, across devices** — the two-level model that flat
/// [`Presence`] cannot express.
///
/// A person signed in on several devices is *one* online user. Each device is kept
/// alive by its own heartbeats (keyed by a stable per-device id), and the user counts
/// as online while *any* device has beaten within the window. Losing one device — its
/// beats age out, or an explicit [`forget`](UserPresence::forget) on logout — leaves
/// the user online until their last device is gone. So [`count`](UserPresence::count)
/// is a count of *people*, not connections.
///
/// Reach for this over [`Presence`] when "online" is about people but you also need to
/// know they are on N devices (multi-device login), or to sign one device out without
/// touching the others. `now` is unix seconds, passed in so the caller owns the clock.
pub trait UserPresence: Send + Sync {
    /// Record that `user` is alive on `device` as of `now`, stamping the device with
    /// caller-defined `meta` (opaque here — e.g. IP + user-agent for an admin view).
    fn heartbeat(&self, user: &str, device: &str, meta: &str, now: u64) -> BoxFuture<'_, Result<(), StoreError>>;
    /// Drop one `device` for `user` (an explicit logout), and the user too if it was
    /// their last — so a logout shows immediately, not after the window.
    fn forget(&self, user: &str, device: &str) -> BoxFuture<'_, Result<(), StoreError>>;
    /// How many distinct users are online within the window of `now`.
    fn count(&self, now: u64) -> BoxFuture<'_, Result<usize, StoreError>>;
    /// How many of `user`'s devices are live within the window of `now`.
    fn devices(&self, user: &str, now: u64) -> BoxFuture<'_, Result<usize, StoreError>>;
    /// Whether `user` has any device that beat within the window of `now`.
    fn is_online(&self, user: &str, now: u64) -> BoxFuture<'_, Result<bool, StoreError>>;
    /// Every online user with their live device count, sorted by user.
    fn online(&self, now: u64) -> BoxFuture<'_, Result<Vec<OnlineUser>, StoreError>>;
    /// Every live device as a flat list (with its `meta`), sorted by user then device —
    /// the per-device view an admin lists to act on individual sessions.
    fn sessions(&self, now: u64) -> BoxFuture<'_, Result<Vec<DeviceSession>, StoreError>>;
}

/// In-memory per-user/device presence: `user -> (device -> (last-beat, meta))`. For a
/// single instance; heartbeats are map writes, and reads prune anything past the window
/// (empty users included) as they go.
pub struct InMemoryUserPresence {
    window_secs: u64,
    users: RwLock<HashMap<String, HashMap<String, (u64, String)>>>,
}

impl InMemoryUserPresence {
    /// A tracker whose window is `window` — a device is online for that long after its
    /// last beat, and a user for as long as any of their devices is.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self { window_secs: window.as_secs(), users: RwLock::new(HashMap::new()) }
    }

    fn since(&self, now: u64) -> u64 {
        now.saturating_sub(self.window_secs)
    }

    /// Prune devices (then now-empty users) whose last beat fell outside the window.
    fn prune(&self, now: u64) {
        let since = self.since(now);
        let mut users = self.users.write().unwrap();
        users.values_mut().for_each(|devices| devices.retain(|_, (last, _)| *last >= since));
        users.retain(|_, devices| !devices.is_empty());
    }
}

impl UserPresence for InMemoryUserPresence {
    fn heartbeat(&self, user: &str, device: &str, meta: &str, now: u64) -> BoxFuture<'_, Result<(), StoreError>> {
        let (user, device, meta) = (user.to_string(), device.to_string(), meta.to_string());
        Box::pin(async move {
            self.users.write().unwrap().entry(user).or_default().insert(device, (now, meta));
            Ok(())
        })
    }

    fn forget(&self, user: &str, device: &str) -> BoxFuture<'_, Result<(), StoreError>> {
        let (user, device) = (user.to_string(), device.to_string());
        Box::pin(async move {
            let mut users = self.users.write().unwrap();
            if let Some(devices) = users.get_mut(&user) {
                devices.remove(&device);
                if devices.is_empty() {
                    users.remove(&user);
                }
            }
            Ok(())
        })
    }

    fn count(&self, now: u64) -> BoxFuture<'_, Result<usize, StoreError>> {
        Box::pin(async move {
            self.prune(now);
            Ok(self.users.read().unwrap().len())
        })
    }

    fn devices(&self, user: &str, now: u64) -> BoxFuture<'_, Result<usize, StoreError>> {
        let user = user.to_string();
        Box::pin(async move {
            self.prune(now);
            Ok(self.users.read().unwrap().get(&user).map_or(0, HashMap::len))
        })
    }

    fn is_online(&self, user: &str, now: u64) -> BoxFuture<'_, Result<bool, StoreError>> {
        let user = user.to_string();
        Box::pin(async move {
            self.prune(now);
            Ok(self.users.read().unwrap().contains_key(&user))
        })
    }

    fn online(&self, now: u64) -> BoxFuture<'_, Result<Vec<OnlineUser>, StoreError>> {
        Box::pin(async move {
            self.prune(now);
            let mut list: Vec<OnlineUser> = self
                .users
                .read()
                .unwrap()
                .iter()
                .map(|(user, devices)| OnlineUser { user: user.clone(), devices: devices.len() })
                .collect();
            list.sort_by(|a, b| a.user.cmp(&b.user));
            Ok(list)
        })
    }

    fn sessions(&self, now: u64) -> BoxFuture<'_, Result<Vec<DeviceSession>, StoreError>> {
        Box::pin(async move {
            self.prune(now);
            let mut list: Vec<DeviceSession> = self
                .users
                .read()
                .unwrap()
                .iter()
                .flat_map(|(user, devices)| {
                    devices.iter().map(move |(device, (last, meta))| DeviceSession {
                        user: user.clone(),
                        device: device.clone(),
                        meta: meta.clone(),
                        last_seen: *last,
                    })
                })
                .collect();
            list.sort_by(|a, b| a.user.cmp(&b.user).then_with(|| a.device.cmp(&b.device)));
            Ok(list)
        })
    }
}

/// Redis-backed presence (feature = `redis`): a sorted set scored by last-beat
/// timestamp, shared across instances.
///
/// - heartbeat → `ZADD kw:presence now user`
/// - online → prune with `ZREMRANGEBYSCORE`, then `ZRANGEBYSCORE (now-window) +inf`
/// - is_online → `ZSCORE`, count → `ZCOUNT`
#[cfg(feature = "redis")]
pub use redis_impl::RedisPresence;

#[cfg(feature = "redis")]
mod redis_impl {
    use std::net::SocketAddr;
    use std::time::Duration;

    use kernway_core::layer::BoxFuture;
    use kernway_redis::{Pool, RedisError};

    use super::Presence;
    use crate::session::StoreError;

    /// The sorted set holding every user's last-beat timestamp.
    const PRESENCE_KEY: &str = "kw:presence";

    fn to_store(err: RedisError) -> StoreError {
        StoreError::Backend(err.to_string())
    }

    /// Presence over a Redis sorted set.
    pub struct RedisPresence {
        pool: Pool,
        window_secs: u64,
    }

    impl RedisPresence {
        /// Connect to `addr`, with a `window`-long online window.
        #[must_use]
        pub fn new(addr: SocketAddr, window: Duration) -> Self {
            Self {
                pool: Pool::new(addr),
                window_secs: window.as_secs(),
            }
        }

        /// Use a pre-configured [`Pool`] (e.g. one carrying `AUTH`).
        #[must_use]
        pub fn from_pool(pool: Pool, window: Duration) -> Self {
            Self {
                pool,
                window_secs: window.as_secs(),
            }
        }

        fn since(&self, now: u64) -> u64 {
            now.saturating_sub(self.window_secs)
        }
    }

    impl Presence for RedisPresence {
        fn heartbeat(&self, user: &str, now: u64) -> BoxFuture<'_, Result<(), StoreError>> {
            let user = user.to_string();
            Box::pin(async move {
                self.pool
                    .with(async |c| c.zadd(PRESENCE_KEY, now as i64, &user).await)
                    .await
                    .map_err(to_store)
            })
        }

        fn online(&self, now: u64) -> BoxFuture<'_, Result<Vec<String>, StoreError>> {
            let since = self.since(now);
            Box::pin(async move {
                self.pool
                    .with(async |c| {
                        // Drop everything strictly older than the window, then read
                        // what remains.
                        c.zremrangebyscore(PRESENCE_KEY, "-inf", &format!("({since}"))
                            .await?;
                        let mut users = c
                            .zrangebyscore(PRESENCE_KEY, &since.to_string(), "+inf")
                            .await?;
                        users.sort();
                        Ok(users)
                    })
                    .await
                    .map_err(to_store)
            })
        }

        fn is_online(&self, user: &str, now: u64) -> BoxFuture<'_, Result<bool, StoreError>> {
            let user = user.to_string();
            let since = self.since(now);
            Box::pin(async move {
                let score = self
                    .pool
                    .with(async |c| c.zscore(PRESENCE_KEY, &user).await)
                    .await
                    .map_err(to_store)?;
                Ok(score.is_some_and(|s| s >= since as i64))
            })
        }

        fn count(&self, now: u64) -> BoxFuture<'_, Result<usize, StoreError>> {
            let since = self.since(now);
            Box::pin(async move {
                let n = self
                    .pool
                    .with(async |c| c.zcount(PRESENCE_KEY, &since.to_string(), "+inf").await)
                    .await
                    .map_err(to_store)?;
                Ok(n.max(0) as usize)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    /// The in-memory tracker resolves on the first poll — no runtime needed.
    fn block<T>(fut: impl Future<Output = T>) -> T {
        let mut fut = Box::pin(fut);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("in-memory presence must resolve synchronously"),
        }
    }

    fn tracker() -> InMemoryPresence {
        InMemoryPresence::new(Duration::from_secs(30))
    }

    #[test]
    fn a_recent_heartbeat_is_online() {
        let p = tracker();
        block(p.heartbeat("alice", 1000)).unwrap();
        assert!(
            block(p.is_online("alice", 1010)).unwrap(),
            "beat 10s ago, window 30s"
        );
        assert_eq!(block(p.online(1010)).unwrap(), vec!["alice".to_string()]);
        assert_eq!(block(p.count(1010)).unwrap(), 1);
    }

    #[test]
    fn a_stale_heartbeat_falls_offline() {
        let p = tracker();
        block(p.heartbeat("alice", 1000)).unwrap();
        // 31s later, past the 30s window.
        assert!(
            !block(p.is_online("alice", 1031)).unwrap(),
            "beat is now stale"
        );
        assert_eq!(block(p.online(1031)).unwrap(), Vec::<String>::new());
        assert_eq!(block(p.count(1031)).unwrap(), 0);
    }

    #[test]
    fn a_fresh_beat_keeps_a_user_online() {
        let p = tracker();
        block(p.heartbeat("alice", 1000)).unwrap();
        // Beat again before the window closes → still online well past the first.
        block(p.heartbeat("alice", 1025)).unwrap();
        assert!(
            block(p.is_online("alice", 1050)).unwrap(),
            "the newer beat keeps her online"
        );
    }

    #[test]
    fn online_lists_every_live_user_sorted() {
        let p = tracker();
        block(p.heartbeat("carol", 1000)).unwrap();
        block(p.heartbeat("alice", 1000)).unwrap();
        block(p.heartbeat("bob", 1000)).unwrap();
        assert_eq!(
            block(p.online(1005)).unwrap(),
            vec!["alice".to_string(), "bob".to_string(), "carol".to_string()]
        );
    }

    fn user_tracker() -> InMemoryUserPresence {
        InMemoryUserPresence::new(Duration::from_secs(30))
    }

    #[test]
    fn one_user_on_two_devices_counts_once_but_reports_two_devices() {
        let p = user_tracker();
        block(p.heartbeat("alice", "laptop", "", 1000)).unwrap();
        block(p.heartbeat("alice", "phone", "", 1000)).unwrap();
        // One *person* online, on two devices — not two people.
        assert_eq!(block(p.count(1005)).unwrap(), 1);
        assert_eq!(block(p.devices("alice", 1005)).unwrap(), 2);
        assert_eq!(
            block(p.online(1005)).unwrap(),
            vec![OnlineUser { user: "alice".to_string(), devices: 2 }]
        );
    }

    #[test]
    fn losing_one_device_leaves_the_user_online_until_the_last_goes() {
        let p = user_tracker();
        block(p.heartbeat("alice", "laptop", "", 1000)).unwrap();
        block(p.heartbeat("alice", "phone", "", 1000)).unwrap();
        // The phone keeps beating; the laptop goes quiet.
        block(p.heartbeat("alice", "phone", "", 1025)).unwrap();
        // 20s after the laptop's last beat (still in window) — both devices live.
        assert_eq!(block(p.devices("alice", 1020)).unwrap(), 2);
        // 31s after the laptop's last beat — it aged out, but the phone (beat at 1025)
        // keeps her online on one device.
        assert!(block(p.is_online("alice", 1031)).unwrap());
        assert_eq!(block(p.devices("alice", 1031)).unwrap(), 1);
        assert_eq!(block(p.count(1031)).unwrap(), 1);
    }

    #[test]
    fn forget_signs_out_one_device_only() {
        let p = user_tracker();
        block(p.heartbeat("alice", "laptop", "", 1000)).unwrap();
        block(p.heartbeat("alice", "phone", "", 1000)).unwrap();
        // Explicit logout on the laptop — immediate, not waiting for the window.
        block(p.forget("alice", "laptop")).unwrap();
        assert_eq!(block(p.devices("alice", 1005)).unwrap(), 1);
        assert!(block(p.is_online("alice", 1005)).unwrap(), "phone still online");
        // Log the phone out too → she is fully offline and the user is dropped.
        block(p.forget("alice", "phone")).unwrap();
        assert!(!block(p.is_online("alice", 1005)).unwrap());
        assert_eq!(block(p.count(1005)).unwrap(), 0);
    }

    #[test]
    fn sessions_lists_each_device_with_its_meta() {
        let p = user_tracker();
        block(p.heartbeat("alice", "laptop", "ip=1.1.1.1;ua=Firefox", 1000)).unwrap();
        block(p.heartbeat("alice", "phone", "ip=2.2.2.2;ua=Safari", 1000)).unwrap();
        block(p.heartbeat("bob", "d1", "ip=3.3.3.3;ua=Chrome", 1000)).unwrap();
        let s = block(p.sessions(1005)).unwrap();
        // Flat per-device, sorted by (user, device): alice/laptop, alice/phone, bob/d1.
        assert_eq!(s.len(), 3);
        assert_eq!(
            s[0],
            DeviceSession {
                user: "alice".to_string(),
                device: "laptop".to_string(),
                meta: "ip=1.1.1.1;ua=Firefox".to_string(),
                last_seen: 1000,
            }
        );
        assert_eq!(s[1].device, "phone");
        assert_eq!(s[1].meta, "ip=2.2.2.2;ua=Safari");
        assert_eq!(s[2].user, "bob");
        // A stale device drops out of the flat list too.
        assert_eq!(block(p.sessions(1040)).unwrap().len(), 0, "all beats aged out");
    }

    #[test]
    fn count_is_distinct_users_and_online_is_sorted() {
        let p = user_tracker();
        block(p.heartbeat("carol", "d1", "", 1000)).unwrap();
        block(p.heartbeat("alice", "d1", "", 1000)).unwrap();
        block(p.heartbeat("alice", "d2", "", 1000)).unwrap();
        block(p.heartbeat("bob", "d1", "", 1000)).unwrap();
        assert_eq!(block(p.count(1005)).unwrap(), 3, "three people, four devices");
        assert_eq!(
            block(p.online(1005)).unwrap(),
            vec![
                OnlineUser { user: "alice".to_string(), devices: 2 },
                OnlineUser { user: "bob".to_string(), devices: 1 },
                OnlineUser { user: "carol".to_string(), devices: 1 },
            ]
        );
    }

    // The same presence semantics over a real Redis sorted set. Ignored (needs a
    // server); run with:
    //   KW_REDIS_ADDR=127.0.0.1:6380 cargo test -p kernway-security --features redis \
    //     presence::tests::redis -- --ignored
    #[cfg(feature = "redis")]
    mod redis {
        use super::super::RedisPresence;
        use super::*;
        use std::time::Duration;

        fn run<T>(fut: impl Future<Output = T>) -> T {
            rt_core::Executor::new().unwrap().block_on(fut).unwrap()
        }

        fn redis_tracker() -> RedisPresence {
            let addr = std::env::var("KW_REDIS_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:6379".to_string())
                .parse()
                .expect("KW_REDIS_ADDR must be host:port");
            RedisPresence::new(addr, Duration::from_secs(30))
        }

        #[test]
        #[ignore = "needs a Redis server (set KW_REDIS_ADDR or run one on 127.0.0.1:6379)"]
        fn heartbeat_online_and_expiry_over_redis() {
            let p = redis_tracker();
            run(async {
                // Base timestamp far in the future so it never collides with real data;
                // clean the two members first for an idempotent re-run.
                let base = 4_000_000_000u64;
                p.heartbeat("kw-presence-alice", base).await.unwrap();
                p.heartbeat("kw-presence-bob", base).await.unwrap();

                // Both within the 30s window.
                assert!(p.is_online("kw-presence-alice", base + 10).await.unwrap());
                let online = p.online(base + 10).await.unwrap();
                assert!(online.contains(&"kw-presence-alice".to_string()));
                assert!(online.contains(&"kw-presence-bob".to_string()));

                // Alice beats again; bob goes stale past the window.
                p.heartbeat("kw-presence-alice", base + 25).await.unwrap();
                assert!(
                    p.is_online("kw-presence-alice", base + 40).await.unwrap(),
                    "fresh beat"
                );
                assert!(
                    !p.is_online("kw-presence-bob", base + 40).await.unwrap(),
                    "stale → offline"
                );

                // A prune-and-read at base+40 drops bob and leaves alice.
                let online = p.online(base + 40).await.unwrap();
                assert_eq!(online, vec!["kw-presence-alice".to_string()]);

                // Clean up the test members regardless of window.
                p.heartbeat("kw-presence-alice", 0).await.unwrap();
                let _ = p.online(u64::MAX).await; // prunes the 0-scored member
            });
        }
    }
}
