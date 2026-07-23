use crate::{error::CacheError, stats::CacheStats, ttl::Ttl};
use std::time::Instant;

/// A cached value with metadata.
#[derive(Debug, Clone)]
pub struct CacheEntry<V> {
    /// The cached value.
    pub value: V,
    /// When the entry was stored. Expiry is measured from here, so a re-`put`
    /// resets the clock.
    pub created_at: Instant,
    /// How long the entry stays valid.
    pub ttl: Ttl,
}

impl<V> CacheEntry<V> {
    /// Wrap a value, stamping it with the current time.
    pub fn new(value: V, ttl: Ttl) -> Self {
        Self { value, created_at: Instant::now(), ttl }
    }

    /// Returns true if this entry has expired.
    pub fn is_expired(&self) -> bool {
        match self.ttl.as_duration() {
            None => false,
            Some(duration) => self.created_at.elapsed() >= duration,
        }
    }
}

/// Core cache trait — synchronous, generic over key and value types.
///
/// Equivalent to Spring's `CacheManager` + `Cache` combined.
/// Implement this trait to provide Redis, Memcached, or in-memory caching.
pub trait Cache<K, V>: Send + Sync + 'static
where
    K: Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Retrieve a value by key. Returns None on miss or expired entry.
    fn get(&self, key: &K) -> Result<Option<V>, CacheError>;

    /// Store a value with a TTL.
    fn put(&self, key: K, value: V, ttl: Ttl) -> Result<(), CacheError>;

    /// Store a value only if the key does not already exist.
    /// Returns true if the value was stored, false if key already existed.
    fn put_if_absent(&self, key: K, value: V, ttl: Ttl) -> Result<bool, CacheError>;

    /// Remove a key from the cache.
    fn evict(&self, key: &K) -> Result<(), CacheError>;

    /// Remove all entries from the cache.
    fn clear(&self) -> Result<(), CacheError>;

    /// Check if a key exists (and is not expired).
    fn contains(&self, key: &K) -> Result<bool, CacheError>;

    /// Number of non-expired entries.
    fn size(&self) -> Result<usize, CacheError>;

    /// Get-or-compute: retrieve cached value or call loader, store result with ttl.
    ///
    /// Equivalent to Spring's `@Cacheable` behaviour.
    fn get_or_load<F>(&self, key: K, ttl: Ttl, loader: F) -> Result<V, CacheError>
    where
        F: FnOnce() -> Result<V, CacheError>,
        K: Clone,
        Self: Sized,
    {
        if let Some(v) = self.get(&key)? {
            return Ok(v);
        }
        let v = loader()?;
        self.put(key, v.clone(), ttl)?;
        Ok(v)
    }

    /// Performance statistics (hits, misses, entries).
    fn stats(&self) -> CacheStats;
}

/// Manages multiple named caches.
///
/// Equivalent to Spring's `CacheManager`.
pub trait CacheManager: Send + Sync + 'static {
    /// Key type shared by every region this manager hands out.
    type K: Send + Sync + 'static;
    /// Value type shared by every region this manager hands out.
    type V: Clone + Send + Sync + 'static;

    /// Get or create a named cache region.
    fn get_cache(&self, region: &str) -> Box<dyn Cache<Self::K, Self::V>>;

    /// List all cache region names.
    fn cache_names(&self) -> Vec<String>;
}
