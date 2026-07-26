//! Activity — *who is here right now and what are they looking at*, a passive,
//! request-driven companion to [`Presence`](crate::presence).
//!
//! Presence answers "is this user online" from an explicit heartbeat. Activity is
//! recorded on *every request*, with no client cooperation: each request updates one
//! [`ActiveVisitor`] — the identity (a logged-in user, else the anonymous visitor
//! id), the current path/method, the client IP, and the User-Agent — so an admin can
//! see a live list of who is on the site and where. It is built straight from the
//! [`RequestMeta`](crate::tracking::RequestMeta) a `VisitorTracking` middleware
//! already put in the scope, plus the `SecurityContext` for the identity.
//!
//! Like presence, an entry is live only within a window of its last request and is
//! pruned as the list is read — a visitor who stops making requests falls out on
//! their own. Two backends, same shape as the session and presence stores:
//! [`InMemoryActivity`] and (feature = `redis`) [`RedisActivity`].

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::RwLock;
use std::time::Duration;

use kernway_core::layer::BoxFuture;

use crate::session::StoreError;

/// One live visitor: who they are, where they are, and how they reached us. Built
/// from a request's [`RequestMeta`](crate::tracking::RequestMeta) + `SecurityContext`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveVisitor {
    /// The stable identity — a logged-in user's principal, else the anonymous
    /// visitor id. This is the map key, so one person is one row.
    pub id: String,
    /// Whether `id` is an authenticated user (vs an anonymous visitor id).
    pub authenticated: bool,
    /// The resolved client IP (proxy-aware), if known.
    pub ip: Option<IpAddr>,
    /// The User-Agent, if the client sent one.
    pub user_agent: Option<String>,
    /// The path of their most recent request — the page or API they are on now.
    pub path: String,
    /// The method of that request.
    pub method: String,
    /// Unix seconds of that request — the liveness clock.
    pub last_seen: u64,
}

/// Records the latest request per visitor and reports who is currently active. `now`
/// is unix seconds, passed in so the caller owns the clock (and tests can too).
pub trait Activity: Send + Sync {
    /// Record `visitor` as their most recent request (overwrites any earlier one).
    fn record(&self, visitor: ActiveVisitor) -> BoxFuture<'_, Result<(), StoreError>>;
    /// The visitors whose last request is within the window of `now`, most-recent
    /// first.
    fn active(&self, now: u64) -> BoxFuture<'_, Result<Vec<ActiveVisitor>, StoreError>>;
    /// How many visitors are active within the window of `now`.
    fn count(&self, now: u64) -> BoxFuture<'_, Result<usize, StoreError>>;
}

/// In-memory activity: `id -> latest ActiveVisitor`. For a single instance; a record
/// is a map write, and reads prune anything past the window as they go.
pub struct InMemoryActivity {
    window_secs: u64,
    visitors: RwLock<HashMap<String, ActiveVisitor>>,
}

impl InMemoryActivity {
    /// A tracker whose window is `window` — a visitor stays active for that long
    /// after their last request.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self { window_secs: window.as_secs(), visitors: RwLock::new(HashMap::new()) }
    }

    /// The oldest request that still counts as active at `now`.
    fn since(&self, now: u64) -> u64 {
        now.saturating_sub(self.window_secs)
    }
}

impl Activity for InMemoryActivity {
    fn record(&self, visitor: ActiveVisitor) -> BoxFuture<'_, Result<(), StoreError>> {
        Box::pin(async move {
            self.visitors.write().unwrap().insert(visitor.id.clone(), visitor);
            Ok(())
        })
    }

    fn active(&self, now: u64) -> BoxFuture<'_, Result<Vec<ActiveVisitor>, StoreError>> {
        Box::pin(async move {
            let since = self.since(now);
            let mut visitors = self.visitors.write().unwrap();
            visitors.retain(|_, v| v.last_seen >= since); // prune stale as we read
            let mut live: Vec<ActiveVisitor> = visitors.values().cloned().collect();
            live.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then_with(|| a.id.cmp(&b.id)));
            Ok(live)
        })
    }

    fn count(&self, now: u64) -> BoxFuture<'_, Result<usize, StoreError>> {
        Box::pin(async move {
            let since = self.since(now);
            Ok(self.visitors.read().unwrap().values().filter(|v| v.last_seen >= since).count())
        })
    }
}

/// Redis-backed activity (feature = `redis`): a sorted set scored by last-request
/// timestamp for liveness + listing, plus one key per visitor holding the encoded
/// record. Shared across instances.
///
/// - record → `ZADD kw:active last_seen id` + `SET kw:active:{id} <record> EX window`
/// - active → `ZREMRANGEBYSCORE` prune, `ZRANGEBYSCORE`, then one `GET` per id
/// - count → `ZCOUNT`
///
/// The per-id `GET` fan-out in `active` is an admin-view read (rare, few rows), not a
/// hot path — a request only does the single `ZADD` + `SET`.
#[cfg(feature = "redis")]
pub use redis_impl::RedisActivity;

#[cfg(feature = "redis")]
mod redis_impl {
    use std::net::SocketAddr;
    use std::time::Duration;

    use kernway_core::layer::BoxFuture;
    use kernway_redis::{Pool, RedisError};

    use super::{decode_visitor, encode_visitor, ActiveVisitor, Activity};
    use crate::session::StoreError;

    /// The sorted set of every visitor's last-request timestamp.
    const INDEX_KEY: &str = "kw:active";

    fn data_key(id: &str) -> String {
        format!("kw:active:{id}")
    }

    fn to_store(err: RedisError) -> StoreError {
        StoreError::Backend(err.to_string())
    }

    /// Activity over Redis.
    pub struct RedisActivity {
        pool: Pool,
        window_secs: u64,
    }

    impl RedisActivity {
        /// Connect to `addr`, with a `window`-long active window.
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

    impl Activity for RedisActivity {
        fn record(&self, visitor: ActiveVisitor) -> BoxFuture<'_, Result<(), StoreError>> {
            let ttl = self.window_secs.max(1);
            Box::pin(async move {
                let key = data_key(&visitor.id);
                let bytes = encode_visitor(&visitor);
                self.pool
                    .with(async |c| {
                        c.zadd(INDEX_KEY, visitor.last_seen as i64, &visitor.id).await?;
                        c.set_ex(&key, &bytes, ttl).await
                    })
                    .await
                    .map_err(to_store)
            })
        }

        fn active(&self, now: u64) -> BoxFuture<'_, Result<Vec<ActiveVisitor>, StoreError>> {
            let since = self.since(now);
            Box::pin(async move {
                self.pool
                    .with(async |c| {
                        // Drop everything older than the window, then read the ids left.
                        c.zremrangebyscore(INDEX_KEY, "-inf", &format!("({since}")).await?;
                        let ids = c.zrangebyscore(INDEX_KEY, &since.to_string(), "+inf").await?;
                        let mut live = Vec::with_capacity(ids.len());
                        for id in ids {
                            if let Some(bytes) = c.get(&data_key(&id)).await? {
                                if let Some(v) = decode_visitor(&bytes) {
                                    live.push(v);
                                }
                            }
                        }
                        // Most-recent first, id as a stable tiebreak.
                        live.sort_by(|a, b| b.last_seen.cmp(&a.last_seen).then_with(|| a.id.cmp(&b.id)));
                        Ok(live)
                    })
                    .await
                    .map_err(to_store)
            })
        }

        fn count(&self, now: u64) -> BoxFuture<'_, Result<usize, StoreError>> {
            let since = self.since(now);
            Box::pin(async move {
                let n = self
                    .pool
                    .with(async |c| c.zcount(INDEX_KEY, &since.to_string(), "+inf").await)
                    .await
                    .map_err(to_store)?;
                Ok(n.max(0) as usize)
            })
        }
    }
}

// A length-prefixed binary encoding for the Redis record (the same approach as the
// session store), so kernway-security stays serde-free. Binary-safe: an arbitrary
// User-Agent or path survives intact.
#[cfg(feature = "redis")]
fn encode_visitor(v: &ActiveVisitor) -> Vec<u8> {
    let mut out = Vec::new();
    put_str(&mut out, &v.id);
    out.push(u8::from(v.authenticated));
    put_str(&mut out, &v.ip.map_or_else(String::new, |ip| ip.to_string()));
    put_opt(&mut out, v.user_agent.as_deref());
    put_str(&mut out, &v.path);
    put_str(&mut out, &v.method);
    out.extend_from_slice(&v.last_seen.to_le_bytes());
    out
}

#[cfg(feature = "redis")]
fn decode_visitor(b: &[u8]) -> Option<ActiveVisitor> {
    let mut pos = 0;
    let id = take_str(b, &mut pos)?;
    let authenticated = *b.get(pos)? != 0;
    pos += 1;
    let ip_str = take_str(b, &mut pos)?;
    let ip = if ip_str.is_empty() { None } else { ip_str.parse().ok() };
    let user_agent = take_opt(b, &mut pos)?;
    let path = take_str(b, &mut pos)?;
    let method = take_str(b, &mut pos)?;
    let last_seen = take_u64(b, &mut pos)?;
    Some(ActiveVisitor { id, authenticated, ip, user_agent, path, method, last_seen })
}

#[cfg(feature = "redis")]
fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

#[cfg(feature = "redis")]
fn put_opt(out: &mut Vec<u8>, s: Option<&str>) {
    out.push(u8::from(s.is_some()));
    put_str(out, s.unwrap_or(""));
}

#[cfg(feature = "redis")]
fn take_opt(b: &[u8], pos: &mut usize) -> Option<Option<String>> {
    let present = *b.get(*pos)? != 0;
    *pos += 1;
    let s = take_str(b, pos)?;
    Some(present.then_some(s))
}

#[cfg(feature = "redis")]
fn take_str(b: &[u8], pos: &mut usize) -> Option<String> {
    let len = take_u32(b, pos)? as usize;
    let end = pos.checked_add(len)?;
    let s = String::from_utf8(b.get(*pos..end)?.to_vec()).ok()?;
    *pos = end;
    Some(s)
}

#[cfg(feature = "redis")]
fn take_u32(b: &[u8], pos: &mut usize) -> Option<u32> {
    let end = pos.checked_add(4)?;
    let v = u32::from_le_bytes(b.get(*pos..end)?.try_into().ok()?);
    *pos = end;
    Some(v)
}

#[cfg(feature = "redis")]
fn take_u64(b: &[u8], pos: &mut usize) -> Option<u64> {
    let end = pos.checked_add(8)?;
    let v = u64::from_le_bytes(b.get(*pos..end)?.try_into().ok()?);
    *pos = end;
    Some(v)
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
            Poll::Pending => panic!("in-memory activity must resolve synchronously"),
        }
    }

    fn visitor(id: &str, path: &str, last_seen: u64) -> ActiveVisitor {
        ActiveVisitor {
            id: id.to_string(),
            authenticated: !id.starts_with("anon-"),
            ip: Some("10.0.0.1".parse().unwrap()),
            user_agent: Some("test/1.0".to_string()),
            path: path.to_string(),
            method: "GET".to_string(),
            last_seen,
        }
    }

    fn tracker() -> InMemoryActivity {
        InMemoryActivity::new(Duration::from_secs(30))
    }

    #[test]
    fn a_recent_request_is_active_with_its_details() {
        let a = tracker();
        block(a.record(visitor("alice", "/dashboard", 1000))).unwrap();
        let live = block(a.active(1010)).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, "alice");
        assert_eq!(live[0].path, "/dashboard", "we see the page she is on");
        assert!(live[0].authenticated);
        assert_eq!(block(a.count(1010)).unwrap(), 1);
    }

    #[test]
    fn the_latest_request_wins() {
        let a = tracker();
        block(a.record(visitor("alice", "/dashboard", 1000))).unwrap();
        block(a.record(visitor("alice", "/settings", 1020))).unwrap();
        let live = block(a.active(1030)).unwrap();
        assert_eq!(live.len(), 1, "still one row per identity");
        assert_eq!(live[0].path, "/settings", "moved to the newer page");
    }

    #[test]
    fn a_stale_visitor_is_pruned() {
        let a = tracker();
        block(a.record(visitor("alice", "/x", 1000))).unwrap();
        // 31s later, past the 30s window.
        assert_eq!(block(a.active(1031)).unwrap(), Vec::new());
        assert_eq!(block(a.count(1031)).unwrap(), 0);
    }

    #[test]
    fn active_is_sorted_most_recent_first() {
        let a = tracker();
        block(a.record(visitor("alice", "/a", 1000))).unwrap();
        block(a.record(visitor("anon-42", "/b", 1005))).unwrap();
        block(a.record(visitor("bob", "/c", 1003))).unwrap();
        let ids: Vec<String> = block(a.active(1010)).unwrap().into_iter().map(|v| v.id).collect();
        assert_eq!(ids, vec!["anon-42".to_string(), "bob".to_string(), "alice".to_string()]);
    }

    /// The Redis codec is binary-safe and preserves every field, including a `None`
    /// IP/UA and a path with awkward bytes.
    #[cfg(feature = "redis")]
    #[test]
    fn the_redis_codec_round_trips() {
        for v in [
            visitor("alice", "/dashboard?tab=1", 4_000_000_000),
            ActiveVisitor {
                id: "anon-7".to_string(),
                authenticated: false,
                ip: None,
                user_agent: None,
                path: "/x\ty".to_string(), // a tab in the path
                method: "POST".to_string(),
                last_seen: 42,
            },
        ] {
            let decoded = decode_visitor(&encode_visitor(&v)).unwrap();
            assert_eq!(decoded, v);
        }
    }

    // The same activity semantics over a real Redis. Ignored (needs a server); run:
    //   KW_REDIS_ADDR=127.0.0.1:6380 cargo test -p kernway-security \
    //     --features presence,redis activity::tests::redis -- --ignored
    #[cfg(feature = "redis")]
    mod redis {
        use super::super::RedisActivity;
        use super::*;

        fn run<T>(fut: impl Future<Output = T>) -> T {
            rt_core::Executor::new().unwrap().block_on(fut).unwrap()
        }

        fn redis_tracker() -> RedisActivity {
            let addr = std::env::var("KW_REDIS_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:6379".to_string())
                .parse()
                .expect("KW_REDIS_ADDR must be host:port");
            RedisActivity::new(addr, Duration::from_secs(30))
        }

        #[test]
        #[ignore = "needs a Redis server (set KW_REDIS_ADDR or run one on 127.0.0.1:6379)"]
        fn record_active_and_expiry_over_redis() {
            let a = redis_tracker();
            run(async {
                // Timestamps far in the future so they never collide with real data.
                let base = 4_100_000_000u64;
                a.record(visitor("kw-act-alice", "/dashboard", base)).await.unwrap();
                a.record(visitor("kw-act-bob", "/reports", base)).await.unwrap();

                // Both within the 30s window, with their pages visible.
                let live = a.active(base + 10).await.unwrap();
                let alice = live.iter().find(|v| v.id == "kw-act-alice").expect("alice active");
                assert_eq!(alice.path, "/dashboard");
                assert!(live.iter().any(|v| v.id == "kw-act-bob" && v.path == "/reports"));

                // Alice moves to a new page; bob goes stale past the window.
                a.record(visitor("kw-act-alice", "/settings", base + 25)).await.unwrap();
                let live = a.active(base + 40).await.unwrap();
                assert_eq!(live.len(), 1, "only alice remains");
                assert_eq!(live[0].id, "kw-act-alice");
                assert_eq!(live[0].path, "/settings", "her latest page");

                // Clean up regardless of window.
                a.record(visitor("kw-act-alice", "/x", 0)).await.unwrap();
                let _ = a.active(u64::MAX).await; // prunes the 0-scored member
            });
        }
    }
}
