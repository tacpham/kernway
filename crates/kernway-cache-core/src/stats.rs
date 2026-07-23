/// Cache performance statistics.
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    /// Lookups answered from the cache.
    pub hits: u64,
    /// Lookups that found nothing, or found an expired entry.
    pub misses: u64,
    /// Non-expired entries currently held.
    pub entries: usize,
}

impl CacheStats {
    /// Cache hit ratio (0.0 – 1.0). Returns 0.0 if no requests yet.
    pub fn hit_ratio(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 { 0.0 } else { self.hits as f64 / total as f64 }
    }
}
