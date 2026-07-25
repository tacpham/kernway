//! Round-trips against a real Redis. Ignored by default (needs a server); run with:
//!
//! ```text
//! cargo test -p kernway-redis --test integration -- --ignored
//! ```
//!
//! It touches only `kw:selftest:*` keys, with short TTLs, and deletes them on the
//! way out — safe to point at any Redis. Set `KW_REDIS_ADDR` to override the
//! default `127.0.0.1:6379`.

use rt_core::Executor;

use kernway_redis::Pool;

fn addr() -> std::net::SocketAddr {
    std::env::var("KW_REDIS_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:6379".to_string())
        .parse()
        .expect("KW_REDIS_ADDR must be host:port")
}

#[test]
#[ignore = "needs a Redis server (set KW_REDIS_ADDR or run one on 127.0.0.1:6379)"]
fn set_get_del_round_trip() {
    let executor = Executor::new().unwrap();
    executor
        .block_on(async {
            let pool = Pool::new(addr());
            let key = "kw:selftest:round-trip";

            // Clean slate, then set with a 60s TTL and read it back.
            pool.with(async |c| c.del(&[key]).await).await.unwrap();
            pool.with(async |c| c.set_ex(key, b"hello-kernway", 60).await).await.unwrap();
            let got = pool.with(async |c| c.get(key).await).await.unwrap();
            assert_eq!(got, Some(b"hello-kernway".to_vec()));

            // A missing key is None, and DEL reports the one removal.
            let removed = pool.with(async |c| c.del(&[key]).await).await.unwrap();
            assert_eq!(removed, 1);
            let gone = pool.with(async |c| c.get(key).await).await.unwrap();
            assert_eq!(gone, None);
        })
        .unwrap();
}

#[test]
#[ignore = "needs a Redis server (set KW_REDIS_ADDR or run one on 127.0.0.1:6379)"]
fn set_membership_round_trip() {
    let executor = Executor::new().unwrap();
    executor
        .block_on(async {
            let pool = Pool::new(addr());
            let set = "kw:selftest:members";

            pool.with(async |c| c.del(&[set]).await).await.unwrap();
            pool.with(async |c| c.sadd(set, "a").await).await.unwrap();
            pool.with(async |c| c.sadd(set, "b").await).await.unwrap();
            pool.with(async |c| c.srem(set, "a").await).await.unwrap();

            let members = pool.with(async |c| c.smembers(set).await).await.unwrap();
            assert_eq!(members, vec!["b".to_string()]);

            pool.with(async |c| c.del(&[set]).await).await.unwrap();
        })
        .unwrap();
}
