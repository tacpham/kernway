//! `AsyncCache` — the async-top cache contract.
//!
//! The unifying abstraction: one trait every backend implements, sync or async.
//! An async method can express a **sync** backend for free — an in-memory
//! implementation returns a future that is `Ready` on first poll (a `HashMap`
//! lookup, no suspension, no scheduler hop). But a sync method *cannot* express
//! an async backend without blocking a thread — and blocking a reactor shard
//! stalls (or deadlocks) a whole core. So the common surface is async; sync
//! backends pay almost nothing, async backends (Redis) get real non-blocking I/O.
//!
//! Futures are **`!Send`** (shard-local, polled where created), but the cache
//! *value* can still be shared across shards: a `Send + Sync` in-memory cache
//! whose per-call futures are `!Send` is exactly the handler model (the function
//! is shared, each call's future is local).
//!
//! The matching sync [`Cache`](crate::Cache) trait stays, for code that is purely
//! synchronous and wants zero `.await`.

use crate::{error::CacheError, stats::CacheStats, ttl::Ttl};
use std::future::Future;
use std::pin::Pin;

/// Boxed future used by [`AsyncCache`]. Not `Send` — see the module docs.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Async cache over key `K` and value `V`.
///
/// Core operations are object-safe (`dyn AsyncCache` works);
/// [`get_or_load`](Self::get_or_load) is a generic default gated on `Self: Sized`.
pub trait AsyncCache<K, V>
where
    V: Clone,
{
    /// Retrieve a value by key. `None` on miss or expiry.
    fn get<'a>(&'a self, key: &'a K) -> BoxFuture<'a, Result<Option<V>, CacheError>>;

    /// Store a value with a TTL.
    fn put<'a>(&'a self, key: K, value: V, ttl: Ttl) -> BoxFuture<'a, Result<(), CacheError>>;

    /// Store only if the key is absent. Resolves to `true` if it was stored.
    fn put_if_absent<'a>(&'a self, key: K, value: V, ttl: Ttl) -> BoxFuture<'a, Result<bool, CacheError>>;

    /// Remove a key.
    fn evict<'a>(&'a self, key: &'a K) -> BoxFuture<'a, Result<(), CacheError>>;

    /// Remove all entries.
    fn clear(&self) -> BoxFuture<'_, Result<(), CacheError>>;

    /// Whether a key exists and is not expired. Default: `get(...).is_some()`.
    fn contains<'a>(&'a self, key: &'a K) -> BoxFuture<'a, Result<bool, CacheError>> {
        Box::pin(async move { Ok(self.get(key).await?.is_some()) })
    }

    /// Number of non-expired entries.
    fn size(&self) -> BoxFuture<'_, Result<usize, CacheError>>;

    /// Hit/miss statistics.
    fn stats(&self) -> CacheStats;

    /// Get-or-compute with an **async** loader: return the cached value or run
    /// `loader`, store its result, and return it.
    ///
    /// This default does not coalesce concurrent misses — a backend that wants
    /// single-flight (e.g. the in-memory cache via
    /// [`AsyncSingleFlight`](crate::singleflight::AsyncSingleFlight)) overrides it.
    fn get_or_load<'a, F, Fut>(&'a self, key: K, ttl: Ttl, loader: F) -> BoxFuture<'a, Result<V, CacheError>>
    where
        F: FnOnce() -> Fut + 'a,
        Fut: Future<Output = Result<V, CacheError>> + 'a,
        K: Clone + 'a,
        V: 'a,
        Self: Sized,
    {
        Box::pin(async move {
            if let Some(v) = self.get(&key).await? {
                return Ok(v);
            }
            let v = loader().await?;
            self.put(key, v.clone(), ttl).await?;
            Ok(v)
        })
    }
}
