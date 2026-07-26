//! Visitor tracking + ban middleware — the request-side of `kernway_security::tracking`.
//!
//! - [`VisitorTracking`] gives every request a stable visitor id (a `kw_visitor`
//!   cookie, set on first contact), resolves the client IP (proxy-aware) and the
//!   User-Agent, and puts a [`RequestMeta`] in the request scope (KEP-0005) — read
//!   by a handler or a template like a `SecurityContext`.
//! - [`BanFilter`] rejects a request whose IP / subnet / User-Agent is on a
//!   [`BanList`], with a default `403` or a caller-supplied response.
//!
//! Both resolve the IP the same safe way ([`client_ip`]): the socket peer unless it
//! is a trusted proxy, then the first untrusted `X-Forwarded-For` hop.

use std::net::IpAddr;
use std::sync::Arc;

use di_core::RequestScope;
use kernway_core::error::StatusCode;
use kernway_core::layer::BoxFuture;
use kernway_core::request::Request;
use kernway_core::response::Response;
use kernway_security::tracking::{client_ip, new_visitor_id, visitor_cookie, visitor_from_cookie, RequestMeta};
use kernway_security::Bans;

use crate::middleware::{Middleware, Next};

/// The `X-Forwarded-For` header the proxy-aware IP resolution reads.
const FORWARDED_FOR: &str = "x-forwarded-for";

/// Establishes per-request visitor identity + metadata (KEP-0005): a `kw_visitor`
/// cookie (set on the first visit), the proxy-aware client IP, the User-Agent, and
/// the path — a [`RequestMeta`] in the scope. Add trusted proxies so the IP is
/// resolved from `X-Forwarded-For` only when it can be trusted.
pub struct VisitorTracking {
    trusted: Vec<IpAddr>,
}

impl VisitorTracking {
    /// A tracker that trusts no proxy (IP = the socket peer, forwarded headers
    /// ignored). Add proxies with [`trust_proxy`](Self::trust_proxy).
    #[must_use]
    pub fn new() -> Self {
        Self { trusted: Vec::new() }
    }

    /// Trust a reverse proxy by IP, so `X-Forwarded-For` from it is honoured.
    #[must_use]
    pub fn trust_proxy(mut self, ip: IpAddr) -> Self {
        self.trusted.push(ip);
        self
    }
}

impl Default for VisitorTracking {
    fn default() -> Self {
        Self::new()
    }
}

impl Middleware for VisitorTracking {
    fn name(&self) -> &'static str {
        "VisitorTracking"
    }

    fn handle<'a>(&'a self, req: Request, scope: &'a RequestScope, next: Next<'a>) -> BoxFuture<'a, Response> {
        // Resolve everything synchronously (borrows req/scope), then move on.
        let existing = req.header("cookie").and_then(visitor_from_cookie).map(str::to_string);
        let is_new = existing.is_none();
        let visitor_id = existing.unwrap_or_else(new_visitor_id);
        let ip = client_ip(req.remote_addr, req.header(FORWARDED_FOR), &self.trusted);
        let user_agent = req.header("user-agent").map(str::to_string);
        scope.set(RequestMeta {
            visitor_id: visitor_id.clone(),
            ip,
            user_agent,
            path: req.path.clone(),
            method: req.method.clone(),
        });
        // Set the cookie on the response only for a first-time visitor.
        let new_cookie = is_new.then(|| visitor_cookie(&visitor_id));

        Box::pin(async move {
            let mut response = next.run(req, scope).await;
            if let Some(cookie) = new_cookie {
                response.headers.insert("set-cookie", &cookie);
            }
            response
        })
    }
}

/// Rejects a banned request early. Resolves the client IP the same proxy-aware way,
/// checks it and the User-Agent against the shared [`Bans`] list, and returns the
/// ban response (default `403`, or [`response`](Self::response)) without reaching the
/// handler. Because it holds the shared [`Bans`], runtime `unban` takes effect on the
/// next request.
pub struct BanFilter {
    bans: Bans,
    trusted: Vec<IpAddr>,
    response: Option<Arc<dyn Fn() -> Response + Send + Sync>>,
}

impl BanFilter {
    /// A filter enforcing the shared `bans`. Trusts no proxy by default (add with
    /// [`trust_proxy`](Self::trust_proxy)) and returns a default `403`.
    #[must_use]
    pub fn new(bans: Bans) -> Self {
        Self { bans, trusted: Vec::new(), response: None }
    }

    /// Trust a reverse proxy by IP (so the ban applies to the real client IP behind it).
    #[must_use]
    pub fn trust_proxy(mut self, ip: IpAddr) -> Self {
        self.trusted.push(ip);
        self
    }

    /// Serve a **custom** response to a banned request (a branded page, a different
    /// status) instead of the default `403`.
    ///
    /// ```rust,ignore
    /// BanFilter::new(bans).response(|| {
    ///     Response::new(StatusCode::FORBIDDEN)
    ///         .content_type("text/html; charset=utf-8")
    ///         .body(b"<h1>You are banned</h1>".to_vec())
    /// })
    /// ```
    #[must_use]
    pub fn response<F>(mut self, response: F) -> Self
    where
        F: Fn() -> Response + Send + Sync + 'static,
    {
        self.response = Some(Arc::new(response));
        self
    }
}

impl Middleware for BanFilter {
    fn name(&self) -> &'static str {
        "BanFilter"
    }

    fn handle<'a>(&'a self, req: Request, scope: &'a RequestScope, next: Next<'a>) -> BoxFuture<'a, Response> {
        let ip = client_ip(req.remote_addr, req.header(FORWARDED_FOR), &self.trusted);
        if self.bans.is_banned(ip, req.header("user-agent")) {
            let response = self.response.as_ref().map_or_else(banned_response, |custom| custom());
            return Box::pin(async move { response });
        }
        next.run(req, scope)
    }
}

/// The default response for a banned request — a `403` RFC 7807.
fn banned_response() -> Response {
    Response::new(StatusCode::FORBIDDEN)
        .content_type("application/json; charset=utf-8")
        .body(br#"{"status":403,"title":"Forbidden","detail":"access denied"}"#.to_vec())
}

/// Records the current request into an [`Activity`] store (feature = `presence`), so
/// an admin can see a live "who is on the site and where" list. Reads the
/// [`RequestMeta`] a `VisitorTracking` upstream put in the scope and the
/// `SecurityContext` an auth middleware set — the identity is the logged-in user if
/// there is one, else the anonymous visitor id. Recording is best-effort: a store
/// error never fails the request.
///
/// Order it **after** `VisitorTracking` (and any auth), so both are in the scope.
#[cfg(feature = "presence")]
pub struct ActivityTracking {
    activity: std::sync::Arc<dyn kernway_security::Activity>,
}

#[cfg(feature = "presence")]
impl ActivityTracking {
    /// Record into `activity` (an `InMemoryActivity` or a `RedisActivity`).
    #[must_use]
    pub fn new(activity: std::sync::Arc<dyn kernway_security::Activity>) -> Self {
        Self { activity }
    }
}

#[cfg(feature = "presence")]
impl Middleware for ActivityTracking {
    fn name(&self) -> &'static str {
        "ActivityTracking"
    }

    fn handle<'a>(&'a self, req: Request, scope: &'a RequestScope, next: Next<'a>) -> BoxFuture<'a, Response> {
        use kernway_security::{ActiveVisitor, SecurityContext};

        // Needs the RequestMeta from VisitorTracking; without it, just pass through.
        let Ok(meta) = scope.get::<RequestMeta>() else {
            return next.run(req, scope);
        };
        // Identity: the logged-in user if authenticated, else the anonymous visitor id.
        let ctx = scope.get::<SecurityContext>().ok();
        let (id, authenticated) = match ctx.as_deref() {
            Some(c) if c.is_authenticated() => {
                (c.principal().unwrap_or(&meta.visitor_id).to_string(), true)
            }
            _ => (meta.visitor_id.clone(), false),
        };
        let visitor = ActiveVisitor {
            id,
            authenticated,
            ip: meta.ip,
            user_agent: meta.user_agent.clone(),
            path: meta.path.clone(),
            method: meta.method.clone(),
            last_seen: unix_now(),
        };
        let activity = self.activity.clone();
        Box::pin(async move {
            // Best-effort: a store error must not fail the request being served.
            let _ = activity.record(visitor).await;
            next.run(req, scope).await
        })
    }
}

/// Unix seconds now — the activity liveness clock. Runtime code (not a workflow), so
/// the system clock is available.
#[cfg(feature = "presence")]
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
