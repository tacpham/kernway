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
    singleflight::{AsyncSingleFlight, SingleFlight},
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

thread_local! {
    static JITTER_RNG: std::cell::Cell<u64> = std::cell::Cell::new(jitter_seed());
}

/// A distinct-ish per-thread seed without pulling in a `rand` dependency —
/// jitter only needs spread, not unpredictability.
fn jitter_seed() -> u64 {
    use std::hash::{Hash, Hasher};
    static CTR: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let c = CTR.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    (c ^ h.finish()) | 1 // non-zero: xorshift stays stuck at 0
}

/// xorshift64 — cheap, good enough to spread TTL expiry times.
fn jitter_rand() -> u64 {
    JITTER_RNG.with(|cell| {
        let mut x = cell.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        cell.set(x);
        x
    })
}

/// Upper bound on cached "not-found" entries, so a flood of unique missing keys
/// (cache penetration) cannot grow the negative set without limit.
const NEGATIVE_CAPACITY: usize = 10_000;

/// Trim a TTL-keyed store to `cap`: drop expired entries first, then evict the
/// least-recently-used (oldest `last_access`) until within the cap. Shared by the
/// value store and the negative store.
fn evict_to_cap<K2, V2>(store: &mut HashMap<K2, CacheEntry<V2>>, cap: usize)
where
    K2: Eq + Hash + Clone,
{
    if store.len() <= cap {
        return;
    }
    store.retain(|_, entry| !entry.is_expired());
    while store.len() > cap {
        let Some(victim) = store
            .iter()
            .min_by_key(|(_, entry)| entry.last_access)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        store.remove(&victim);
    }
}

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
    /// Max live entries; `None` = unbounded. Over the cap, `put` evicts (expired
    /// first, then arbitrary) so the cache cannot grow without limit.
    capacity: Option<usize>,
    hits: AtomicU64,
    misses: AtomicU64,
    /// Per-key coalescing so a stampede of concurrent misses runs the loader once.
    flight: SingleFlight<K>,
    /// Async counterpart of `flight`, for [`AsyncCache::get_or_load`] with an
    /// async loader — waiters await instead of blocking.
    async_flight: AsyncSingleFlight<K>,
    /// Fractional TTL jitter in `[0, 1]`; `0` = off. A non-zero value spreads each
    /// entry's expiry by ±this fraction so a batch warmed together does not all
    /// expire in the same instant (cache avalanche).
    jitter: f64,
    /// Keys known to be absent in the source, cached with a short TTL so a repeated
    /// lookup of a missing key (cache penetration) does not hit the source every
    /// time. Bounded by [`NEGATIVE_CAPACITY`]. Only consulted by
    /// [`get_or_load_optional`](Self::get_or_load_optional).
    negatives: Mutex<HashMap<K, CacheEntry<()>>>,
}

impl<K, V> InMemoryCache<K, V>
where
    K: Eq + Hash + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Create a new empty, **unbounded** in-memory cache (grows until entries
    /// expire). Prefer [`with_capacity`](Self::with_capacity) for large values
    /// (e.g. cached images) so memory stays bounded.
    pub fn new() -> Self {
        Self {
            store: Mutex::new(HashMap::new()),
            capacity: None,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            flight: SingleFlight::new(),
            async_flight: AsyncSingleFlight::new(),
            jitter: 0.0,
            negatives: Mutex::new(HashMap::new()),
        }
    }

    /// A cache bounded to at most `max_entries` live entries. When full, `put`
    /// drops expired entries first, then evicts arbitrary ones to stay within the
    /// cap — so the memory footprint is bounded (`max_entries` × value size).
    pub fn with_capacity(max_entries: usize) -> Self {
        Self {
            store: Mutex::new(HashMap::new()),
            capacity: Some(max_entries.max(1)),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            flight: SingleFlight::new(),
            async_flight: AsyncSingleFlight::new(),
            jitter: 0.0,
            negatives: Mutex::new(HashMap::new()),
        }
    }

    /// Spread entry expiry by ±`fraction` (clamped to `[0, 1]`) so a batch of keys
    /// warmed together does not expire in the same instant — the standard guard
    /// against a cache avalanche. `0` disables it. Builder-style:
    /// `InMemoryCache::with_capacity(n).with_jitter(0.1)`.
    #[must_use]
    pub fn with_jitter(mut self, fraction: f64) -> Self {
        self.jitter = fraction.clamp(0.0, 1.0);
        self
    }

    /// Apply TTL jitter to a finite TTL; `Forever` and `0` are returned unchanged.
    fn jittered(&self, ttl: Ttl) -> Ttl {
        if self.jitter <= 0.0 {
            return ttl;
        }
        match ttl {
            Ttl::Seconds(s) if s > 0 => {
                // A factor in [1-jitter, 1+jitter].
                let unit = (jitter_rand() as f64) / (u64::MAX as f64); // [0, 1]
                let factor = 1.0 + (unit * 2.0 - 1.0) * self.jitter;
                let secs = (s as f64 * factor).round().max(1.0) as u64;
                Ttl::Seconds(secs)
            }
            other => other,
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

    /// The read used by `get_or_load`'s single-flight: returns the fresh value
    /// (counting a hit) or `None`. Unlike [`get`](Cache::get), a miss is **not**
    /// counted here — the coalescing path peeks more than once per logical
    /// request, so counting every peek as a miss would inflate the miss rate.
    fn peek_hit(&self, key: &K) -> Option<V> {
        let mut store = self.lock();
        match store.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch(); // recency for LRU
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(entry.value.clone())
            }
            Some(_) => {
                store.remove(key);
                None
            }
            None => None,
        }
    }

    /// Lock the negative set, recovering from poisoning (same rationale as `lock`).
    fn lock_negatives(&self) -> std::sync::MutexGuard<'_, HashMap<K, CacheEntry<()>>> {
        self.negatives
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Single-flight peek for [`get_or_load_optional`](Self::get_or_load_optional):
    /// `Some(Some(v))` on a positive hit, `Some(None)` on a cached "not-found",
    /// `None` on a real miss. Positive store is checked first, so a present value
    /// always shadows a lingering negative.
    fn peek_optional(&self, key: &K) -> Option<Option<V>> {
        {
            let mut store = self.lock();
            match store.get_mut(key) {
                Some(entry) if !entry.is_expired() => {
                    entry.touch();
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return Some(Some(entry.value.clone()));
                }
                Some(_) => {
                    store.remove(key);
                }
                None => {}
            }
        }
        let mut neg = self.lock_negatives();
        match neg.get(key) {
            Some(entry) if !entry.is_expired() => {
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(None)
            }
            Some(_) => {
                neg.remove(key);
                None
            }
            None => None,
        }
    }

    /// Record that `key` is absent in the source, kept for `negative_ttl` and
    /// bounded by [`NEGATIVE_CAPACITY`].
    fn put_negative(&self, key: K, negative_ttl: Ttl)
    where
        K: Clone,
    {
        let ttl = self.jittered(negative_ttl);
        let mut neg = self.lock_negatives();
        neg.insert(key, CacheEntry::new((), ttl));
        evict_to_cap(&mut neg, NEGATIVE_CAPACITY);
    }

    /// Get-or-compute that also caches **absence** — the guard against cache
    /// penetration (repeated lookups of non-existent keys hammering the source).
    /// `loader` returns `Ok(None)` for "not found"; that is remembered for
    /// `negative_ttl` so the next lookup is a hit, not another source query.
    /// Present values are cached for `ttl`. Single-flighted like
    /// [`get_or_load`](Cache::get_or_load), and the negative set is bounded by
    /// [`NEGATIVE_CAPACITY`] so a flood of unique missing keys cannot grow it.
    pub fn get_or_load_optional<F>(
        &self,
        key: K,
        ttl: Ttl,
        negative_ttl: Ttl,
        loader: F,
    ) -> Result<Option<V>, CacheError>
    where
        F: FnOnce() -> Result<Option<V>, CacheError>,
        K: Clone,
    {
        self.flight.run(
            &key,
            || self.peek_optional(&key),
            || match loader()? {
                Some(value) => {
                    Cache::put(self, key.clone(), value.clone(), ttl)?;
                    Ok(Some(value))
                }
                None => {
                    self.put_negative(key.clone(), negative_ttl);
                    Ok(None)
                }
            },
        )
    }
}

impl<K, V> Default for InMemoryCache<K, V>
where
    K: Eq + Hash + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> Cache<K, V> for InMemoryCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn get(&self, key: &K) -> Result<Option<V>, CacheError> {
        let mut store = self.lock();
        match store.get_mut(key) {
            Some(entry) if !entry.is_expired() => {
                entry.touch(); // recency for LRU
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
        let ttl = self.jittered(ttl);
        let mut store = self.lock();
        store.insert(key, CacheEntry::new(value, ttl));
        // Enforce the capacity bound: drop expired entries first, then evict
        // arbitrary entries until within the cap — so memory cannot grow without
        // limit (the [`with_capacity`] contract).
        if let Some(cap) = self.capacity {
            evict_to_cap(&mut store, cap);
        }
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

    /// Get-or-compute with **single-flight**: when several callers miss the same
    /// key at once, only one runs `loader` while the rest wait and then read the
    /// value it stored — a cache stampede collapses from N loads to 1. (Per
    /// process; coalescing across nodes needs a distributed lease.)
    fn get_or_load<F>(&self, key: K, ttl: Ttl, loader: F) -> Result<V, CacheError>
    where
        F: FnOnce() -> Result<V, CacheError>,
        K: Clone,
        Self: Sized,
    {
        self.flight.run(
            &key,
            || self.peek_hit(&key),
            || {
                let value = loader()?;
                Cache::put(self, key.clone(), value.clone(), ttl)?;
                Ok(value)
            },
        )
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

/// The async-top impl lives in its own module so `AsyncCache` is not brought into
/// the crate's scope — otherwise every `cache.get()`/`cache.put()` in sync code
/// (and tests) would be ambiguous between the two traits.
mod async_cache_impl {
    use super::*;
    use kernway_cache_core::async_cache::{AsyncCache, BoxFuture};

/// Async-top view of the same cache: every method wraps the synchronous one in a
/// future that is `Ready` on first poll (no suspension), so the in-memory cache
/// is near-free under the async contract — while still shared (`Send + Sync`).
/// `get_or_load` coalesces concurrent misses of an **async** loader via
/// [`AsyncSingleFlight`], so waiters await rather than block a reactor shard.
impl<K, V> AsyncCache<K, V> for InMemoryCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn get<'a>(&'a self, key: &'a K) -> BoxFuture<'a, Result<Option<V>, CacheError>> {
        Box::pin(async move { Cache::get(self, key) })
    }

    fn put<'a>(&'a self, key: K, value: V, ttl: Ttl) -> BoxFuture<'a, Result<(), CacheError>> {
        Box::pin(async move { Cache::put(self, key, value, ttl) })
    }

    fn put_if_absent<'a>(&'a self, key: K, value: V, ttl: Ttl) -> BoxFuture<'a, Result<bool, CacheError>> {
        Box::pin(async move { Cache::put_if_absent(self, key, value, ttl) })
    }

    fn evict<'a>(&'a self, key: &'a K) -> BoxFuture<'a, Result<(), CacheError>> {
        Box::pin(async move { Cache::evict(self, key) })
    }

    fn clear(&self) -> BoxFuture<'_, Result<(), CacheError>> {
        Box::pin(async move { Cache::clear(self) })
    }

    fn contains<'a>(&'a self, key: &'a K) -> BoxFuture<'a, Result<bool, CacheError>> {
        Box::pin(async move { Cache::contains(self, key) })
    }

    fn size(&self) -> BoxFuture<'_, Result<usize, CacheError>> {
        Box::pin(async move { Cache::size(self) })
    }

    fn stats(&self) -> CacheStats {
        Cache::stats(self)
    }

    fn get_or_load<'a, F, Fut>(&'a self, key: K, ttl: Ttl, loader: F) -> BoxFuture<'a, Result<V, CacheError>>
    where
        F: FnOnce() -> Fut + 'a,
        Fut: std::future::Future<Output = Result<V, CacheError>> + 'a,
        K: Clone + 'a,
        V: 'a,
        Self: Sized,
    {
        Box::pin(async move {
            let key_compute = key.clone();
            self.async_flight
                .run(
                    &key,
                    || Cache::get(self, &key).ok().flatten(),
                    move || async move {
                        let value = loader().await?;
                        Cache::put(self, key_compute, value.clone(), ttl)?;
                        Ok(value)
                    },
                )
                .await
        })
    }
}
} // mod async_cache_impl

#[cfg(test)]
mod tests {
    use super::*;
    use kernway_cache_core::{Cache, Ttl};
    use std::thread;
    use std::time::Duration;

    fn make_cache() -> InMemoryCache<String, String> {
        InMemoryCache::new()
    }

    #[test]
    fn put_and_get_hit() {
        let c = make_cache();
        c.put("k".to_string(), "v".to_string(), Ttl::Forever)
            .unwrap();
        assert_eq!(c.get(&"k".to_string()).unwrap(), Some("v".to_string()));
    }

    #[test]
    fn get_miss_returns_none() {
        let c = make_cache();
        assert_eq!(c.get(&"missing".to_string()).unwrap(), None);
    }

    #[test]
    fn with_capacity_bounds_the_store() {
        let c: InMemoryCache<u32, u32> = InMemoryCache::with_capacity(3);
        for i in 0..100 {
            c.put(i, i, Ttl::Forever).unwrap();
        }
        // The cache never holds more than the cap, however many were inserted.
        let mut live = 0;
        for i in 0..100 {
            if c.get(&i).unwrap().is_some() {
                live += 1;
            }
        }
        assert!(live <= 3, "held {live} entries, cap was 3");
    }

    #[test]
    fn eviction_is_least_recently_used() {
        let c: InMemoryCache<u32, u32> = InMemoryCache::with_capacity(2);
        c.put(1, 1, Ttl::Forever).unwrap();
        thread::sleep(Duration::from_millis(2));
        c.put(2, 2, Ttl::Forever).unwrap();
        thread::sleep(Duration::from_millis(2));
        // Touch key 1 so key 2 is now the least-recently-used.
        assert_eq!(c.get(&1).unwrap(), Some(1));
        thread::sleep(Duration::from_millis(2));
        // Over capacity → the LRU entry (key 2) is evicted, not key 1.
        c.put(3, 3, Ttl::Forever).unwrap();
        assert_eq!(c.get(&2).unwrap(), None, "least-recently-used key evicted");
        assert_eq!(c.get(&1).unwrap(), Some(1), "recently-used key kept");
        assert_eq!(c.get(&3).unwrap(), Some(3), "new key present");
    }

    #[test]
    fn ttl_expiry() {
        let c = make_cache();
        c.put("k".to_string(), "v".to_string(), Ttl::Seconds(0))
            .unwrap();
        thread::sleep(Duration::from_millis(1));
        assert_eq!(c.get(&"k".to_string()).unwrap(), None);
    }

    #[test]
    fn evict_removes_entry() {
        let c = make_cache();
        c.put("k".to_string(), "v".to_string(), Ttl::Forever)
            .unwrap();
        c.evict(&"k".to_string()).unwrap();
        assert_eq!(c.get(&"k".to_string()).unwrap(), None);
    }

    #[test]
    fn clear_empties_cache() {
        let c = make_cache();
        c.put("a".to_string(), "1".to_string(), Ttl::Forever)
            .unwrap();
        c.put("b".to_string(), "2".to_string(), Ttl::Forever)
            .unwrap();
        c.clear().unwrap();
        assert_eq!(c.size().unwrap(), 0);
    }

    #[test]
    fn put_if_absent_only_inserts_once() {
        let c = make_cache();
        let inserted = c
            .put_if_absent("k".to_string(), "first".to_string(), Ttl::Forever)
            .unwrap();
        assert!(inserted);
        let inserted2 = c
            .put_if_absent("k".to_string(), "second".to_string(), Ttl::Forever)
            .unwrap();
        assert!(!inserted2);
        assert_eq!(c.get(&"k".to_string()).unwrap(), Some("first".to_string()));
    }

    #[test]
    fn contains_returns_false_for_expired() {
        let c = make_cache();
        c.put("k".to_string(), "v".to_string(), Ttl::Seconds(0))
            .unwrap();
        thread::sleep(Duration::from_millis(1));
        assert!(!c.contains(&"k".to_string()).unwrap());
    }

    #[test]
    fn stats_tracks_hits_and_misses() {
        let c = make_cache();
        c.put("k".to_string(), "v".to_string(), Ttl::Forever)
            .unwrap();
        c.get(&"k".to_string()).unwrap();
        c.get(&"k".to_string()).unwrap();
        c.get(&"nope".to_string()).unwrap();
        let s = c.stats();
        assert_eq!(s.hits, 2);
        assert_eq!(s.misses, 1);
    }

    #[test]
    fn hit_ratio_calculation() {
        let s = kernway_cache_core::stats::CacheStats {
            hits: 3,
            misses: 1,
            entries: 0,
        };
        assert!((s.hit_ratio() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn hit_ratio_no_requests_is_zero() {
        let s = kernway_cache_core::stats::CacheStats::default();
        assert_eq!(s.hit_ratio(), 0.0);
    }

    #[test]
    fn get_or_load_coalesces_concurrent_misses() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};

        let c: Arc<InMemoryCache<String, String>> = Arc::new(InMemoryCache::new());
        let loads = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(8));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (c, loads, barrier) =
                    (Arc::clone(&c), Arc::clone(&loads), Arc::clone(&barrier));
                thread::spawn(move || {
                    barrier.wait(); // all miss at the same instant
                    c.get_or_load("k".to_string(), Ttl::Forever, || {
                        loads.fetch_add(1, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(20));
                        Ok("v".to_string())
                    })
                    .unwrap()
                })
            })
            .collect();

        for h in handles {
            assert_eq!(h.join().unwrap(), "v");
        }
        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "single-flight must run the loader exactly once"
        );
    }

    #[test]
    fn get_or_load_caches_result() {
        let c = make_cache();
        let mut call_count = 0usize;
        let v = c
            .get_or_load("k".to_string(), Ttl::Forever, || {
                call_count += 1;
                Ok("loaded".to_string())
            })
            .unwrap();
        assert_eq!(v, "loaded");

        let v2 = c
            .get_or_load("k".to_string(), Ttl::Forever, || {
                call_count += 1;
                Ok("loaded-again".to_string())
            })
            .unwrap();
        assert_eq!(v2, "loaded");
        assert_eq!(call_count, 1);
    }

    #[test]
    fn negative_caching_remembers_absent_keys() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let c: InMemoryCache<String, String> = InMemoryCache::new();
        let loads = AtomicUsize::new(0);

        // First lookup of a missing key: loader runs, returns None → cached negative.
        let r1 = c
            .get_or_load_optional("ghost".into(), Ttl::Forever, Ttl::seconds(60), || {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok(None)
            })
            .unwrap();
        assert_eq!(r1, None);

        // Second lookup: the cached negative answers; loader must NOT run.
        let r2 = c
            .get_or_load_optional("ghost".into(), Ttl::Forever, Ttl::seconds(60), || {
                loads.fetch_add(1, Ordering::SeqCst);
                Ok(Some("surprise".into()))
            })
            .unwrap();
        assert_eq!(r2, None, "negative entry hides the loader");
        assert_eq!(loads.load(Ordering::SeqCst), 1, "loader runs once, not per lookup");
    }

    #[test]
    fn optional_caches_present_values_in_the_main_store() {
        let c: InMemoryCache<String, String> = InMemoryCache::new();
        let r = c
            .get_or_load_optional("k".into(), Ttl::Forever, Ttl::seconds(60), || {
                Ok(Some("v".into()))
            })
            .unwrap();
        assert_eq!(r, Some("v".to_string()));
        // A present value lands in the normal store, so plain `get` sees it too.
        assert_eq!(c.get(&"k".to_string()).unwrap(), Some("v".to_string()));
    }

    #[test]
    fn jitter_off_by_default_leaves_ttl_exact() {
        let c: InMemoryCache<String, String> = InMemoryCache::new();
        assert!(matches!(c.jittered(Ttl::Seconds(100)), Ttl::Seconds(100)));
        assert!(matches!(c.jittered(Ttl::Forever), Ttl::Forever));
    }

    #[test]
    fn jitter_spreads_ttl_within_bounds() {
        let c: InMemoryCache<String, String> = InMemoryCache::new().with_jitter(0.2);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            match c.jittered(Ttl::Seconds(100)) {
                Ttl::Seconds(s) => {
                    assert!((80..=120).contains(&s), "ttl {s} outside ±20% of 100");
                    seen.insert(s);
                }
                _ => panic!("expected Seconds"),
            }
        }
        assert!(seen.len() > 1, "jitter should vary ttls, got {}", seen.len());
    }

    // --- async-top (AsyncCache) ---

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        rt_core::Executor::new().unwrap().block_on(f).unwrap()
    }

    #[test]
    fn async_get_put_roundtrip() {
        use kernway_cache_core::AsyncCache;
        let cache: InMemoryCache<String, u64> = InMemoryCache::new();
        block_on(async {
            AsyncCache::put(&cache, "k".into(), 42, Ttl::Forever).await.unwrap();
            assert_eq!(AsyncCache::get(&cache, &"k".to_string()).await.unwrap(), Some(42));
            assert!(AsyncCache::contains(&cache, &"k".to_string()).await.unwrap());
            assert_eq!(AsyncCache::size(&cache).await.unwrap(), 1);
            AsyncCache::evict(&cache, &"k".to_string()).await.unwrap();
            assert_eq!(AsyncCache::get(&cache, &"k".to_string()).await.unwrap(), None);
        });
    }

    #[test]
    fn async_get_or_load_coalesces_across_threads() {
        use kernway_cache_core::AsyncCache;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let cache: Arc<InMemoryCache<String, u64>> = Arc::new(InMemoryCache::new());
        let loads = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(std::sync::Barrier::new(8));

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let (cache, loads, barrier) =
                    (Arc::clone(&cache), Arc::clone(&loads), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    block_on(async move {
                        barrier.wait();
                        AsyncCache::get_or_load(&*cache, "k".to_string(), Ttl::Forever, move || {
                            let loads = Arc::clone(&loads);
                            async move {
                                loads.fetch_add(1, Ordering::SeqCst);
                                rt_core::sleep(Duration::from_millis(30)).await;
                                Ok(42u64)
                            }
                        })
                        .await
                        .unwrap()
                    })
                })
            })
            .collect();

        for h in handles {
            assert_eq!(h.join().unwrap(), 42);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1, "async loader runs once across threads");
    }
}
