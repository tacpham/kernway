//! A **per-shard** DNS cache (positive + negative), honouring record TTL.
//!
//! Thread-per-core means each shard owns its cache via `thread_local!` — no lock,
//! no cross-thread atomics on the resolve path. The trade-off is that a popular
//! name may be resolved once per shard rather than once per process; for DNS
//! (infrequent, tiny N = core count) that is a good trade, and it keeps the
//! runtime's "no shared state on the hot path" property.
//!
//! The [`Cache`] struct takes an explicit `now: Instant` so its TTL logic is
//! deterministically unit-testable; the `thread_local` wrappers pass
//! `Instant::now()`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Positive TTLs are clamped into this range: a floor avoids re-lookup storms on
/// tiny TTLs, a ceiling avoids unbounded staleness on huge ones.
const MIN_TTL: Duration = Duration::from_secs(1);
const MAX_TTL: Duration = Duration::from_secs(3600);
/// TTL applied to a negative (NXDOMAIN) result. We don't parse the SOA MINIMUM
/// yet, so a short conservative value rather than caching a miss too long.
pub const NEGATIVE_TTL: Duration = Duration::from_secs(30);
/// Soft cap on distinct cached names; a put at/above this sweeps expired entries.
const SOFT_CAP: usize = 1024;

/// What a cache lookup found (only entries still within TTL are returned).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cached {
    /// Addresses still within their TTL.
    Hit(Vec<IpAddr>),
    /// The name is known not to exist (within the negative TTL).
    Negative,
}

enum Kind {
    Positive(Vec<IpAddr>),
    Negative,
}

struct Entry {
    kind: Kind,
    expires: Instant,
}

/// A DNS cache keyed by hostname (A records only, for now).
#[derive(Default)]
pub struct Cache {
    map: HashMap<String, Entry>,
}

impl Cache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up `host`, returning a hit only if it exists and has not expired.
    pub fn get(&self, host: &str, now: Instant) -> Option<Cached> {
        let entry = self.map.get(host)?;
        if now >= entry.expires {
            return None;
        }
        Some(match &entry.kind {
            Kind::Positive(ips) => Cached::Hit(ips.clone()),
            Kind::Negative => Cached::Negative,
        })
    }

    /// Cache `ips` for `host` for `ttl` (clamped to `[MIN_TTL, MAX_TTL]`).
    pub fn put_positive(&mut self, host: &str, ips: Vec<IpAddr>, ttl: Duration, now: Instant) {
        let ttl = ttl.clamp(MIN_TTL, MAX_TTL);
        self.maybe_sweep(now);
        self.map.insert(
            host.to_owned(),
            Entry { kind: Kind::Positive(ips), expires: now + ttl },
        );
    }

    /// Cache a negative result for `host` for `ttl`.
    pub fn put_negative(&mut self, host: &str, ttl: Duration, now: Instant) {
        self.maybe_sweep(now);
        self.map.insert(
            host.to_owned(),
            Entry { kind: Kind::Negative, expires: now + ttl },
        );
    }

    /// Drop every entry.
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// Number of entries (including any not-yet-swept expired ones).
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the cache holds no entries.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn maybe_sweep(&mut self, now: Instant) {
        if self.map.len() >= SOFT_CAP {
            self.map.retain(|_, e| now < e.expires);
        }
    }
}

thread_local! {
    static SHARD_CACHE: RefCell<Cache> = RefCell::new(Cache::new());
}

/// Look up `host` in this shard's cache.
pub fn get(host: &str) -> Option<Cached> {
    SHARD_CACHE.with(|c| c.borrow().get(host, Instant::now()))
}

/// Cache `ips` for `host` for `ttl` in this shard's cache.
pub fn put_positive(host: &str, ips: Vec<IpAddr>, ttl: Duration) {
    SHARD_CACHE.with(|c| c.borrow_mut().put_positive(host, ips, ttl, Instant::now()));
}

/// Cache a negative result for `host` for `ttl` in this shard's cache.
pub fn put_negative(host: &str, ttl: Duration) {
    SHARD_CACHE.with(|c| c.borrow_mut().put_negative(host, ttl, Instant::now()));
}

/// Clear this shard's cache — e.g. after a network change, or in tests.
pub fn clear() {
    SHARD_CACHE.with(|c| c.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    #[test]
    fn a_positive_entry_hits_before_expiry_and_misses_after() {
        let mut c = Cache::new();
        let t0 = Instant::now();
        c.put_positive("a.example", vec![ip(1)], Duration::from_secs(10), t0);

        assert_eq!(
            c.get("a.example", t0 + Duration::from_secs(5)),
            Some(Cached::Hit(vec![ip(1)]))
        );
        assert_eq!(c.get("a.example", t0 + Duration::from_secs(11)), None);
    }

    #[test]
    fn a_negative_entry_is_remembered_then_expires() {
        let mut c = Cache::new();
        let t0 = Instant::now();
        c.put_negative("gone.example", Duration::from_secs(30), t0);
        assert_eq!(c.get("gone.example", t0 + Duration::from_secs(1)), Some(Cached::Negative));
        assert_eq!(c.get("gone.example", t0 + Duration::from_secs(31)), None);
    }

    #[test]
    fn a_tiny_ttl_is_clamped_up_to_the_floor() {
        let mut c = Cache::new();
        let t0 = Instant::now();
        // TTL 0 would otherwise never hit; clamped to MIN_TTL (1s).
        c.put_positive("b.example", vec![ip(2)], Duration::from_secs(0), t0);
        assert!(c.get("b.example", t0 + Duration::from_millis(500)).is_some());
    }

    #[test]
    fn a_huge_ttl_is_clamped_down_to_the_ceiling() {
        let mut c = Cache::new();
        let t0 = Instant::now();
        c.put_positive("c.example", vec![ip(3)], Duration::from_secs(999_999), t0);
        // Past the ceiling it must be gone.
        assert!(c.get("c.example", t0 + MAX_TTL + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn an_absent_name_is_a_miss() {
        let c = Cache::new();
        assert_eq!(c.get("nope.example", Instant::now()), None);
    }

    #[test]
    fn clear_empties_the_cache() {
        let mut c = Cache::new();
        c.put_positive("d.example", vec![ip(4)], Duration::from_secs(10), Instant::now());
        assert!(!c.is_empty());
        c.clear();
        assert!(c.is_empty());
    }
}
