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

use di_macro::controller;
use kernway_security::SecurityContext;
use kernway_server::{
    BoxFuture, KernwayApp, Middleware, Next, Request, RequestScope, Response, StatusCode,
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
        .build()
}
