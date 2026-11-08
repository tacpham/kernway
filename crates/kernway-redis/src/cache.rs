//! Redis-backed [`AsyncCache`] — string keys, raw byte values.
//!
//! `!Send` (holds a per-core connection); build one per shard from
//! [`RedisConfig`]. Values are bytes, so the caller chooses the encoding (same
//! convention as pub/sub). Serialization-typed values can wrap this later.
//!
//! `get_or_load` uses the trait default (no single-flight): `AsyncSingleFlight`
//! coalesces on a **sync** peek, which fits an in-memory cache but not Redis
//! (a peek here is itself async I/O). Cross-node coalescing wants a distributed
//! lease — a documented follow-up.

use crate::conn::Connection;
use crate::error::RedisError;
use crate::pubsub::RedisConfig;
use crate::resp::Value;
use kernway_cache_core::async_cache::{AsyncCache, BoxFuture};
use kernway_cache_core::{CacheError, CacheStats, Ttl};
use std::cell::RefCell;

fn to_cache_err(e: RedisError) -> CacheError {
    match e {
        RedisError::Io(e) => CacheError::Connection(e.to_string()),
        RedisError::Closed => CacheError::Connection("redis connection closed".into()),
        other => CacheError::Operation(other.to_string()),
    }
}

/// Redis-backed cache over string keys and byte values.
pub struct RedisCache {
    config: RedisConfig,
    conn: RefCell<Option<Connection>>,
}

impl RedisCache {
    /// Create a handle (connects lazily on first use).
    pub fn new(config: RedisConfig) -> Self {
        Self { config, conn: RefCell::new(None) }
    }

    async fn open(&self) -> Result<Connection, CacheError> {
        let mut conn = Connection::connect(self.config.addr).await.map_err(to_cache_err)?;
        if let Some(password) = &self.config.password {
            conn.auth(self.config.user.as_deref(), password).await.map_err(to_cache_err)?;
        }
        Ok(conn)
    }

    /// Take the pooled connection out (or open one) — never held across an await
    /// under a RefCell borrow.
    async fn take_conn(&self) -> Result<Connection, CacheError> {
        match self.conn.borrow_mut().take() {
            Some(c) => Ok(c),
            None => self.open().await,
        }
    }

    fn return_conn(&self, conn: Connection) {
        *self.conn.borrow_mut() = Some(conn);
    }
}

impl AsyncCache<String, Vec<u8>> for RedisCache {
    fn get<'a>(&'a self, key: &'a String) -> BoxFuture<'a, Result<Option<Vec<u8>>, CacheError>> {
        Box::pin(async move {
            let mut conn = self.take_conn().await?;
            match conn.get(key).await {
                Ok(v) => {
                    self.return_conn(conn);
                    Ok(v)
                }
                Err(e) => Err(to_cache_err(e)),
            }
        })
    }

    fn put<'a>(&'a self, key: String, value: Vec<u8>, ttl: Ttl) -> BoxFuture<'a, Result<(), CacheError>> {
        Box::pin(async move {
            let mut conn = self.take_conn().await?;
            let result = match ttl.as_duration() {
                Some(d) => conn.set_ex(&key, &value, d.as_secs()).await,
                None => conn.command(&[b"SET", key.as_bytes(), &value]).await.map(|_| ()),
            };
            match result {
                Ok(()) => {
                    self.return_conn(conn);
                    Ok(())
                }
                Err(e) => Err(to_cache_err(e)),
            }
        })
    }

    fn put_if_absent<'a>(
        &'a self,
        key: String,
        value: Vec<u8>,
        ttl: Ttl,
    ) -> BoxFuture<'a, Result<bool, CacheError>> {
        Box::pin(async move {
            let mut conn = self.take_conn().await?;
            // SET key value NX [EX secs] — replies OK if stored, Nil if it existed.
            let ttl_secs = ttl.as_duration().map(|d| d.as_secs().to_string());
            let mut args: Vec<&[u8]> = vec![b"SET", key.as_bytes(), &value, b"NX"];
            if let Some(secs) = &ttl_secs {
                args.push(b"EX");
                args.push(secs.as_bytes());
            }
            match conn.command(&args).await {
                Ok(v) => {
                    self.return_conn(conn);
                    Ok(matches!(v, Value::Simple(_)))
                }
                Err(e) => Err(to_cache_err(e)),
            }
        })
    }

    fn evict<'a>(&'a self, key: &'a String) -> BoxFuture<'a, Result<(), CacheError>> {
        Box::pin(async move {
            let mut conn = self.take_conn().await?;
            match conn.del(&[key.as_str()]).await {
                Ok(_) => {
                    self.return_conn(conn);
                    Ok(())
                }
                Err(e) => Err(to_cache_err(e)),
            }
        })
    }

    /// `FLUSHDB` — clears the **current Redis database**. Use a dedicated DB or
    /// key prefixes if other data shares it.
    fn clear(&self) -> BoxFuture<'_, Result<(), CacheError>> {
        Box::pin(async move {
            let mut conn = self.take_conn().await?;
            match conn.command(&[b"FLUSHDB"]).await {
                Ok(_) => {
                    self.return_conn(conn);
                    Ok(())
                }
                Err(e) => Err(to_cache_err(e)),
            }
        })
    }

    fn contains<'a>(&'a self, key: &'a String) -> BoxFuture<'a, Result<bool, CacheError>> {
        Box::pin(async move {
            let mut conn = self.take_conn().await?;
            match conn.command(&[b"EXISTS", key.as_bytes()]).await {
                Ok(v) => {
                    self.return_conn(conn);
                    Ok(v.as_int().unwrap_or(0) > 0)
                }
                Err(e) => Err(to_cache_err(e)),
            }
        })
    }

    fn size(&self) -> BoxFuture<'_, Result<usize, CacheError>> {
        Box::pin(async move {
            let mut conn = self.take_conn().await?;
            match conn.command(&[b"DBSIZE"]).await {
                Ok(v) => {
                    self.return_conn(conn);
                    Ok(v.as_int().unwrap_or(0).max(0) as usize)
                }
                Err(e) => Err(to_cache_err(e)),
            }
        })
    }

    fn stats(&self) -> CacheStats {
        // Redis keeps server-side stats; this client does not track per-call hits.
        CacheStats::default()
    }
}
