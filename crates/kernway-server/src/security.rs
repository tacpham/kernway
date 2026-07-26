//! Web security — central, path-based access rules, Spring Security's `HttpSecurity`.
//!
//! Declare which paths are public, which need a login, and which need a role, in one
//! place, and a middleware enforces it before any handler runs:
//!
//! ```rust,ignore
//! let security = HttpSecurity::new()
//!     .permit_all("/public/**")            // open
//!     .permit_all("/login")
//!     .has_role("/admin/**", "ADMIN")      // ADMIN only
//!     .has_any_role("/staff/**", &["ADMIN", "STAFF"])
//!     .authenticated("/api/**")            // any logged-in user
//!     .any_request(Access::Authenticated)  // the fallback (Spring's anyRequest())
//!     .build();
//! app.layer(auth_middleware).layer(security)   // auth sets identity, this enforces
//! ```
//!
//! This is **authorization** (who may reach a path). **Authentication** — how the
//! identity is established (a session via `SessionManager`, a JWT, a header) — is a
//! separate upstream middleware that puts a `SecurityContext` in the request scope
//! (KEP-0005); this layer reads it. Missing context = anonymous. Rules are
//! first-match-wins, in declaration order, then `any_request`.

use std::sync::Arc;

use di_core::RequestScope;
use kernway_core::layer::BoxFuture;
use kernway_core::request::Request;
use kernway_core::response::Response;
use kernway_security::SecurityContext;

use crate::app::{forbidden, unauthorized};
use crate::middleware::{Middleware, Next};

/// What a path requires — the right-hand side of a Spring `authorizeHttpRequests` rule.
#[derive(Debug, Clone)]
pub enum Access {
    /// Anyone, authenticated or not.
    PermitAll,
    /// Any authenticated user.
    Authenticated,
    /// An authenticated user with this role.
    HasRole(String),
    /// An authenticated user with at least one of these roles.
    HasAnyRole(Vec<String>),
    /// No one.
    DenyAll,
}

/// A builder of path-based access rules (Spring's `HttpSecurity`). Rules match in
/// declaration order (first match wins); [`any_request`](HttpSecurity::any_request)
/// is the fallback.
pub struct HttpSecurity {
    rules: Vec<(String, Access)>,
    default: Access,
}

impl HttpSecurity {
    /// A fresh policy whose fallback is `Authenticated` (secure by default — an
    /// unlisted path needs a login until you say otherwise).
    #[must_use]
    pub fn new() -> Self {
        Self { rules: Vec::new(), default: Access::Authenticated }
    }

    /// Open a path pattern to everyone.
    #[must_use]
    pub fn permit_all(mut self, pattern: &str) -> Self {
        self.rules.push((pattern.to_string(), Access::PermitAll));
        self
    }

    /// Require any authenticated user on a pattern.
    #[must_use]
    pub fn authenticated(mut self, pattern: &str) -> Self {
        self.rules.push((pattern.to_string(), Access::Authenticated));
        self
    }

    /// Require `role` on a pattern.
    #[must_use]
    pub fn has_role(mut self, pattern: &str, role: &str) -> Self {
        self.rules.push((pattern.to_string(), Access::HasRole(role.to_string())));
        self
    }

    /// Require any of `roles` on a pattern.
    #[must_use]
    pub fn has_any_role(mut self, pattern: &str, roles: &[&str]) -> Self {
        let roles = roles.iter().map(|r| (*r).to_string()).collect();
        self.rules.push((pattern.to_string(), Access::HasAnyRole(roles)));
        self
    }

    /// Deny a pattern outright.
    #[must_use]
    pub fn deny_all(mut self, pattern: &str) -> Self {
        self.rules.push((pattern.to_string(), Access::DenyAll));
        self
    }

    /// The fallback for a request no rule matched (Spring's `anyRequest()`).
    #[must_use]
    pub fn any_request(mut self, access: Access) -> Self {
        self.default = access;
        self
    }

    /// Finish into the enforcing middleware — add it with `app.layer(...)`.
    #[must_use]
    pub fn build(self) -> SecurityLayer {
        SecurityLayer { rules: Arc::new(self.rules), default: self.default }
    }
}

impl Default for HttpSecurity {
    fn default() -> Self {
        Self::new()
    }
}

/// The middleware [`HttpSecurity::build`] produces: match the request path to a
/// rule and enforce it (401 when a login is needed, 403 when the role is missing).
pub struct SecurityLayer {
    rules: Arc<Vec<(String, Access)>>,
    default: Access,
}

impl SecurityLayer {
    /// The access rule for `path` — the first matching pattern, else the default.
    fn access_for(&self, path: &str) -> &Access {
        self.rules
            .iter()
            .find(|(pattern, _)| path_matches(pattern, path))
            .map_or(&self.default, |(_, access)| access)
    }
}

impl Middleware for SecurityLayer {
    fn name(&self) -> &'static str {
        "Security"
    }

    fn handle<'a>(&'a self, req: Request, scope: &'a RequestScope, next: Next<'a>) -> BoxFuture<'a, Response> {
        let anonymous = SecurityContext::anonymous();
        let context = scope.get::<SecurityContext>().ok();
        let decision = decide(self.access_for(&req.path), context.as_deref().unwrap_or(&anonymous));
        match decision {
            Decision::Allow => next.run(req, scope),
            Decision::Unauthenticated => Box::pin(async { unauthorized() }),
            Decision::Forbidden => Box::pin(async { forbidden() }),
        }
    }
}

/// The outcome of evaluating one rule against a context.
enum Decision {
    Allow,
    /// No/anonymous identity where one is required → 401.
    Unauthenticated,
    /// Authenticated, but the role is missing (or denied) → 403.
    Forbidden,
}

fn decide(access: &Access, context: &SecurityContext) -> Decision {
    let authed = context.is_authenticated();
    match access {
        Access::PermitAll => Decision::Allow,
        Access::Authenticated if authed => Decision::Allow,
        Access::Authenticated => Decision::Unauthenticated,
        Access::HasRole(role) if !authed => {
            let _ = role;
            Decision::Unauthenticated
        }
        Access::HasRole(role) if context.has_role(role) => Decision::Allow,
        Access::HasRole(_) => Decision::Forbidden,
        Access::HasAnyRole(_) if !authed => Decision::Unauthenticated,
        Access::HasAnyRole(roles) if roles.iter().any(|r| context.has_role(r)) => Decision::Allow,
        Access::HasAnyRole(_) => Decision::Forbidden,
        Access::DenyAll if authed => Decision::Forbidden,
        Access::DenyAll => Decision::Unauthenticated,
    }
}

/// Ant-style path match: `/a/**` matches `/a` and any descendant; `/a/*` matches
/// exactly one more segment; `**` matches everything; otherwise an exact match.
fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern == "**" || pattern == "/**" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path == prefix || path.starts_with(&format!("{prefix}/"));
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        return match path.strip_prefix(&format!("{prefix}/")) {
            Some(rest) => !rest.is_empty() && !rest.contains('/'),
            None => false,
        };
    }
    pattern == path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::Terminal;
    use di_core::{AppContext, RequestScope};
    use kernway_core::error::StatusCode;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        rt_core::Executor::new().unwrap().block_on(f).unwrap()
    }

    /// Run `layer` for `path` with `ctx` in the scope, through the real middleware
    /// chain (a terminal that returns 200), and give back the status. 200 means the
    /// request reached the handler; 401/403 mean it was stopped.
    fn enforce(layer: &SecurityLayer, path: &str, ctx: SecurityContext) -> StatusCode {
        let app = AppContext::new();
        let scope = RequestScope::new(&app);
        scope.set(ctx);
        let terminal: &Terminal =
            &|_req, _scope| Box::pin(async { Response::new(StatusCode::OK) }) as BoxFuture<'static, Response>;
        let next = Next { rest: &[], terminal };
        block_on(layer.handle(Request::new("GET", path), &scope, next)).status
    }

    #[test]
    fn ant_patterns_match_as_expected() {
        // `/**` — self and any depth.
        assert!(path_matches("/admin/**", "/admin"));
        assert!(path_matches("/admin/**", "/admin/users"));
        assert!(path_matches("/admin/**", "/admin/users/42"));
        assert!(!path_matches("/admin/**", "/admins"));
        assert!(!path_matches("/admin/**", "/public"));
        // `/*` — exactly one more segment.
        assert!(path_matches("/users/*", "/users/42"));
        assert!(!path_matches("/users/*", "/users"));
        assert!(!path_matches("/users/*", "/users/42/posts"));
        // exact, and catch-all.
        assert!(path_matches("/health", "/health"));
        assert!(!path_matches("/health", "/healthz"));
        assert!(path_matches("/**", "/anything/at/all"));
    }

    #[test]
    fn first_matching_rule_wins_then_the_default() {
        let layer = HttpSecurity::new()
            .permit_all("/public/**")
            .has_role("/admin/**", "ADMIN")
            .authenticated("/api/**")
            .any_request(Access::PermitAll)
            .build();
        assert!(matches!(layer.access_for("/public/x"), Access::PermitAll));
        assert!(matches!(layer.access_for("/admin/x"), Access::HasRole(r) if r == "ADMIN"));
        assert!(matches!(layer.access_for("/api/x"), Access::Authenticated));
        assert!(matches!(layer.access_for("/other"), Access::PermitAll), "falls to any_request");
    }

    #[test]
    fn decisions_cover_auth_and_role() {
        let admin = SecurityContext::authenticated("a", ["ADMIN"]);
        let user = SecurityContext::authenticated("u", ["USER"]);
        let anon = SecurityContext::anonymous();

        assert!(matches!(decide(&Access::PermitAll, &anon), Decision::Allow));
        assert!(matches!(decide(&Access::Authenticated, &anon), Decision::Unauthenticated));
        assert!(matches!(decide(&Access::Authenticated, &user), Decision::Allow));
        assert!(matches!(decide(&Access::HasRole("ADMIN".into()), &anon), Decision::Unauthenticated));
        assert!(matches!(decide(&Access::HasRole("ADMIN".into()), &user), Decision::Forbidden));
        assert!(matches!(decide(&Access::HasRole("ADMIN".into()), &admin), Decision::Allow));
        assert!(matches!(
            decide(&Access::HasAnyRole(vec!["ADMIN".into(), "STAFF".into()]), &admin),
            Decision::Allow
        ));
        assert!(matches!(decide(&Access::DenyAll, &admin), Decision::Forbidden));
        assert!(matches!(decide(&Access::DenyAll, &anon), Decision::Unauthenticated));
    }

    #[test]
    fn more_pattern_edge_cases() {
        // A `/**` prefix must not leak past a segment boundary.
        assert!(!path_matches("/a/**", "/ab/c"), "/a is not a prefix segment of /ab");
        assert!(path_matches("/a/**", "/a/b/c"));
        // Root and catch-all.
        assert!(path_matches("/**", "/"));
        assert!(path_matches("/**", "/anything"));
        // `/*` rejects the bare prefix and deeper paths.
        assert!(!path_matches("/x/*", "/x"));
        assert!(!path_matches("/x/*", "/x/a/b"));
        assert!(path_matches("/x/*", "/x/a"));
        // Exact never matches a longer path.
        assert!(!path_matches("/api", "/api/v1"));
    }

    #[test]
    fn the_layer_allows_challenges_and_forbids_through_the_chain() {
        let layer = HttpSecurity::new()
            .permit_all("/public/**")
            .has_role("/admin/**", "ADMIN")
            .any_request(Access::Authenticated)
            .build();
        let admin = || SecurityContext::authenticated("a", ["ADMIN"]);
        let user = || SecurityContext::authenticated("u", ["USER"]);

        // Public — anyone reaches the handler.
        assert_eq!(enforce(&layer, "/public/x", SecurityContext::anonymous()), StatusCode::OK);
        // Admin path — role gate.
        assert_eq!(enforce(&layer, "/admin/x", admin()), StatusCode::OK);
        assert_eq!(enforce(&layer, "/admin/x", user()), StatusCode::FORBIDDEN);
        assert_eq!(enforce(&layer, "/admin/x", SecurityContext::anonymous()), StatusCode::UNAUTHORIZED);
        // Default (authenticated) — login required.
        assert_eq!(enforce(&layer, "/anything", user()), StatusCode::OK);
        assert_eq!(enforce(&layer, "/anything", SecurityContext::anonymous()), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn a_missing_security_context_is_anonymous() {
        // No auth middleware set a context → the layer must treat it as anonymous.
        let layer = HttpSecurity::new().any_request(Access::Authenticated).build();
        let app = AppContext::new();
        let scope = RequestScope::new(&app); // nothing set
        let terminal: &Terminal =
            &|_req, _scope| Box::pin(async { Response::new(StatusCode::OK) }) as BoxFuture<'static, Response>;
        let response = block_on(layer.handle(Request::new("GET", "/x"), &scope, Next { rest: &[], terminal }));
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "no context → 401");
    }

    #[test]
    fn permit_all_reaches_the_handler_even_when_anonymous_and_denyall_blocks_admin() {
        let layer = HttpSecurity::new()
            .permit_all("/open/**")
            .deny_all("/blocked/**")
            .any_request(Access::PermitAll)
            .build();
        // permit_all → 200 for anyone.
        assert_eq!(enforce(&layer, "/open/x", SecurityContext::anonymous()), StatusCode::OK);
        // deny_all → 403 even for an admin, 401 for anonymous.
        assert_eq!(
            enforce(&layer, "/blocked/x", SecurityContext::authenticated("a", ["ADMIN"])),
            StatusCode::FORBIDDEN
        );
        assert_eq!(enforce(&layer, "/blocked/x", SecurityContext::anonymous()), StatusCode::UNAUTHORIZED);
    }
}
