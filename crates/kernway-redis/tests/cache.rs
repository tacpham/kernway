//! Integration test for the Redis-backed `AsyncCache`, against a real Redis with
//! auth. Skips unless `KERNWAY_REDIS_ADDR` is set.
//!
//! ```bash
//! KERNWAY_REDIS_ADDR=127.0.0.1:6380 KERNWAY_REDIS_USER=kernway \
//!   KERNWAY_REDIS_PASS=kernway_pw cargo test -p kernway-redis --test cache -- --nocapture
//! ```

use kernway_cache_core::{AsyncCache, Ttl};
use kernway_redis::{RedisCache, RedisConfig};
use std::future::Future;

fn run<F: Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

fn config() -> Option<RedisConfig> {
    let addr = std::env::var("KERNWAY_REDIS_ADDR").ok()?.parse().ok()?;
    Some(RedisConfig {
        addr,
        user: std::env::var("KERNWAY_REDIS_USER").ok(),
        password: std::env::var("KERNWAY_REDIS_PASS").ok(),
    })
}

#[test]
fn redis_cache_roundtrip_with_auth() {
    let Some(cfg) = config() else {
        eprintln!("skip redis_cache_roundtrip_with_auth: KERNWAY_REDIS_ADDR not set");
        return;
    };
    run(async move {
        let cache = RedisCache::new(cfg);
        let key = "cache:test:k1".to_string();

        // Clean slate for this key (unique key, no FLUSHDB so other data is safe).
        AsyncCache::evict(&cache, &key).await.unwrap();
        assert_eq!(AsyncCache::get(&cache, &key).await.unwrap(), None);
        assert!(!AsyncCache::contains(&cache, &key).await.unwrap());

        // put + get (bytes round-trip)
        AsyncCache::put(&cache, key.clone(), b"hello".to_vec(), Ttl::Forever).await.unwrap();
        assert_eq!(AsyncCache::get(&cache, &key).await.unwrap(), Some(b"hello".to_vec()));
        assert!(AsyncCache::contains(&cache, &key).await.unwrap());

        // put_if_absent on an existing key → not stored, value unchanged
        let stored = AsyncCache::put_if_absent(&cache, key.clone(), b"other".to_vec(), Ttl::Forever)
            .await
            .unwrap();
        assert!(!stored, "existing key must not be overwritten");
        assert_eq!(AsyncCache::get(&cache, &key).await.unwrap(), Some(b"hello".to_vec()));

        // evict → gone
        AsyncCache::evict(&cache, &key).await.unwrap();
        assert_eq!(AsyncCache::get(&cache, &key).await.unwrap(), None);

        // put_if_absent on an absent key → stored, with a TTL
        let stored2 = AsyncCache::put_if_absent(&cache, key.clone(), b"new".to_vec(), Ttl::seconds(60))
            .await
            .unwrap();
        assert!(stored2, "absent key must be stored");
        assert_eq!(AsyncCache::get(&cache, &key).await.unwrap(), Some(b"new".to_vec()));

        // get_or_load (default, no coalescing): miss → load → store → hit
        AsyncCache::evict(&cache, &key).await.unwrap();
        let loaded = AsyncCache::get_or_load(&cache, key.clone(), Ttl::seconds(60), || async {
            Ok::<_, kernway_cache_core::CacheError>(b"loaded".to_vec())
        })
        .await
        .unwrap();
        assert_eq!(loaded, b"loaded".to_vec());
        assert_eq!(AsyncCache::get(&cache, &key).await.unwrap(), Some(b"loaded".to_vec()));

        AsyncCache::evict(&cache, &key).await.unwrap();
    });
}
