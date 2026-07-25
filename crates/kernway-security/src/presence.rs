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
        Self { window_secs: window.as_secs(), beats: RwLock::new(HashMap::new()) }
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
            Ok(self.beats.read().unwrap().get(&user).is_some_and(|last| *last >= since))
        })
    }

    fn count(&self, now: u64) -> BoxFuture<'_, Result<usize, StoreError>> {
        Box::pin(async move {
            let since = self.since(now);
            Ok(self.beats.read().unwrap().values().filter(|last| **last >= since).count())
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
            Self { pool: Pool::new(addr), window_secs: window.as_secs() }
        }

        /// Use a pre-configured [`Pool`] (e.g. one carrying `AUTH`).
        #[must_use]
        pub fn from_pool(pool: Pool, window: Duration) -> Self {
            Self { pool, window_secs: window.as_secs() }
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
                        c.zremrangebyscore(PRESENCE_KEY, "-inf", &format!("({since}")).await?;
                        let mut users =
                            c.zrangebyscore(PRESENCE_KEY, &since.to_string(), "+inf").await?;
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
        assert!(block(p.is_online("alice", 1010)).unwrap(), "beat 10s ago, window 30s");
        assert_eq!(block(p.online(1010)).unwrap(), vec!["alice".to_string()]);
        assert_eq!(block(p.count(1010)).unwrap(), 1);
    }

    #[test]
    fn a_stale_heartbeat_falls_offline() {
        let p = tracker();
        block(p.heartbeat("alice", 1000)).unwrap();
        // 31s later, past the 30s window.
        assert!(!block(p.is_online("alice", 1031)).unwrap(), "beat is now stale");
        assert_eq!(block(p.online(1031)).unwrap(), Vec::<String>::new());
        assert_eq!(block(p.count(1031)).unwrap(), 0);
    }

    #[test]
    fn a_fresh_beat_keeps_a_user_online() {
        let p = tracker();
        block(p.heartbeat("alice", 1000)).unwrap();
        // Beat again before the window closes → still online well past the first.
        block(p.heartbeat("alice", 1025)).unwrap();
        assert!(block(p.is_online("alice", 1050)).unwrap(), "the newer beat keeps her online");
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
                assert!(p.is_online("kw-presence-alice", base + 40).await.unwrap(), "fresh beat");
                assert!(!p.is_online("kw-presence-bob", base + 40).await.unwrap(), "stale → offline");

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
