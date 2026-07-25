//! The same login flow, run against each `SessionStore` backend — so it is visible
//! that swapping the backend (a Cargo feature) does not change the behaviour.
//!
//! - [`login_flow_in_memory`] runs on every `cargo test` — the default backend.
//! - [`login_flow_redis`] and [`logout_user_over_redis`] run only with
//!   `--features redis` and are `#[ignore]`d (they need a server). Run them with a
//!   throwaway Redis:
//!
//!   ```text
//!   docker run -d --rm --name kw-redis -p 6380:6379 redis:alpine
//!   KW_REDIS_ADDR=127.0.0.1:6380 cargo test -p kernway-security --features redis \
//!     --test login_backends -- --ignored
//!   ```
//!
//! Both drive the async `SessionManager` with `rt-core`'s executor: over the
//! in-memory store every future is immediately ready; over Redis they await the
//! network. Same assertions either way.

use rt_core::Executor;

use kernway_security::session::{MemorySessionStore, SessionConfig, SessionManager};

/// Log in, authenticate the issued token, then log out and confirm revocation —
/// the whole point of the hybrid token + registry, independent of the backend.
async fn login_flow(mgr: &SessionManager, user: &str) {
    let token = mgr.login(user, vec!["ADMIN".to_string()], "device").await.unwrap();

    // The signed token authenticates against the stored session.
    let ctx = mgr.authenticate(Some(&token)).await;
    assert!(ctx.is_authenticated(), "a stored session must authenticate");
    assert_eq!(ctx.principal(), Some(user));
    assert!(ctx.has_role("ADMIN"), "roles come from the token");

    // The registry knows this one session.
    assert_eq!(mgr.sessions_of(user).await.unwrap().len(), 1);

    // Logout revokes it in the registry: the same token is now anonymous.
    mgr.logout_token(&token).await.unwrap();
    assert!(!mgr.authenticate(Some(&token)).await.is_authenticated(), "revoked → anonymous");
    assert_eq!(mgr.sessions_of(user).await.unwrap().len(), 0);
}

#[test]
fn login_flow_in_memory() {
    let mgr = SessionManager::new(
        Box::new(MemorySessionStore::new()),
        "in-memory-test-key",
        SessionConfig::default(),
    );
    Executor::new().unwrap().block_on(login_flow(&mgr, "mem-alice")).unwrap();
}

#[cfg(feature = "redis")]
mod redis {
    use super::*;
    use std::time::Duration;

    use kernway_security::RedisSessionStore;

    fn redis_manager() -> SessionManager {
        let addr = std::env::var("KW_REDIS_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:6379".to_string())
            .parse()
            .expect("KW_REDIS_ADDR must be host:port");
        let store = RedisSessionStore::new(addr, Duration::from_secs(3600));
        SessionManager::new(Box::new(store), "redis-test-key", SessionConfig::default())
    }

    #[test]
    #[ignore = "needs a Redis server (set KW_REDIS_ADDR or run one on 127.0.0.1:6379)"]
    fn login_flow_redis() {
        let mgr = redis_manager();
        Executor::new()
            .unwrap()
            .block_on(async {
                // Clean slate in case a prior aborted run left state.
                mgr.logout_user("redis-alice").await.unwrap();
                login_flow(&mgr, "redis-alice").await;
            })
            .unwrap();
    }

    #[test]
    #[ignore = "needs a Redis server (set KW_REDIS_ADDR or run one on 127.0.0.1:6379)"]
    fn logout_user_over_redis() {
        let mgr = redis_manager();
        Executor::new()
            .unwrap()
            .block_on(async {
                let user = "redis-bob";
                mgr.logout_user(user).await.unwrap();

                let phone = mgr.login(user, vec![], "phone").await.unwrap();
                let laptop = mgr.login(user, vec![], "laptop").await.unwrap();
                assert_eq!(mgr.sessions_of(user).await.unwrap().len(), 2, "two devices logged in");

                // One call revokes every session of the user (logout everywhere / ban).
                mgr.logout_user(user).await.unwrap();
                assert!(!mgr.authenticate(Some(&phone)).await.is_authenticated());
                assert!(!mgr.authenticate(Some(&laptop)).await.is_authenticated());
                assert_eq!(mgr.sessions_of(user).await.unwrap().len(), 0);
            })
            .unwrap();
    }
}
