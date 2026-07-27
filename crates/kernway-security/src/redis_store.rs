//! A Redis-backed `SessionStore` (feature = `redis`).
//!
//! The distributed backend for a multi-instance deployment: the session registry
//! lives in Redis instead of one process's memory, so every instance sees the same
//! logins and revocations. It drives [`kernway_redis`] — the async RESP client on
//! Kernway's own runtime — so a lookup awaits the network without blocking a core
//! ([KEP-0006]).
//!
//! ## Keys
//!
//! - `kw:sess:{sid}` — the encoded `SessionRecord`, with a TTL backstop (KEP-0004:
//!   the storage TTL is a garbage-collection floor, never the authority on timeout).
//! - `kw:user:{user}` — a set of the user's `sid`s, for `sessions_of` / `remove_user`.
//! - `kw:sess:index` — a set of every live `sid`, for the `len` capacity check.
//!
//! ## Errors are reported, not swallowed
//!
//! Every method returns [`Result`]: a Redis failure surfaces as
//! `StoreError::Backend` rather than being hidden. The *policy* for a failure
//! lives in the [`SessionManager`](crate::session::SessionManager), not here —
//! `login` fails loudly (no token for an unstored session), while `authenticate`
//! fails closed (a registry it cannot reach means "not authenticated"). This
//! backend's only job is to talk to Redis and report honestly what happened.
//!
//! [KEP-0006]: https://github.com/tacpham/kernway/blob/main/docs/kep/0006-async-handlers.md

use std::net::SocketAddr;
use std::time::Duration;

use kernway_core::layer::BoxFuture;
use kernway_redis::{Pool, RedisError};

use crate::session::{SessionRecord, SessionStore, StoreError};

/// The set holding every live `sid`, for `len`.
const INDEX_KEY: &str = "kw:sess:index";

/// A `SessionStore` backed by Redis via [`kernway_redis`].
pub struct RedisSessionStore {
    pool: Pool,
    /// Storage TTL backstop, in seconds — refreshed on write and on `touch`.
    ttl_secs: u64,
}

impl RedisSessionStore {
    /// Connect to `addr`, keeping each stored session for at most `ttl` (the GC
    /// backstop — set it to at least the maximum session lifetime).
    #[must_use]
    pub fn new(addr: SocketAddr, ttl: Duration) -> Self {
        Self { pool: Pool::new(addr), ttl_secs: ttl.as_secs() }
    }

    /// Use a pre-configured [`Pool`] (e.g. one carrying `AUTH` credentials).
    #[must_use]
    pub fn from_pool(pool: Pool, ttl: Duration) -> Self {
        Self { pool, ttl_secs: ttl.as_secs() }
    }
}

/// Map the client's error to the trait's backend-agnostic [`StoreError`].
fn to_store(err: RedisError) -> StoreError {
    StoreError::Backend(err.to_string())
}

fn sess_key(sid: &str) -> String {
    format!("kw:sess:{sid}")
}

fn user_key(user: &str) -> String {
    format!("kw:user:{user}")
}

impl SessionStore for RedisSessionStore {
    fn insert(&self, sid: &str, record: SessionRecord) -> BoxFuture<'_, Result<(), StoreError>> {
        let sid = sid.to_string();
        let user = record.user.clone();
        let bytes = encode_record(&record);
        let ttl = self.ttl_secs;
        Box::pin(async move {
            let sk = sess_key(&sid);
            let uk = user_key(&user);
            self.pool
                .with(async |c| {
                    c.set_ex(&sk, &bytes, ttl).await?;
                    c.sadd(&uk, &sid).await?;
                    c.expire(&uk, ttl).await?; // keep the user index alive as long as a session
                    c.sadd(INDEX_KEY, &sid).await?;
                    Ok(())
                })
                .await
                .map_err(to_store)
        })
    }

    fn get(&self, sid: &str) -> BoxFuture<'_, Result<Option<SessionRecord>, StoreError>> {
        let sk = sess_key(sid);
        Box::pin(async move {
            let bytes = self.pool.with(async |c| c.get(&sk).await).await.map_err(to_store)?;
            Ok(bytes.and_then(|b| decode_record(&b)))
        })
    }

    fn touch(&self, sid: &str, at: u64) -> BoxFuture<'_, Result<(), StoreError>> {
        let sk = sess_key(sid);
        let ttl = self.ttl_secs;
        Box::pin(async move {
            self.pool
                .with(async |c| {
                    if let Some(bytes) = c.get(&sk).await? {
                        if let Some(mut record) = decode_record(&bytes) {
                            record.last_seen = at;
                            c.set_ex(&sk, &encode_record(&record), ttl).await?;
                        }
                    }
                    Ok(())
                })
                .await
                .map_err(to_store)
        })
    }

    fn remove(&self, sid: &str) -> BoxFuture<'_, Result<(), StoreError>> {
        let sid = sid.to_string();
        Box::pin(async move {
            let sk = sess_key(&sid);
            self.pool
                .with(async |c| {
                    // Find the owning user first, to clean its index.
                    let user = c.get(&sk).await?.and_then(|b| decode_record(&b)).map(|r| r.user);
                    c.del(&[sk.as_str()]).await?;
                    if let Some(user) = user {
                        c.srem(&user_key(&user), &sid).await?;
                    }
                    c.srem(INDEX_KEY, &sid).await?;
                    Ok(())
                })
                .await
                .map_err(to_store)
        })
    }

    fn remove_user(&self, user: &str) -> BoxFuture<'_, Result<(), StoreError>> {
        let uk = user_key(user);
        Box::pin(async move {
            self.pool
                .with(async |c| {
                    for sid in c.smembers(&uk).await? {
                        c.del(&[sess_key(&sid).as_str()]).await?;
                        c.srem(INDEX_KEY, &sid).await?;
                    }
                    c.del(&[uk.as_str()]).await?;
                    Ok(())
                })
                .await
                .map_err(to_store)
        })
    }

    fn sessions_of(&self, user: &str) -> BoxFuture<'_, Result<Vec<(String, SessionRecord)>, StoreError>> {
        let uk = user_key(user);
        Box::pin(async move {
            self.pool
                .with(async |c| {
                    let mut out = Vec::new();
                    for sid in c.smembers(&uk).await? {
                        if let Some(bytes) = c.get(&sess_key(&sid)).await? {
                            if let Some(record) = decode_record(&bytes) {
                                out.push((sid, record));
                            }
                        }
                    }
                    Ok(out)
                })
                .await
                .map_err(to_store)
        })
    }

    fn len(&self) -> BoxFuture<'_, Result<usize, StoreError>> {
        Box::pin(async move {
            let n = self.pool.with(async |c| c.scard(INDEX_KEY).await).await.map_err(to_store)?;
            Ok(n.max(0) as usize)
        })
    }
}

// --- SessionRecord ⇄ bytes -------------------------------------------------
//
// A length-prefixed binary encoding (u32 len + bytes for each string, u64 LE for
// each timestamp). Binary-safe, so it survives arbitrary usernames/metadata, and
// keeps kernway-security free of a serde dependency for the one type Redis stores.

fn encode_record(r: &SessionRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(24 + r.user.len() + r.meta.len());
    put_str(&mut out, &r.user);
    out.extend_from_slice(&r.created.to_le_bytes());
    out.extend_from_slice(&r.last_seen.to_le_bytes());
    put_str(&mut out, &r.meta);
    out
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn decode_record(b: &[u8]) -> Option<SessionRecord> {
    let mut pos = 0;
    let user = take_str(b, &mut pos)?;
    let created = take_u64(b, &mut pos)?;
    let last_seen = take_u64(b, &mut pos)?;
    let meta = take_str(b, &mut pos)?;
    Some(SessionRecord { user, created, last_seen, meta })
}

fn take_str(b: &[u8], pos: &mut usize) -> Option<String> {
    let len = take_u32(b, pos)? as usize;
    let end = pos.checked_add(len)?;
    let s = String::from_utf8(b.get(*pos..end)?.to_vec()).ok()?;
    *pos = end;
    Some(s)
}

fn take_u32(b: &[u8], pos: &mut usize) -> Option<u32> {
    let end = pos.checked_add(4)?;
    let v = u32::from_le_bytes(b.get(*pos..end)?.try_into().ok()?);
    *pos = end;
    Some(v)
}

fn take_u64(b: &[u8], pos: &mut usize) -> Option<u64> {
    let end = pos.checked_add(8)?;
    let v = u64::from_le_bytes(b.get(*pos..end)?.try_into().ok()?);
    *pos = end;
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_through_bytes() {
        let record = SessionRecord {
            user: "alice".to_string(),
            created: 1_700_000_000,
            last_seen: 1_700_000_042,
            meta: "chrome / 10.0.0.1".to_string(),
        };
        let decoded = decode_record(&encode_record(&record)).unwrap();
        assert_eq!(decoded.user, record.user);
        assert_eq!(decoded.created, record.created);
        assert_eq!(decoded.last_seen, record.last_seen);
        assert_eq!(decoded.meta, record.meta);
    }

    #[test]
    fn empty_strings_round_trip() {
        let record = SessionRecord { user: String::new(), created: 0, last_seen: 0, meta: String::new() };
        let decoded = decode_record(&encode_record(&record)).unwrap();
        assert_eq!(decoded.user, "");
        assert_eq!(decoded.meta, "");
    }

    #[test]
    fn truncated_bytes_decode_to_none_not_a_panic() {
        let bytes = encode_record(&SessionRecord {
            user: "bob".to_string(),
            created: 1,
            last_seen: 2,
            meta: "x".to_string(),
        });
        // Every prefix shorter than the whole must fail cleanly.
        for cut in 0..bytes.len() {
            assert!(decode_record(&bytes[..cut]).is_none(), "prefix len {cut} must not decode");
        }
        assert!(decode_record(&bytes).is_some());
    }
}
