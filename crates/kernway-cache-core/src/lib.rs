//! # kernway-cache-core
//!
//! The cache spec: traits and value types, no backend. Roughly Spring's
//! `CacheManager` + `Cache`, minus the runtime wiring.
//!
//! ## The idea
//!
//! Same split as the rest of Kernway — this crate says what a cache *is*, and a
//! backend crate provides one. [`kernway-cache-memory`] keeps a map in-process;
//! a Redis backend implements the identical trait. Service code depends on
//! [`Cache<K, V>`], so moving from a local map to a shared Redis is a
//! dependency change, not a rewrite.
//!
//! [`kernway-cache-memory`]: https://docs.rs/kernway-cache-memory
//! [`Cache<K, V>`]: cache::Cache
//!
//! ## The flow
//!
//! ```text
//!   CacheManager::get_cache("users")  ──►  Cache<K, V>   (one named region)
//!                                              │
//!            ┌─────────────────────────────────┤
//!            ▼                                 ▼
//!         get(&k)                       get_or_load(k, ttl, loader)
//!            │                                 │
//!      hit ──┴── miss                    hit ──┴── miss
//!       │         │                       │         │
//!       ▼         ▼                       ▼         ▼
//!     Some(v)   None                    Some(v)   loader() ─► put ─► v
//! ```
//!
//! [`get_or_load`] is the one worth reaching for: it collapses the
//! check-compute-store dance into a single call, and it is what `#[cacheable]`
//! expands to once the AOP layer lands.
//!
//! [`get_or_load`]: cache::Cache::get_or_load
//!
//! ## Expiry is lazy
//!
//! A [`CacheEntry`] records when it was stored and how long it lives; nothing
//! sweeps the map on a timer. An expired entry is detected on the read that
//! touches it, which is why [`size`] reports non-expired entries rather than
//! raw occupancy — the two can differ until something reads.
//!
//! [`CacheEntry`]: cache::CacheEntry
//! [`size`]: cache::Cache::size
//!
//! ## Everything is synchronous
//!
//! Every method blocks, for the same reason the ORM spec does: a blocking call
//! belongs on a blocking pool, not smeared across a thread-per-core executor.
//! Keeping the trait sync also keeps this crate free of any runtime dependency.
//!
//! ## Failure is usually not fatal
//!
//! A cache being down means the data is slower to reach, not unreachable. The
//! usual response to a [`CacheError`] is to log it and go to the real source —
//! reserve propagating it for the rare case where the cache *is* the source.
//!
//! [`CacheError`]: error::CacheError
//!
//! ## Module map
//!
//! - [`cache`] — [`Cache`], [`CacheEntry`], [`CacheManager`]: the core contracts
//! - [`ttl`] — [`Ttl`]: how long an entry lives
//! - [`stats`] — [`CacheStats`]: hits, misses, hit ratio
//! - [`error`] — [`CacheError`]
//!
//! [`Cache`]: cache::Cache
//! [`CacheManager`]: cache::CacheManager
//! [`Ttl`]: ttl::Ttl
//! [`CacheStats`]: stats::CacheStats

/// The cache contracts: [`Cache`], entries, and region management.
///
/// [`Cache`]: cache::Cache
pub mod cache;
/// The error type shared by every backend.
pub mod error;
/// Entry lifetimes.
pub mod ttl;
/// Hit/miss accounting.
pub mod stats;

pub use cache::{Cache, CacheEntry, CacheManager};
pub use error::CacheError;
pub use ttl::Ttl;
pub use stats::CacheStats;
