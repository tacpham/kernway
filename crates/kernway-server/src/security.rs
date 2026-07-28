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
use kernway_security::{SecurityContext, SecurityHeaders};

use crate::app::{forbidden, unauthorized};
use crate::middleware::{Middleware, Next};

/// [`SecurityHeaders`] as a middleware: add the headers to **every** response
/// (`app.layer(SecurityHeaders::strict())`), instead of applying them per handler.
/// This is the working replacement for the removed `SecurityHeadersLayer` (which
/// implemented a superseded pre-KEP-0006 `Layer` trait).
impl Middleware for SecurityHeaders {
    fn name(&self) -> &'static str {
        "SecurityHeaders"
    }

    fn handle<'a>(
        &'a self,
        req: Request,
        scope: &'a RequestScope,
        next: Next<'a>,
    ) -> BoxFuture<'a, Response> {
        Box::pin(async move {
            let mut response = next.run(req, scope).await;
            self.apply(&mut response);
            response
        })
    }
}

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

/// One rule: an optional HTTP method (`None` = any), a path pattern, and the access
/// it grants.
type Rule = (Option<String>, String, Access);

/// A builder of path-based access rules (Spring's `HttpSecurity`). Rules match in
/// declaration order (first match wins); [`any_request`](HttpSecurity::any_request)
/// is the fallback.
pub struct HttpSecurity {
    rules: Vec<Rule>,
    default: Access,
    login_redirect: Option<String>,
}

impl HttpSecurity {
    /// A fresh policy whose fallback is `Authenticated` (secure by default — an
    /// unlisted path needs a login until you say otherwise).
    #[must_use]
    pub fn new() -> Self {
        Self {
            rules: Vec::new(),
            default: Access::Authenticated,
            login_redirect: None,
        }
    }

    /// Open a path pattern to everyone (any method).
    #[must_use]
    pub fn permit_all(self, pattern: &str) -> Self {
        self.rule(None, pattern, Access::PermitAll)
    }

    /// Require any authenticated user on a pattern (any method).
    #[must_use]
    pub fn authenticated(self, pattern: &str) -> Self {
        self.rule(None, pattern, Access::Authenticated)
    }

    /// Require `role` on a pattern (any method).
    #[must_use]
    pub fn has_role(self, pattern: &str, role: &str) -> Self {
        self.rule(None, pattern, Access::HasRole(role.to_string()))
    }

    /// Require any of `roles` on a pattern (any method).
    #[must_use]
    pub fn has_any_role(self, pattern: &str, roles: &[&str]) -> Self {
        self.rule(
            None,
            pattern,
            Access::HasAnyRole(roles.iter().map(|r| (*r).to_string()).collect()),
        )
    }

    /// Deny a pattern outright (any method).
    #[must_use]
    pub fn deny_all(self, pattern: &str) -> Self {
        self.rule(None, pattern, Access::DenyAll)
    }

    /// A **method-aware** rule (Spring's `requestMatchers(HttpMethod.POST, "/x")`):
    /// only requests with `method` on `pattern` match it. So `POST /articles` can
    /// need ADMIN while `GET /articles` is open.
    #[must_use]
    pub fn request(self, method: &str, pattern: &str, access: Access) -> Self {
        self.rule(Some(method.to_ascii_uppercase()), pattern, access)
    }

    /// Add a rule directly (optional method + pattern + access).
    #[must_use]
    fn rule(mut self, method: Option<String>, pattern: &str, access: Access) -> Self {
        self.rules.push((method, pattern.to_string(), access));
        self
    }

    /// The fallback for a request no rule matched (Spring's `anyRequest()`).
    #[must_use]
    pub fn any_request(mut self, access: Access) -> Self {
        self.default = access;
        self
    }

    /// Redirect an **unauthenticated** request to `path` (a `302`) instead of
    /// returning `401` — Spring's `RedirectServerAuthenticationEntryPoint` /
    /// `formLogin().loginPage(...)`. This is what makes a browser app "you must log
    /// in to enter": a visitor without a session is bounced to the login page. A
    /// missing *role* (an authenticated user) still gets `403`, not a redirect.
    #[must_use]
    pub fn login_page(mut self, path: &str) -> Self {
        self.login_redirect = Some(path.to_string());
        self
    }

    /// Finish into the enforcing middleware — add it with `app.layer(...)`.
    #[must_use]
    pub fn build(self) -> SecurityLayer {
        SecurityLayer {
            rules: Arc::new(self.rules),
            default: self.default,
            login_redirect: self.login_redirect,
        }
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
    rules: Arc<Vec<Rule>>,
    default: Access,
    login_redirect: Option<String>,
}

impl SecurityLayer {
    /// The access rule for a `method` `path` request — the first rule whose method
    /// (or any) and pattern match, else the default.
    fn access_for(&self, method: &str, path: &str) -> &Access {
        self.rules
            .iter()
            .find(|(rule_method, pattern, _)| {
                rule_method.as_deref().is_none_or(|m| m == method) && path_matches(pattern, path)
            })
            .map_or(&self.default, |(_, _, access)| access)
    }

    /// Whether a `method` `path` request from `ctx` is **allowed** by the policy —
    /// the authorization decision without running a request (`false` covers both a
    /// missing login and a missing role). Handy for testing a policy or measuring it.
    #[must_use]
    pub fn allows(&self, method: &str, path: &str, ctx: &SecurityContext) -> bool {
        matches!(decide(self.access_for(method, path), ctx), Decision::Allow)
    }
}

impl Middleware for SecurityLayer {
    fn name(&self) -> &'static str {
        "Security"
    }

    fn handle<'a>(
        &'a self,
        req: Request,
        scope: &'a RequestScope,
        next: Next<'a>,
    ) -> BoxFuture<'a, Response> {
        let anonymous = SecurityContext::anonymous();
        let context = scope.get::<SecurityContext>().ok();
        let decision = decide(
            self.access_for(&req.method, &req.path),
            context.as_deref().unwrap_or(&anonymous),
        );
        match decision {
            Decision::Allow => next.run(req, scope),
            Decision::Unauthenticated => match &self.login_redirect {
                Some(path) => {
                    let mut resp = Response::new(kernway_core::error::StatusCode(302));
                    resp.headers.insert("location", path);
                    Box::pin(async move { resp })
                }
                None => Box::pin(async { unauthorized() }),
            },
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
/// Allocation-free — it runs once per rule per request (the `security` bench).
fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern == "**" || pattern == "/**" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        // `path == prefix`, or `path` continues past `prefix` at a `/` boundary.
        return path == prefix
            || (path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/'));
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        // `prefix/` then exactly one more segment (no further `/`).
        if path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/') {
            let rest = &path[prefix.len() + 1..];
            return !rest.is_empty() && !rest.contains('/');
        }
        return false;
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
        let terminal: &Terminal = &|_req, _scope| {
            Box::pin(async { Response::new(StatusCode::OK) }) as BoxFuture<'static, Response>
        };
        let next = Next {
            rest: &[],
            terminal,
        };
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
    fn login_page_redirects_anonymous_instead_of_401() {
        let with = HttpSecurity::new()
            .any_request(Access::Authenticated)
            .login_page("/login")
            .build();
        // Anonymous → 302 (the entry-point redirect), not 401.
        assert_eq!(enforce(&with, "/", SecurityContext::anonymous()), StatusCode(302));
        // An authenticated user still reaches the handler.
        assert_eq!(
            enforce(&with, "/", SecurityContext::authenticated("u", ["USER".to_string()])),
            StatusCode::OK
        );
        // Without login_page, an anonymous request is a plain 401.
        let without = HttpSecurity::new().any_request(Access::Authenticated).build();
        assert_eq!(
            enforce(&without, "/", SecurityContext::anonymous()),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn first_matching_rule_wins_then_the_default() {
        let layer = HttpSecurity::new()
            .permit_all("/public/**")
            .has_role("/admin/**", "ADMIN")
            .authenticated("/api/**")
            .any_request(Access::PermitAll)
            .build();
        assert!(matches!(
            layer.access_for("GET", "/public/x"),
            Access::PermitAll
        ));
        assert!(matches!(layer.access_for("GET", "/admin/x"), Access::HasRole(r) if r == "ADMIN"));
        assert!(matches!(
            layer.access_for("GET", "/api/x"),
            Access::Authenticated
        ));
        assert!(
            matches!(layer.access_for("GET", "/other"), Access::PermitAll),
            "falls to any_request"
        );
    }

    #[test]
    fn method_aware_rules_match_the_method() {
        // GET /articles/** is open; POST/PUT/DELETE need ADMIN.
        let layer = HttpSecurity::new()
            .request("POST", "/articles/**", Access::HasRole("ADMIN".into()))
            .request("DELETE", "/articles/**", Access::HasRole("ADMIN".into()))
            .permit_all("/articles/**") // any other method (GET) is open
            .any_request(Access::Authenticated)
            .build();
        assert!(
            matches!(layer.access_for("GET", "/articles/1"), Access::PermitAll),
            "GET open"
        );
        assert!(
            matches!(layer.access_for("POST", "/articles"), Access::HasRole(r) if r == "ADMIN")
        );
        assert!(
            matches!(layer.access_for("DELETE", "/articles/1"), Access::HasRole(r) if r == "ADMIN")
        );
        // A method-specific rule does not catch other methods.
        assert!(
            matches!(layer.access_for("PUT", "/articles/1"), Access::PermitAll),
            "PUT falls to the open rule"
        );
    }

    #[test]
    fn decisions_cover_auth_and_role() {
        let admin = SecurityContext::authenticated("a", ["ADMIN"]);
        let user = SecurityContext::authenticated("u", ["USER"]);
        let anon = SecurityContext::anonymous();

        assert!(matches!(decide(&Access::PermitAll, &anon), Decision::Allow));
        assert!(matches!(
            decide(&Access::Authenticated, &anon),
            Decision::Unauthenticated
        ));
        assert!(matches!(
            decide(&Access::Authenticated, &user),
            Decision::Allow
        ));
        assert!(matches!(
            decide(&Access::HasRole("ADMIN".into()), &anon),
            Decision::Unauthenticated
        ));
        assert!(matches!(
            decide(&Access::HasRole("ADMIN".into()), &user),
            Decision::Forbidden
        ));
        assert!(matches!(
            decide(&Access::HasRole("ADMIN".into()), &admin),
            Decision::Allow
        ));
        assert!(matches!(
            decide(
                &Access::HasAnyRole(vec!["ADMIN".into(), "STAFF".into()]),
                &admin
            ),
            Decision::Allow
        ));
        assert!(matches!(
            decide(&Access::DenyAll, &admin),
            Decision::Forbidden
        ));
        assert!(matches!(
            decide(&Access::DenyAll, &anon),
            Decision::Unauthenticated
        ));
    }

    #[test]
    fn more_pattern_edge_cases() {
        // A `/**` prefix must not leak past a segment boundary.
        assert!(
            !path_matches("/a/**", "/ab/c"),
            "/a is not a prefix segment of /ab"
        );
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
        assert_eq!(
            enforce(&layer, "/public/x", SecurityContext::anonymous()),
            StatusCode::OK
        );
        // Admin path — role gate.
        assert_eq!(enforce(&layer, "/admin/x", admin()), StatusCode::OK);
        assert_eq!(enforce(&layer, "/admin/x", user()), StatusCode::FORBIDDEN);
        assert_eq!(
            enforce(&layer, "/admin/x", SecurityContext::anonymous()),
            StatusCode::UNAUTHORIZED
        );
        // Default (authenticated) — login required.
        assert_eq!(enforce(&layer, "/anything", user()), StatusCode::OK);
        assert_eq!(
            enforce(&layer, "/anything", SecurityContext::anonymous()),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn security_headers_middleware_adds_headers_to_every_response() {
        let headers = SecurityHeaders::strict();
        let app = AppContext::new();
        let scope = RequestScope::new(&app);
        let terminal: &Terminal = &|_req, _scope| {
            Box::pin(async { Response::new(StatusCode::OK) }) as BoxFuture<'static, Response>
        };
        let response = block_on(Middleware::handle(
            &headers,
            Request::new("GET", "/"),
            &scope,
            Next {
                rest: &[],
                terminal,
            },
        ));
        assert_eq!(
            response.headers.get("x-frame-options"),
            Some("DENY"),
            "headers applied"
        );
        assert!(
            response.headers.get("content-security-policy").is_some(),
            "CSP applied"
        );
    }

    #[test]
    fn a_missing_security_context_is_anonymous() {
        // No auth middleware set a context → the layer must treat it as anonymous.
        let layer = HttpSecurity::new()
            .any_request(Access::Authenticated)
            .build();
        let app = AppContext::new();
        let scope = RequestScope::new(&app); // nothing set
        let terminal: &Terminal = &|_req, _scope| {
            Box::pin(async { Response::new(StatusCode::OK) }) as BoxFuture<'static, Response>
        };
        let response = block_on(layer.handle(
            Request::new("GET", "/x"),
            &scope,
            Next {
                rest: &[],
                terminal,
            },
        ));
        assert_eq!(
            response.status,
            StatusCode::UNAUTHORIZED,
            "no context → 401"
        );
    }

    #[test]
    fn permit_all_reaches_the_handler_even_when_anonymous_and_denyall_blocks_admin() {
        let layer = HttpSecurity::new()
            .permit_all("/open/**")
            .deny_all("/blocked/**")
            .any_request(Access::PermitAll)
            .build();
        // permit_all → 200 for anyone.
        assert_eq!(
            enforce(&layer, "/open/x", SecurityContext::anonymous()),
            StatusCode::OK
        );
        // deny_all → 403 even for an admin, 401 for anonymous.
        assert_eq!(
            enforce(
                &layer,
                "/blocked/x",
                SecurityContext::authenticated("a", ["ADMIN"])
            ),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            enforce(&layer, "/blocked/x", SecurityContext::anonymous()),
            StatusCode::UNAUTHORIZED
        );
    }
}
