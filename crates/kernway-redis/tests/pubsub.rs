//! Integration test for Redis-backed pub/sub, against a real Redis with auth.
//!
//! Runs only when `KERNWAY_REDIS_ADDR` is set; otherwise it skips, so the normal
//! `cargo test` stays green without a server. The whole test runs inside one
//! `Executor::block_on` (connections are pinned to the reactor that opened them).
//!
//! ```bash
//! docker run -d --name kernway-redis -p 6380:6379 redis:7-alpine \
//!   redis-server --requirepass secret
//! docker exec kernway-redis redis-cli -a secret \
//!   ACL SETUSER kernway on '>kernway_pw' '~*' '&*' '+@all'
//! KERNWAY_REDIS_ADDR=127.0.0.1:6380 KERNWAY_REDIS_USER=kernway \
//!   KERNWAY_REDIS_PASS=kernway_pw cargo test -p kernway-redis --test pubsub -- --nocapture
//! ```

use kernway_cache_core::pubsub::{Publisher, Subscriber};
use kernway_redis::{RedisConfig, RedisPubSub};
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
fn publish_subscribe_roundtrip_with_auth() {
    let Some(cfg) = config() else {
        eprintln!("skip publish_subscribe_roundtrip_with_auth: KERNWAY_REDIS_ADDR not set");
        return;
    };
    run(async move {
        let bus = RedisPubSub::new(cfg);

        // Subscribe first (pub/sub is fire-and-forget — a message published
        // before the subscription exists is lost).
        let mut sub = bus.subscribe(&["pos:location:42"]).await.expect("subscribe (auth ok)");

        let received = bus
            .publish("pos:location:42", b"checkin:Jennifer")
            .await
            .expect("publish");
        assert!(received >= 1, "the live subscriber should receive it");

        let msg = sub.recv().await.expect("recv");
        assert_eq!(msg.channel, "pos:location:42");
        assert_eq!(msg.payload, b"checkin:Jennifer");

        // A second message on the same subscription.
        bus.publish("pos:location:42", b"turn:assigned").await.unwrap();
        let msg2 = sub.recv().await.expect("recv 2");
        assert_eq!(msg2.payload, b"turn:assigned");
    });
}

#[test]
fn wrong_password_is_rejected() {
    let Some(mut cfg) = config() else {
        eprintln!("skip wrong_password_is_rejected: KERNWAY_REDIS_ADDR not set");
        return;
    };
    cfg.password = Some("definitely-wrong".into());
    run(async move {
        let bus = RedisPubSub::new(cfg);
        // Auth happens on first use; it must fail rather than silently proceed.
        let result = bus.publish("pos:location:42", b"x").await;
        assert!(result.is_err(), "a wrong password must be rejected");
    });
}
