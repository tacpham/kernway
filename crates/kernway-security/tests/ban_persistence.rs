//! Ban-list persistence over Redis — the "does a restart lose the bans?" test.
//!
//! `#[ignore]`d (needs a server) and only meaningful with `--features redis`. Run it
//! with a throwaway Redis:
//!
//! ```text
//! docker run -d --rm --name kw-redis -p 6380:6379 redis:alpine
//! KW_REDIS_ADDR=127.0.0.1:6380 cargo test -p kernway-security --features redis \
//!   --test ban_persistence -- --ignored
//! ```
//!
//! The flow mimics a restart: ban through one `PersistentBans`, drop it, then
//! `restore` a fresh one from the *same* Redis and confirm the ban is still there —
//! and that an unban likewise survives.

#![cfg(feature = "redis")]

use std::net::SocketAddr;

use kernway_security::{PersistentBans, RedisBanStore};
use rt_core::Executor;

fn redis_addr() -> SocketAddr {
    std::env::var("KW_REDIS_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:6379".to_string())
        .parse()
        .expect("KW_REDIS_ADDR must be host:port")
}

fn ip(s: &str) -> std::net::IpAddr {
    s.parse().unwrap()
}

#[test]
#[ignore = "needs a Redis server (set KW_REDIS_ADDR or run one on 127.0.0.1:6379)"]
fn bans_survive_a_restart() {
    Executor::new()
        .unwrap()
        .block_on(async {
            let addr = redis_addr();

            // Clean slate in case a prior aborted run left state.
            let seed = PersistentBans::restore(RedisBanStore::new(addr)).await.unwrap();
            seed.unban_ip(ip("203.0.113.7")).await.unwrap();
            seed.unban_subnet("198.51.100.0/24").await.unwrap();
            seed.unban_user_agent_containing("evilbot").await.unwrap();

            // "Instance A": ban an IP, a subnet, and a UA phrase.
            let a = PersistentBans::restore(RedisBanStore::new(addr)).await.unwrap();
            a.ban_ip(ip("203.0.113.7")).await.unwrap();
            a.ban_subnet("198.51.100.0/24").await.unwrap();
            a.ban_user_agent_containing("EvilBot").await.unwrap();
            assert!(a.bans().is_banned(Some(ip("203.0.113.7")), None), "banned in memory now");
            drop(a); // the process "restarts" — in-memory state is gone.

            // "Instance B": a fresh restore from the same Redis must see every ban.
            let b = PersistentBans::restore(RedisBanStore::new(addr)).await.unwrap();
            let bans = b.bans();
            assert!(bans.is_banned(Some(ip("203.0.113.7")), None), "IP ban restored");
            assert!(bans.is_banned(Some(ip("198.51.100.42")), None), "subnet ban restored");
            assert!(bans.is_banned(None, Some("Some EVILBOT/2.0")), "UA-contains ban restored (case-insensitive)");
            assert!(!bans.is_banned(Some(ip("8.8.8.8")), Some("Mozilla/5.0")), "a clean request is not banned");

            // An unban also persists across a restart.
            b.unban_ip(ip("203.0.113.7")).await.unwrap();
            drop(b);
            let c = PersistentBans::restore(RedisBanStore::new(addr)).await.unwrap();
            assert!(!c.bans().is_banned(Some(ip("203.0.113.7")), None), "unban restored");
            assert!(c.bans().is_banned(Some(ip("198.51.100.42")), None), "other bans still there");

            // Tidy up so a re-run starts clean.
            c.unban_subnet("198.51.100.0/24").await.unwrap();
            c.unban_user_agent_containing("evilbot").await.unwrap();
        })
        .unwrap();
}
