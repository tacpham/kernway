//! Sessions (KEP-0004): a signed token backed by a revocable registry, with live
//! timeouts and an optional account seam.
//!
//! The [`SessionManager`] ties together the token codec, the session registry
//! ([`SessionStore`]), a hot-reloadable [`SessionConfig`], and an optional
//! [`AccountStatus`] provider. `authenticate` is the one decision point: verify the
//! token, confirm the `sid` is still registered, enforce the current timeouts, and —
//! when an account provider is set — check `active`/`expire`/`version`. Any failure
//! is anonymous, and the session is evicted on the way out.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kernway_core::layer::BoxFuture;

use crate::csrf::CsrfToken;
use crate::token::{Claims, TokenCodec};
use crate::SecurityContext;

/// The cookie the session token is carried in.
pub const COOKIE: &str = "kw_session";

/// The `Set-Cookie` value that stores a session token. `HttpOnly` (JS cannot read
/// it) and `SameSite=Lax`; `Secure` when served over HTTPS.
pub fn set_cookie(token: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax{secure}")
}

/// The `Set-Cookie` value that clears the session cookie (logout).
pub fn clear_cookie() -> String {
    format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// Pull the session token out of a `Cookie` header value.
pub fn token_from_cookie(cookie_header: &str) -> Option<&str> {
    cookie_header.split(';').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == COOKIE).then(|| v.trim())
    })
}

/// Unix seconds now.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One login. The registry holds one of these per active session.
#[derive(Debug, Clone)]
pub struct SessionRecord {
    /// The user this session belongs to.
    pub user: String,
    /// Login time, unix seconds — for the absolute timeout.
    pub created: u64,
    /// Last activity, unix seconds — for the idle timeout (advanced lazily).
    pub last_seen: u64,
    /// A device/ip label, for a "my sessions" list.
    pub meta: String,
}

/// A session store backend failed — the network, the protocol, whatever the
/// backend surfaces. The in-memory store never produces one; a Redis/SQL backend
/// does when it is unreachable, and the manager turns that into a real error at
/// `login` rather than a silent half-success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The backing store (Redis, SQL, …) errored. The string is the backend's message.
    Backend(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Backend(message) => write!(f, "session store backend error: {message}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// The session registry — an index of live sessions, keyed by `sid`. The default
/// backend is in-memory; a trait so Redis/SQL can be swapped in for scale.
///
/// Every method is **async** ([KEP-0004]/[KEP-0006]): a Redis or SQL backend awaits
/// its network call without blocking a core, and the in-memory backend wraps its
/// `RwLock` read in a ready future (~21 ns, measured — see `benches/session_store`).
/// The returned futures borrow `&self` and are awaited within `authenticate`'s own
/// frame, so they need only be `Send`, not `'static`.
///
/// Every method is **fallible** ([`StoreError`]): a remote store can be unreachable,
/// and swallowing that is how a "logged in" token ends up naming a session that was
/// never stored. The in-memory store returns `Ok` always; a Redis backend reports
/// its error, and the manager decides the policy — `login` fails loudly, while
/// `authenticate` fails *closed* (a store it cannot reach means "not authenticated").
///
/// [KEP-0004]: https://github.com/tacpham/kernway/blob/main/docs/kep/0004-sessions.md
/// [KEP-0006]: https://github.com/tacpham/kernway/blob/main/docs/kep/0006-async-handlers.md
// `len` is the store's session count; an `is_empty` on a live session store would be
// a racy, rarely-useful async round-trip, so it is deliberately not part of the trait.
#[allow(clippy::len_without_is_empty)]
pub trait SessionStore: Send + Sync {
    /// Register a new session.
    fn insert(&self, sid: &str, record: SessionRecord) -> BoxFuture<'_, Result<(), StoreError>>;
    /// The record for `sid`, or `None` — this is also the membership check.
    fn get(&self, sid: &str) -> BoxFuture<'_, Result<Option<SessionRecord>, StoreError>>;
    /// Advance `last_seen` (for the idle timeout); called lazily.
    fn touch(&self, sid: &str, at: u64) -> BoxFuture<'_, Result<(), StoreError>>;
    /// Remove one session (logout / revocation / eviction).
    fn remove(&self, sid: &str) -> BoxFuture<'_, Result<(), StoreError>>;
    /// Remove every session of a user (logout everywhere / ban).
    fn remove_user(&self, user: &str) -> BoxFuture<'_, Result<(), StoreError>>;
    /// This user's active sessions, as `(sid, record)`.
    fn sessions_of(
        &self,
        user: &str,
    ) -> BoxFuture<'_, Result<Vec<(String, SessionRecord)>, StoreError>>;
    /// The number of live sessions — for the capacity cap.
    fn len(&self) -> BoxFuture<'_, Result<usize, StoreError>>;
}

/// In-memory registry: a read-mostly `RwLock<HashMap>`. The per-request path is a
/// read; login/logout are the rare writes.
#[derive(Default)]
pub struct MemorySessionStore {
    inner: RwLock<HashMap<String, SessionRecord>>,
}

impl MemorySessionStore {
    /// An empty in-memory registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Every live session as `(sid, record)` — for a durable backend to snapshot the
    /// whole registry at a checkpoint.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(String, SessionRecord)> {
        self.inner
            .read()
            .unwrap()
            .iter()
            .map(|(sid, r)| (sid.clone(), r.clone()))
            .collect()
    }
}

// A read/write on an uncontended in-process `RwLock` resolves immediately; each
// method does its work synchronously and hands back a ready future, so the async
// trait costs the in-memory store only the box (measured ~21 ns — negligible next
// to the auth path's HMAC verify; see `benches/session_store`). Args are copied to
// owned values before the block so the future borrows only `&self`.
// In-memory work is infallible — every method returns `Ok`. It still returns a
// `Result` to satisfy the trait, so the manager's error handling is uniform across
// backends.
impl SessionStore for MemorySessionStore {
    fn insert(&self, sid: &str, record: SessionRecord) -> BoxFuture<'_, Result<(), StoreError>> {
        let sid = sid.to_string();
        Box::pin(async move {
            self.inner.write().unwrap().insert(sid, record);
            Ok(())
        })
    }
    fn get(&self, sid: &str) -> BoxFuture<'_, Result<Option<SessionRecord>, StoreError>> {
        let sid = sid.to_string();
        Box::pin(async move { Ok(self.inner.read().unwrap().get(&sid).cloned()) })
    }
    fn touch(&self, sid: &str, at: u64) -> BoxFuture<'_, Result<(), StoreError>> {
        let sid = sid.to_string();
        Box::pin(async move {
            if let Some(r) = self.inner.write().unwrap().get_mut(&sid) {
                r.last_seen = at;
            }
            Ok(())
        })
    }
    fn remove(&self, sid: &str) -> BoxFuture<'_, Result<(), StoreError>> {
        let sid = sid.to_string();
        Box::pin(async move {
            self.inner.write().unwrap().remove(&sid);
            Ok(())
        })
    }
    fn remove_user(&self, user: &str) -> BoxFuture<'_, Result<(), StoreError>> {
        let user = user.to_string();
        Box::pin(async move {
            self.inner.write().unwrap().retain(|_, r| r.user != user);
            Ok(())
        })
    }
    fn sessions_of(
        &self,
        user: &str,
    ) -> BoxFuture<'_, Result<Vec<(String, SessionRecord)>, StoreError>> {
        let user = user.to_string();
        Box::pin(async move {
            Ok(self
                .inner
                .read()
                .unwrap()
                .iter()
                .filter(|(_, r)| r.user == user)
                .map(|(sid, r)| (sid.clone(), r.clone()))
                .collect())
        })
    }
    fn len(&self) -> BoxFuture<'_, Result<usize, StoreError>> {
        Box::pin(async move { Ok(self.inner.read().unwrap().len()) })
    }
}

/// Session timeouts and capacity — read live on every `authenticate`, so a change
/// takes effect immediately for existing sessions (KEP-0004).
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// The `exp` baked into a new token (a fail-safe upper bound).
    pub token_ttl: Duration,
    /// Max session age from `created`, enforced server-side against current config.
    pub absolute_timeout: Duration,
    /// Max gap since `last_seen`; `None` = no idle limit.
    pub idle_timeout: Option<Duration>,
    /// Registry capacity cap; `None` = bounded only by memory.
    pub max_sessions: Option<usize>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            token_ttl: Duration::from_secs(60 * 60),
            absolute_timeout: Duration::from_secs(24 * 60 * 60),
            idle_timeout: None,
            max_sessions: None,
        }
    }
}

/// Per-user account status — the optional seam (KEP-0004). When a provider is set,
/// `authenticate` checks it, so deactivation, subscription expiry, and role changes
/// take effect on the next request.
#[derive(Debug, Clone)]
pub struct Account {
    /// `false` → the session is invalidated (account disabled / banned).
    pub active: bool,
    /// Subscription end, unix seconds; `Some(t)` with `t < now` invalidates.
    pub expires: Option<u64>,
    /// Bumped by the app on any change that must force re-login (role, etc.).
    pub version: u64,
}

/// Implemented by the application over its user store.
pub trait AccountStatus: Send + Sync {
    /// The account status for `user`, or `None` if the user does not exist.
    fn account(&self, user: &str) -> Option<Account>;
}

/// Why a login was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum LoginError {
    /// The registry is at `max_sessions`.
    AtCapacity,
    /// The session store failed (e.g. Redis unreachable) — the session was not
    /// persisted, so the login is a real error, not a silent half-success.
    Store(StoreError),
}

impl std::fmt::Display for LoginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoginError::AtCapacity => write!(f, "too many active sessions"),
            LoginError::Store(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for LoginError {}

impl From<StoreError> for LoginError {
    fn from(e: StoreError) -> Self {
        LoginError::Store(e)
    }
}

/// Ties the registry, the token codec, the live config, and the optional account
/// provider together.
pub struct SessionManager {
    store: Box<dyn SessionStore>,
    codec: TokenCodec,
    config: RwLock<SessionConfig>,
    accounts: Option<Box<dyn AccountStatus>>,
}

impl SessionManager {
    /// Build a manager over a store, a signing key, and an initial config.
    pub fn new(
        store: Box<dyn SessionStore>,
        key: impl Into<Vec<u8>>,
        config: SessionConfig,
    ) -> Self {
        Self {
            store,
            codec: TokenCodec::new(key),
            config: RwLock::new(config),
            accounts: None,
        }
    }

    /// Attach an account-status provider (the optional seam).
    #[must_use]
    pub fn with_accounts(mut self, accounts: Box<dyn AccountStatus>) -> Self {
        self.accounts = Some(accounts);
        self
    }

    /// Update the live config; the change applies to the next `authenticate`.
    pub fn reconfigure(&self, f: impl FnOnce(&mut SessionConfig)) {
        f(&mut self.config.write().unwrap());
    }

    /// Log a user in: register a session and return a signed token to set as a
    /// cookie. `roles` is the login snapshot; with an account provider, `version`
    /// is taken from the account so a later change forces re-login.
    pub async fn login(
        &self,
        user: &str,
        roles: Vec<String>,
        meta: impl Into<String>,
    ) -> Result<String, LoginError> {
        self.login_at(user, roles, meta, now()).await
    }

    pub(crate) async fn login_at(
        &self,
        user: &str,
        roles: Vec<String>,
        meta: impl Into<String>,
        at: u64,
    ) -> Result<String, LoginError> {
        // Copy the config values out and drop the guard before any `.await` — a
        // lock guard must never be held across a suspend point.
        let (max_sessions, token_ttl) = {
            let cfg = self.config.read().unwrap();
            (cfg.max_sessions, cfg.token_ttl.as_secs())
        };
        if let Some(max) = max_sessions {
            // A store error here is a real login failure (`?` → LoginError::Store).
            if self.store.len().await? >= max {
                return Err(LoginError::AtCapacity);
            }
        }
        let sid = CsrfToken::generate().as_str().to_string(); // 32 bytes of OS randomness, hex
        let version = self
            .accounts
            .as_ref()
            .and_then(|a| a.account(user))
            .map_or(0, |acc| acc.version);
        // If the store cannot persist the session, the login fails loudly rather
        // than handing back a token for a session that does not exist.
        self.store
            .insert(
                &sid,
                SessionRecord {
                    user: user.to_string(),
                    created: at,
                    last_seen: at,
                    meta: meta.into(),
                },
            )
            .await?;
        let exp = at + token_ttl;
        Ok(self.codec.sign(&Claims {
            sid,
            user: user.to_string(),
            roles,
            version,
            exp,
        }))
    }

    /// Turn a token (from the cookie) into a `SecurityContext`. Absent/invalid/
    /// revoked/expired/disabled → anonymous.
    pub async fn authenticate(&self, token: Option<&str>) -> SecurityContext {
        self.authenticate_at(token, now()).await
    }

    pub(crate) async fn authenticate_at(&self, token: Option<&str>, at: u64) -> SecurityContext {
        let Some(token) = token else {
            return SecurityContext::anonymous();
        };
        let Some(claims) = self.codec.verify(token) else {
            return SecurityContext::anonymous();
        };

        // Token's own expiry — the fail-safe upper bound.
        if claims.exp < at {
            return SecurityContext::anonymous();
        }
        // Revocation: the sid must still be registered. A store error here fails
        // *closed* — a registry we cannot reach means we cannot confirm the session
        // is still valid, so we must not authenticate (deny, never grant on doubt).
        let record = match self.store.get(&claims.sid).await {
            Ok(Some(record)) => record,
            Ok(None) => return SecurityContext::anonymous(),
            Err(err) => {
                kernway_log::warn!(target: "kernway_security", "authenticate failed closed (store error): {err}");
                return SecurityContext::anonymous();
            }
        };

        // Copy the live timeouts out and drop the guard before any `.await`.
        let (absolute_timeout, idle_timeout) = {
            let cfg = self.config.read().unwrap();
            (
                cfg.absolute_timeout.as_secs(),
                cfg.idle_timeout.map(|d| d.as_secs()),
            )
        };
        // Timeouts, enforced against the *current* config. The eviction remove is
        // best-effort — the decision is already "anonymous" regardless.
        if at > record.created.saturating_add(absolute_timeout) {
            let _ = self.store.remove(&claims.sid).await;
            return SecurityContext::anonymous();
        }
        if let Some(idle) = idle_timeout {
            if at > record.last_seen.saturating_add(idle) {
                let _ = self.store.remove(&claims.sid).await;
                return SecurityContext::anonymous();
            }
        }

        // Account seam: active, subscription expiry, and version (role change).
        let roles = if let Some(accounts) = &self.accounts {
            match accounts.account(&claims.user) {
                None => {
                    let _ = self.store.remove(&claims.sid).await;
                    return SecurityContext::anonymous();
                }
                Some(acc) => {
                    let invalid = !acc.active
                        || acc.expires.is_some_and(|e| e < at)
                        || acc.version != claims.version;
                    if invalid {
                        let _ = self.store.remove(&claims.sid).await;
                        return SecurityContext::anonymous();
                    }
                    // Roles from the token are safe: a role change would have bumped
                    // the version and been caught above.
                    claims.roles.clone()
                }
            }
        } else {
            claims.roles.clone()
        };

        // Advance last_seen lazily (only when it drifts, to stay read-mostly).
        // Best-effort: a failed touch just means the idle clock is a little stale.
        if at > record.last_seen + 60 {
            let _ = self.store.touch(&claims.sid, at).await;
        }

        SecurityContext::authenticated(claims.user, roles)
    }

    /// Log out one session (this device). Errors if the store cannot be reached —
    /// the session may still be live, so the caller should know the revocation
    /// did not land (clearing the cookie alone is not a server-side logout).
    pub async fn logout(&self, sid: &str) -> Result<(), StoreError> {
        self.store.remove(sid).await
    }

    /// Log out the session a token names — the handler's convenience: read the
    /// cookie, hand it here. A bad token is a no-op (`Ok`).
    pub async fn logout_token(&self, token: &str) -> Result<(), StoreError> {
        match self.codec.verify(token) {
            Some(claims) => self.store.remove(&claims.sid).await,
            None => Ok(()),
        }
    }

    /// Log a user out everywhere (all devices) — also how a ban takes effect.
    pub async fn logout_user(&self, user: &str) -> Result<(), StoreError> {
        self.store.remove_user(user).await
    }

    /// The user's active sessions, for a "my logins" view.
    pub async fn sessions_of(
        &self,
        user: &str,
    ) -> Result<Vec<(String, SessionRecord)>, StoreError> {
        self.store.sessions_of(user).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    /// Drive a manager/store future to its result. Over `MemorySessionStore` every
    /// future is immediately ready (an uncontended `RwLock` op), so a single poll
    /// with a noop waker completes it — no runtime needed in these unit tests.
    fn block<T>(fut: impl Future<Output = T>) -> T {
        let mut fut = Box::pin(fut);
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("the in-memory session store must resolve synchronously"),
        }
    }

    fn manager() -> SessionManager {
        SessionManager::new(
            Box::new(MemorySessionStore::new()),
            "test-key",
            SessionConfig::default(),
        )
    }

    // Extract the sid from a token by verifying it (test helper).
    fn sid_of(mgr: &SessionManager, token: &str) -> String {
        mgr.codec.verify(token).unwrap().sid
    }

    #[test]
    fn login_then_authenticate_yields_the_user_and_roles() {
        let mgr = manager();
        let token = block(mgr.login("alice", vec!["ADMIN".into()], "chrome")).unwrap();
        let ctx = block(mgr.authenticate(Some(&token)));
        assert!(ctx.is_authenticated());
        assert_eq!(ctx.principal(), Some("alice"));
        assert!(ctx.has_role("ADMIN"));
    }

    #[test]
    fn no_token_is_anonymous() {
        assert!(!block(manager().authenticate(None)).is_authenticated());
    }

    #[test]
    fn a_forged_token_is_anonymous() {
        let mgr = manager();
        assert!(!block(mgr.authenticate(Some("garbage.token"))).is_authenticated());
    }

    #[test]
    fn logout_makes_the_next_request_anonymous() {
        let mgr = manager();
        let token = block(mgr.login("alice", vec![], "d")).unwrap();
        let sid = sid_of(&mgr, &token);
        assert!(block(mgr.authenticate(Some(&token))).is_authenticated());
        block(mgr.logout(&sid)).unwrap();
        assert!(
            !block(mgr.authenticate(Some(&token))).is_authenticated(),
            "revoked session must be anonymous"
        );
    }

    #[test]
    fn logout_user_kills_every_device() {
        let mgr = manager();
        let t1 = block(mgr.login("alice", vec![], "phone")).unwrap();
        let t2 = block(mgr.login("alice", vec![], "laptop")).unwrap();
        assert_eq!(
            block(mgr.sessions_of("alice")).unwrap().len(),
            2,
            "two devices logged in"
        );
        block(mgr.logout_user("alice")).unwrap();
        assert!(!block(mgr.authenticate(Some(&t1))).is_authenticated());
        assert!(!block(mgr.authenticate(Some(&t2))).is_authenticated());
        assert_eq!(block(mgr.sessions_of("alice")).unwrap().len(), 0);
    }

    #[test]
    fn an_expired_token_is_anonymous() {
        let mgr = manager();
        let token = block(mgr.login_at("alice", vec![], "d", 1000)).unwrap();
        // token_ttl default 1h → exp = 1000 + 3600. Authenticate well past it.
        assert!(!block(mgr.authenticate_at(Some(&token), 1000 + 3600 + 1)).is_authenticated());
    }

    #[test]
    fn the_absolute_timeout_is_enforced_live() {
        let mgr = manager();
        mgr.reconfigure(|c| {
            c.token_ttl = Duration::from_secs(10_000); // token long-lived…
            c.absolute_timeout = Duration::from_secs(300); // …but 5-min session cap
        });
        let token = block(mgr.login_at("alice", vec![], "d", 1000)).unwrap();
        assert!(
            block(mgr.authenticate_at(Some(&token), 1000 + 299)).is_authenticated(),
            "within 5 min"
        );
        assert!(
            !block(mgr.authenticate_at(Some(&token), 1000 + 301)).is_authenticated(),
            "past 5 min → out"
        );
    }

    #[test]
    fn shortening_the_timeout_takes_effect_immediately() {
        let mgr = manager();
        mgr.reconfigure(|c| c.absolute_timeout = Duration::from_secs(1800));
        let token = block(mgr.login_at("alice", vec![], "d", 1000)).unwrap();
        assert!(block(mgr.authenticate_at(Some(&token), 1000 + 1000)).is_authenticated());
        // Admin shortens the timeout to 5 min while the session is alive.
        mgr.reconfigure(|c| c.absolute_timeout = Duration::from_secs(300));
        assert!(
            !block(mgr.authenticate_at(Some(&token), 1000 + 1000)).is_authenticated(),
            "the live config applies now"
        );
    }

    #[test]
    fn the_idle_timeout_logs_out_an_inactive_session() {
        let mgr = manager();
        mgr.reconfigure(|c| {
            c.token_ttl = Duration::from_secs(10_000);
            c.absolute_timeout = Duration::from_secs(10_000); // not the cause here
            c.idle_timeout = Some(Duration::from_secs(100)); // 100s of inactivity
        });
        let token = block(mgr.login_at("alice", vec![], "d", 1000)).unwrap();
        assert!(
            block(mgr.authenticate_at(Some(&token), 1000 + 50)).is_authenticated(),
            "still active"
        );
        assert!(
            !block(mgr.authenticate_at(Some(&token), 1000 + 101)).is_authenticated(),
            "idle past 100s → out"
        );
    }

    #[test]
    fn max_sessions_refuses_a_new_login() {
        let mgr = manager();
        mgr.reconfigure(|c| c.max_sessions = Some(1));
        let _ = block(mgr.login("a", vec![], "d")).unwrap();
        assert_eq!(
            block(mgr.login("b", vec![], "d")),
            Err(LoginError::AtCapacity)
        );
    }

    // --- a store that always fails, for the fallible-store behaviour ---------

    /// A `SessionStore` whose every operation errors — stands in for "Redis is
    /// down" without a server, so the manager's error policy is unit-testable.
    struct FailingStore;
    impl SessionStore for FailingStore {
        fn insert(&self, _: &str, _: SessionRecord) -> BoxFuture<'_, Result<(), StoreError>> {
            Box::pin(async { Err(StoreError::Backend("store down".into())) })
        }
        fn get(&self, _: &str) -> BoxFuture<'_, Result<Option<SessionRecord>, StoreError>> {
            Box::pin(async { Err(StoreError::Backend("store down".into())) })
        }
        fn touch(&self, _: &str, _: u64) -> BoxFuture<'_, Result<(), StoreError>> {
            Box::pin(async { Err(StoreError::Backend("store down".into())) })
        }
        fn remove(&self, _: &str) -> BoxFuture<'_, Result<(), StoreError>> {
            Box::pin(async { Err(StoreError::Backend("store down".into())) })
        }
        fn remove_user(&self, _: &str) -> BoxFuture<'_, Result<(), StoreError>> {
            Box::pin(async { Err(StoreError::Backend("store down".into())) })
        }
        fn sessions_of(
            &self,
            _: &str,
        ) -> BoxFuture<'_, Result<Vec<(String, SessionRecord)>, StoreError>> {
            Box::pin(async { Err(StoreError::Backend("store down".into())) })
        }
        fn len(&self) -> BoxFuture<'_, Result<usize, StoreError>> {
            Box::pin(async { Err(StoreError::Backend("store down".into())) })
        }
    }

    #[test]
    fn login_errors_when_the_store_is_down() {
        let mgr = SessionManager::new(Box::new(FailingStore), "k", SessionConfig::default());
        // No token handed back for a session that could not be stored — a real error.
        let err = block(mgr.login("alice", vec![], "d")).unwrap_err();
        assert!(
            matches!(err, LoginError::Store(StoreError::Backend(_))),
            "store down must fail the login, got {err:?}"
        );
    }

    #[test]
    fn logout_errors_when_the_store_is_down() {
        let mgr = SessionManager::new(Box::new(FailingStore), "k", SessionConfig::default());
        // Logout that cannot reach the store reports it — clearing the cookie alone
        // is not a server-side revocation, and the caller must be able to tell.
        assert!(
            block(mgr.logout("some-sid")).is_err(),
            "store down must fail the logout"
        );
    }

    #[test]
    fn authenticate_fails_closed_when_the_store_is_down() {
        // Mint a valid, well-signed token with a working store…
        let key = "shared-signing-key";
        let minting = SessionManager::new(
            Box::new(MemorySessionStore::new()),
            key,
            SessionConfig::default(),
        );
        let token = block(minting.login("alice", vec!["ADMIN".into()], "d")).unwrap();
        // …then authenticate it against a manager whose store is down. The token is
        // genuine, but the registry cannot be reached to confirm the session is live,
        // so it must deny — never grant on doubt.
        let down = SessionManager::new(Box::new(FailingStore), key, SessionConfig::default());
        assert!(
            !block(down.authenticate(Some(&token))).is_authenticated(),
            "store unreachable → authenticate denies (fails closed)"
        );
    }

    // --- the account seam --------------------------------------------------

    struct Accounts(std::sync::Mutex<HashMap<String, Account>>);
    impl AccountStatus for Accounts {
        fn account(&self, user: &str) -> Option<Account> {
            self.0.lock().unwrap().get(user).cloned()
        }
    }

    fn with_account(acc: Account) -> (SessionManager, std::sync::Arc<Accounts>) {
        // Note: for a mutable-in-test provider we keep a handle to tweak it.
        let mut map = HashMap::new();
        map.insert("alice".to_string(), acc);
        let accounts = std::sync::Arc::new(Accounts(std::sync::Mutex::new(map)));
        // The manager needs its own boxed provider; use a thin forwarder.
        struct Fwd(std::sync::Arc<Accounts>);
        impl AccountStatus for Fwd {
            fn account(&self, u: &str) -> Option<Account> {
                self.0.account(u)
            }
        }
        let mgr = SessionManager::new(
            Box::new(MemorySessionStore::new()),
            "k",
            SessionConfig::default(),
        )
        .with_accounts(Box::new(Fwd(accounts.clone())));
        (mgr, accounts)
    }

    #[test]
    fn a_deactivated_account_is_logged_out() {
        let (mgr, accounts) = with_account(Account {
            active: true,
            expires: None,
            version: 1,
        });
        let token = block(mgr.login("alice", vec!["USER".into()], "d")).unwrap();
        assert!(block(mgr.authenticate(Some(&token))).is_authenticated());
        accounts.0.lock().unwrap().get_mut("alice").unwrap().active = false;
        assert!(
            !block(mgr.authenticate(Some(&token))).is_authenticated(),
            "disabled → out"
        );
    }

    #[test]
    fn an_expired_subscription_is_logged_out() {
        let (mgr, _accounts) = with_account(Account {
            active: true,
            expires: Some(500),
            version: 1,
        });
        let token = block(mgr.login_at("alice", vec![], "d", 100)).unwrap();
        assert!(
            block(mgr.authenticate_at(Some(&token), 400)).is_authenticated(),
            "before expiry"
        );
        assert!(
            !block(mgr.authenticate_at(Some(&token), 600)).is_authenticated(),
            "after subscription expiry"
        );
    }

    #[test]
    fn a_version_bump_forces_re_login() {
        let (mgr, accounts) = with_account(Account {
            active: true,
            expires: None,
            version: 1,
        });
        let token = block(mgr.login("alice", vec!["ADMIN".into()], "d")).unwrap();
        assert!(block(mgr.authenticate(Some(&token))).is_authenticated());
        // A role change bumps the version → the old token's version no longer matches.
        accounts.0.lock().unwrap().get_mut("alice").unwrap().version = 2;
        assert!(
            !block(mgr.authenticate(Some(&token))).is_authenticated(),
            "account change → logged out"
        );
    }

    // --- the same force-logout conditions, over a real Redis registry -------
    //
    // These prove that revocation-by-condition (timeout, idle, subscription
    // expiry, deactivation, version bump) still logs a session out when the
    // registry is Redis, not just in-memory — the whole point of the distributed
    // backend. They need a server, so they are `#[ignore]`d; run with:
    //
    //   docker run -d --rm --name kw-redis -p 6380:6379 redis:alpine
    //   KW_REDIS_ADDR=127.0.0.1:6380 cargo test -p kernway-security --features redis \
    //     session::tests::redis_seam -- --ignored
    //
    // Each uses a distinct username and cleans up first, so a re-run is idempotent.
    #[cfg(feature = "redis")]
    mod redis_seam {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        use crate::session::{Account, AccountStatus, SessionConfig, SessionManager};
        use crate::RedisSessionStore;

        /// Drive a real (network-backed) future to completion on rt-core.
        fn block<T>(fut: impl std::future::Future<Output = T>) -> T {
            rt_core::Executor::new().unwrap().block_on(fut).unwrap()
        }

        fn addr() -> std::net::SocketAddr {
            std::env::var("KW_REDIS_ADDR")
                .unwrap_or_else(|_| "127.0.0.1:6379".to_string())
                .parse()
                .expect("KW_REDIS_ADDR must be host:port")
        }

        fn redis_manager() -> SessionManager {
            let store = RedisSessionStore::new(addr(), Duration::from_secs(3600));
            SessionManager::new(Box::new(store), "redis-seam-key", SessionConfig::default())
        }

        struct Accounts(Mutex<HashMap<String, Account>>);
        impl AccountStatus for Accounts {
            fn account(&self, user: &str) -> Option<Account> {
                self.0.lock().unwrap().get(user).cloned()
            }
        }

        fn redis_manager_with_account(user: &str, acc: Account) -> (SessionManager, Arc<Accounts>) {
            let mut map = HashMap::new();
            map.insert(user.to_string(), acc);
            let accounts = Arc::new(Accounts(Mutex::new(map)));
            struct Fwd(Arc<Accounts>);
            impl AccountStatus for Fwd {
                fn account(&self, u: &str) -> Option<Account> {
                    self.0.account(u)
                }
            }
            let store = RedisSessionStore::new(addr(), Duration::from_secs(3600));
            let mgr =
                SessionManager::new(Box::new(store), "redis-seam-key", SessionConfig::default())
                    .with_accounts(Box::new(Fwd(accounts.clone())));
            (mgr, accounts)
        }

        #[test]
        #[ignore = "needs a Redis server (set KW_REDIS_ADDR or run one on 127.0.0.1:6379)"]
        fn absolute_timeout_logs_out_over_redis() {
            let mgr = redis_manager();
            block(async {
                let user = "seam-abs";
                let _ = mgr.logout_user(user).await;
                mgr.reconfigure(|c| {
                    c.token_ttl = Duration::from_secs(10_000);
                    c.absolute_timeout = Duration::from_secs(300);
                });
                let token = mgr.login_at(user, vec![], "d", 1000).await.unwrap();
                assert!(
                    mgr.authenticate_at(Some(&token), 1000 + 299)
                        .await
                        .is_authenticated(),
                    "within 5 min"
                );
                assert!(
                    !mgr.authenticate_at(Some(&token), 1000 + 301)
                        .await
                        .is_authenticated(),
                    "past 5 min → out"
                );
            });
        }

        #[test]
        #[ignore = "needs a Redis server (set KW_REDIS_ADDR or run one on 127.0.0.1:6379)"]
        fn idle_timeout_logs_out_over_redis() {
            let mgr = redis_manager();
            block(async {
                let user = "seam-idle";
                let _ = mgr.logout_user(user).await;
                mgr.reconfigure(|c| {
                    c.token_ttl = Duration::from_secs(10_000);
                    c.absolute_timeout = Duration::from_secs(10_000);
                    c.idle_timeout = Some(Duration::from_secs(100));
                });
                let token = mgr.login_at(user, vec![], "d", 1000).await.unwrap();
                assert!(
                    mgr.authenticate_at(Some(&token), 1000 + 50)
                        .await
                        .is_authenticated(),
                    "still active"
                );
                assert!(
                    !mgr.authenticate_at(Some(&token), 1000 + 101)
                        .await
                        .is_authenticated(),
                    "idle past 100s → out"
                );
            });
        }

        #[test]
        #[ignore = "needs a Redis server (set KW_REDIS_ADDR or run one on 127.0.0.1:6379)"]
        fn expired_subscription_logs_out_over_redis() {
            let user = "seam-expire";
            let (mgr, _accounts) = redis_manager_with_account(
                user,
                Account {
                    active: true,
                    expires: Some(500),
                    version: 1,
                },
            );
            block(async {
                let _ = mgr.logout_user(user).await;
                let token = mgr.login_at(user, vec![], "d", 100).await.unwrap();
                assert!(
                    mgr.authenticate_at(Some(&token), 400)
                        .await
                        .is_authenticated(),
                    "before expiry"
                );
                assert!(
                    !mgr.authenticate_at(Some(&token), 600)
                        .await
                        .is_authenticated(),
                    "after subscription expiry → out"
                );
            });
        }

        #[test]
        #[ignore = "needs a Redis server (set KW_REDIS_ADDR or run one on 127.0.0.1:6379)"]
        fn deactivated_account_logs_out_over_redis() {
            let user = "seam-active";
            let (mgr, accounts) = redis_manager_with_account(
                user,
                Account {
                    active: true,
                    expires: None,
                    version: 1,
                },
            );
            block(async {
                let _ = mgr.logout_user(user).await;
                let token = mgr
                    .login(user, vec!["USER".to_string()], "d")
                    .await
                    .unwrap();
                assert!(mgr.authenticate(Some(&token)).await.is_authenticated());
                accounts.0.lock().unwrap().get_mut(user).unwrap().active = false;
                assert!(
                    !mgr.authenticate(Some(&token)).await.is_authenticated(),
                    "disabled → out"
                );
            });
        }

        #[test]
        #[ignore = "needs a Redis server (set KW_REDIS_ADDR or run one on 127.0.0.1:6379)"]
        fn version_bump_logs_out_over_redis() {
            let user = "seam-version";
            let (mgr, accounts) = redis_manager_with_account(
                user,
                Account {
                    active: true,
                    expires: None,
                    version: 1,
                },
            );
            block(async {
                let _ = mgr.logout_user(user).await;
                let token = mgr
                    .login(user, vec!["ADMIN".to_string()], "d")
                    .await
                    .unwrap();
                assert!(mgr.authenticate(Some(&token)).await.is_authenticated());
                // A role change bumps the version → the old token no longer matches.
                accounts.0.lock().unwrap().get_mut(user).unwrap().version = 2;
                assert!(
                    !mgr.authenticate(Some(&token)).await.is_authenticated(),
                    "account change → out"
                );
            });
        }
    }
}
