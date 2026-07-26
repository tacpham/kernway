//! hello-controller — `#[controller]`/`#[route]`/`#[require_role]` and the security
//! relationship: role-based access to an API.
//!
//! - `GET  /users/{id}` — public.
//! - `DELETE /users/{id}` — `#[require_role("ADMIN")]`: only an ADMIN may delete;
//!   anyone else gets `403`.
//!
//! The role check reads the `SecurityContext` an auth middleware put in the request
//! scope (KEP-0005). Here a demo middleware derives it from an `X-Role` header; a
//! real app authenticates a session/token (see login-htmx). That is the whole
//! relationship: **the controller declares the required role, the auth middleware
//! supplies the identity, the request scope carries it between them.**

use std::sync::Arc;

use di_macro::{controller, Validate};
use kernway_security::SecurityContext;
use kernway_server::{
    BanFilter, Bans, BoxFuture, KernwayApp, Middleware, Next, Path, Request, RequestScope, Response,
    StatusCode, Validated, VisitorTracking,
};

/// Demo auth: turn an `X-Role` header into a `SecurityContext`. No header →
/// anonymous. (A real app reads a session cookie / bearer token instead.)
struct HeaderAuth;

impl Middleware for HeaderAuth {
    fn name(&self) -> &'static str {
        "HeaderAuth"
    }
    fn handle<'a>(&'a self, req: Request, scope: &'a RequestScope, next: Next<'a>) -> BoxFuture<'a, Response> {
        let ctx = match req.header("x-role") {
            Some(role) => SecurityContext::authenticated("demo-user", [role.to_string()]),
            None => SecurityContext::anonymous(),
        };
        scope.set(ctx);
        next.run(req, scope)
    }
}

/// The controller. Real fields would be injected dependencies (a service, a repo);
/// here it is stateless.
pub struct UserController;

#[controller("/users")]
impl UserController {
    /// Public — anyone may read a user.
    #[route(GET, "/{id}")]
    async fn get_user(&self, req: Request) -> Response {
        let id = req.path_params.get("id").cloned().unwrap_or_default();
        json_ok(&format!(r#"{{"id":"{id}"}}"#))
    }

    /// Admin-only — deleting a user requires the ADMIN role, else `403`.
    #[route(DELETE, "/{id}")]
    #[require_role("ADMIN")]
    async fn delete_user(&self, req: Request) -> Response {
        let id = req.path_params.get("id").cloned().unwrap_or_default();
        json_ok(&format!(r#"{{"deleted":"{id}"}}"#))
    }
}

/// A validated request body for the typed-args controller.
#[derive(serde::Deserialize, Validate)]
struct NewItem {
    #[validate(not_blank, length(min = 2, max = 40))]
    name: String,
}

/// A controller using **typed argument extraction**: a `Path<u64>`, a
/// `Validated<T>` body (auto-400 on invalid input, before the method runs), and the
/// `SecurityContext` — extracted by the `#[controller]` macro, no manual parsing.
pub struct ItemController;

#[controller("/items")]
impl ItemController {
    /// `Path<u64>` — the id parsed and typed (a non-numeric id → 400).
    #[route(GET, "/{id}")]
    async fn show(&self, id: Path<u64>) -> Response {
        json_ok(&format!(r#"{{"item":{}}}"#, *id))
    }

    /// `Validated<NewItem>` — the body is validated before the method body runs;
    /// an invalid body never reaches here (the extractor returns a 400).
    #[route(POST, "")]
    async fn create(&self, body: Validated<NewItem>) -> Response {
        json_ok(&format!(r#"{{"created":"{}"}}"#, body.name))
    }

    /// `SecurityContext` — the current identity, extracted from the scope.
    #[route(GET, "/whoami")]
    async fn whoami(&self, ctx: SecurityContext) -> Response {
        json_ok(&format!(
            r#"{{"user":"{}","admin":{}}}"#,
            ctx.principal().unwrap_or("anonymous"),
            ctx.has_role("ADMIN")
        ))
    }
}

fn json_ok(body: &str) -> Response {
    Response::new(StatusCode::OK)
        .content_type("application/json; charset=utf-8")
        .body(body.as_bytes().to_vec())
}

/// Build the app bound to `addr`. Shared by the binary and the socket test.
pub fn build_app(addr: &str) -> KernwayApp {
    KernwayApp::builder()
        .bind(addr)
        .layer(HeaderAuth)
        .controller(Arc::new(UserController))
        .controller(Arc::new(ItemController))
        .build()
}

/// Visitor tracking + a runtime ban list: every request gets a `kw_visitor` cookie
/// (first visit), and `BanFilter` rejects a banned IP/UA before the handler. The
/// caller keeps a `Bans` handle to `ban_ip`/`unban_ip` while the server runs.
pub fn build_app_tracked(addr: &str, bans: Bans) -> KernwayApp {
    KernwayApp::builder()
        .bind(addr)
        .layer(VisitorTracking::new()) // sets kw_visitor + RequestMeta
        .layer(BanFilter::new(bans)) // rejects banned requests early
        .get("/hello", |_req: Request, _scope: &RequestScope| async { json_ok(r#"{"ok":true}"#) })
        .build()
}

/// Live activity: `VisitorTracking` + `ActivityTracking` record every request into a
/// shared [`InMemoryActivity`], so an admin view can list who is on the site and the
/// page each is on. The caller keeps the store to query `active(now)`.
#[cfg(feature = "presence")]
pub fn build_app_activity(addr: &str, activity: std::sync::Arc<kernway_server::InMemoryActivity>) -> KernwayApp {
    use kernway_server::ActivityTracking;

    KernwayApp::builder()
        .bind(addr)
        .layer(VisitorTracking::new()) // sets kw_visitor + RequestMeta
        .layer(ActivityTracking::new(activity)) // records the request into the store
        .get("/hello", |_req: Request, _scope: &RequestScope| async { json_ok(r#"{"ok":true}"#) })
        .get("/reports", |_req: Request, _scope: &RequestScope| async { json_ok(r#"{"page":"reports"}"#) })
        .build()
}

/// JWT bearer auth + role-based access: `BearerAuth` turns `Authorization: Bearer
/// <jwt>` into a `SecurityContext`, then `HttpSecurity` enforces it. No token (or an
/// invalid one) is anonymous — `/me` needs a login, `/admin/**` needs ADMIN.
#[cfg(feature = "jwt")]
pub fn build_app_bearer(addr: &str, secret: &str) -> KernwayApp {
    use kernway_server::{Access, BearerAuth, HttpSecurity};

    let security = HttpSecurity::new()
        .has_role("/admin/**", "ADMIN")
        .any_request(Access::Authenticated)
        .build();

    KernwayApp::builder()
        .bind(addr)
        .layer(BearerAuth::new(secret)) // Bearer JWT → identity
        .layer(security) // path rules → authorization
        .get("/me", |_req: Request, _scope: &RequestScope| async { json_ok(r#"{"me":true}"#) })
        .get("/admin/panel", |_req: Request, _scope: &RequestScope| async { json_ok(r#"{"admin":true}"#) })
        .build()
}

/// The alternative to per-route `#[require_role]`: **central** path-based rules
/// (Spring's `HttpSecurity`). One place declares public/authenticated/role paths;
/// the `SecurityLayer` enforces them before any handler. Auth (identity) is still
/// the upstream `HeaderAuth`; this is authorization.
pub fn build_app_secured(addr: &str) -> KernwayApp {
    use kernway_server::{Access, HttpSecurity};

    let security = HttpSecurity::new()
        .permit_all("/public/**") // open
        .has_role("/admin/**", "ADMIN") // ADMIN only
        .any_request(Access::Authenticated) // everything else needs a login
        .build();

    KernwayApp::builder()
        .bind(addr)
        .layer(HeaderAuth) // sets the SecurityContext (identity)
        .layer(security) // enforces the path rules (authorization)
        .get("/public/info", |_req: Request, _scope: &RequestScope| async { json_ok(r#"{"public":true}"#) })
        .get("/secret/data", |_req: Request, _scope: &RequestScope| async { json_ok(r#"{"secret":true}"#) })
        .get("/admin/panel", |_req: Request, _scope: &RequestScope| async { json_ok(r#"{"admin":true}"#) })
        .build()
}
