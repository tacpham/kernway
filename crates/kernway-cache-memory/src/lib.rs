//! In-memory cache implementation backed by a HashMap with TTL support.
//!
//! Use for testing and development. Not suitable for production:
//! - Data is lost on restart
//! - No distributed cache (single process only)
//! - No persistence
//!
//! For production, use `kernway-cache-redis` (requires Redis server).

use kernway_cache_core::{
    cache::{Cache, CacheEntry},
    error::CacheError,
    stats::CacheStats,
    ttl::Ttl,
};
use std::{
    collections::HashMap,
    hash::Hash,
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
};

/// In-memory cache with TTL eviction.
///
/// Thread-safe via `Mutex`. Expired entries are removed lazily on access.
///
/// # Example
/// ```rust
/// use kernway_cache_memory::InMemoryCache;
/// use kernway_cache_core::{Cache, Ttl};
///
/// let cache: InMemoryCache<String, String> = InMemoryCache::new();
/// cache.put("key".to_string(), "value".to_string(), Ttl::seconds(60)).unwrap();
/// assert_eq!(cache.get(&"key".to_string()).unwrap(), Some("value".to_string()));
/// ```
pub struct InMemoryCache<K, V> {
    store: Mutex<HashMap<K, CacheEntry<V>>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl<K, V> InMemoryCache<K, V>
where
    K: Eq + Hash + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Create a new empty in-memory cache.
    pub fn new() -> Self {
        Self {
            store: Mutex::new(HashMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// Remove all expired entries from the store.
    fn purge_expired(store: &mut HashMap<K, CacheEntry<V>>) {
        store.retain(|_, entry| !entry.is_expired());
    }

    /// Lock the store, recovering from mutex poisoning instead of panicking.
    /// A cache holds no cross-call invariant that a thread panicking mid-op
    /// could corrupt, so one bad thread must not disable the whole cache
    /// (the previous `.lock().unwrap()` turned a single panic into a
    /// cache-wide poison-panic chain).
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<K, CacheEntry<V>>> {
        self.store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl<K, V> Default for InMemoryCache<K, V>
where
    K: Eq + Hash + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn default() -> Self { Self::new() }
}

impl<K, V> Cache<K, V> for InMemoryCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn get(&self, key: &K) -> Result<Option<V>, CacheError> {
        let mut store = self.lock();
        match store.get(key) {
            Some(entry) if !entry.is_expired() => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Ok(Some(entry.value.clone()))
            }
            Some(_) => {
                store.remove(key);
                self.misses.fetch_add(1, Ordering::Relaxed);
                Ok(None)
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                Ok(None)
            }
        }
    }

    fn put(&self, key: K, value: V, ttl: Ttl) -> Result<(), CacheError> {
        let mut store = self.lock();
        store.insert(key, CacheEntry::new(value, ttl));
        Ok(())
    }

    fn put_if_absent(&self, key: K, value: V, ttl: Ttl) -> Result<bool, CacheError> {
        let mut store = self.lock();
        if let Some(entry) = store.get(&key) {
            if entry.is_expired() {
                store.remove(&key);
            }
        }
        match store.entry(key) {
            std::collections::hash_map::Entry::Occupied(_) => Ok(false),
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(CacheEntry::new(value, ttl));
                Ok(true)
            }
        }
    }

    fn evict(&self, key: &K) -> Result<(), CacheError> {
        self.lock().remove(key);
        Ok(())
    }

    fn clear(&self) -> Result<(), CacheError> {
        self.lock().clear();
        Ok(())
    }

    fn contains(&self, key: &K) -> Result<bool, CacheError> {
        let mut store = self.lock();
        Ok(match store.get(key) {
            Some(e) if !e.is_expired() => true,
            Some(_) => {
                store.remove(key);
                false
            }
            None => false,
        })
    }

    fn size(&self) -> Result<usize, CacheError> {
        let mut store = self.lock();
        Self::purge_expired(&mut store);
        Ok(store.len())
    }

    fn stats(&self) -> CacheStats {
        let store = self.lock();
        CacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            entries: store.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernway_cache_core::{Cache, Ttl};
    use std::thread;
    use std::time::Duration;

    fn make_cache() -> InMemoryCache<String, String> { InMemoryCache::new() }

    #[test]
    fn put_and_get_hit() {
        let c = make_cache();
        c.put("k".to_string(), "v".to_string(), Ttl::Forever).unwrap();
        assert_eq!(c.get(&"k".to_string()).unwrap(), Some("v".to_string()));
    }

    #[test]
    fn get_miss_returns_none() {
        let c = make_cache();
        assert_eq!(c.get(&"missing".to_string()).unwrap(), None);
    }

    #[test]
    fn ttl_expiry() {
        let c = make_cache();
        c.put("k".to_string(), "v".to_string(), Ttl::Seconds(0)).unwrap();
        thread::sleep(Duration::from_millis(1));
        assert_eq!(c.get(&"k".to_string()).unwrap(), None);
    }

    #[test]
    fn evict_removes_entry() {
        let c = make_cache();
        c.put("k".to_string(), "v".to_string(), Ttl::Forever).unwrap();
        c.evict(&"k".to_string()).unwrap();
        assert_eq!(c.get(&"k".to_string()).unwrap(), None);
    }

    #[test]
    fn clear_empties_cache() {
        let c = make_cache();
        c.put("a".to_string(), "1".to_string(), Ttl::Forever).unwrap();
        c.put("b".to_string(), "2".to_string(), Ttl::Forever).unwrap();
        c.clear().unwrap();
        assert_eq!(c.size().unwrap(), 0);
    }

    #[test]
    fn put_if_absent_only_inserts_once() {
        let c = make_cache();
        let inserted = c.put_if_absent("k".to_string(), "first".to_string(), Ttl::Forever).unwrap();
        assert!(inserted);
        let inserted2 = c.put_if_absent("k".to_string(), "second".to_string(), Ttl::Forever).unwrap();
        assert!(!inserted2);
        assert_eq!(c.get(&"k".to_string()).unwrap(), Some("first".to_string()));
    }

    #[test]
    fn contains_returns_false_for_expired() {
        let c = make_cache();
        c.put("k".to_string(), "v".to_string(), Ttl::Seconds(0)).unwrap();
        thread::sleep(Duration::from_millis(1));
        assert!(!c.contains(&"k".to_string()).unwrap());
    }

    #[test]
    fn stats_tracks_hits_and_misses() {
        let c = make_cache();
        c.put("k".to_string(), "v".to_string(), Ttl::Forever).unwrap();
        c.get(&"k".to_string()).unwrap();
        c.get(&"k".to_string()).unwrap();
        c.get(&"nope".to_string()).unwrap();
        let s = c.stats();
        assert_eq!(s.hits, 2);
        assert_eq!(s.misses, 1);
    }

    #[test]
    fn hit_ratio_calculation() {
        let s = kernway_cache_core::stats::CacheStats { hits: 3, misses: 1, entries: 0 };
        assert!((s.hit_ratio() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn hit_ratio_no_requests_is_zero() {
        let s = kernway_cache_core::stats::CacheStats::default();
        assert_eq!(s.hit_ratio(), 0.0);
    }

    #[test]
    fn get_or_load_caches_result() {
        let c = make_cache();
        let mut call_count = 0usize;
        let v = c.get_or_load("k".to_string(), Ttl::Forever, || {
            call_count += 1;
            Ok("loaded".to_string())
        }).unwrap();
        assert_eq!(v, "loaded");

        let v2 = c.get_or_load("k".to_string(), Ttl::Forever, || {
            call_count += 1;
            Ok("loaded-again".to_string())
        }).unwrap();
        assert_eq!(v2, "loaded");
        assert_eq!(call_count, 1);
    }
}
