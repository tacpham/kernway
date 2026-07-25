//! login-htmx — a walking-skeleton login flow: kernleaf + kernway-security + htmx.
//!
//! The whole KEP-0004 loop, over the real server:
//!
//! - `GET  /login`     → the login form (kernleaf), auto-CSRF field, CSRF cookie.
//! - `POST /login`     → verify CSRF, check credentials, issue a **session** (a
//!                       signed token in an HttpOnly cookie), `HX-Redirect` to
//!                       `/protected`.
//! - `GET  /protected` → authenticate the session cookie into a `SecurityContext`;
//!                       render role-gated content, or bounce to `/login`.
//! - `POST /logout`    → revoke the session (registry row removed), clear the
//!                       cookie, `HX-Redirect` to `/login`.
//!
//! Demo credentials: `alice` / `secret` (an ADMIN). Wiring uses closure captures;
//! the injectable request-scoped `SecurityContext` is KEP-0005, later.

use std::sync::Arc;
use std::time::Duration;

use kernway_core::error::StatusCode;
use kernway_core::request::Request;
use kernway_core::response::{IntoResponse, Response};
use kernway_core::template::Value;
use kernway_htmx::HtmxResponse;
use kernway_security::session::{self, SessionConfig, SessionManager, MemorySessionStore};
use kernway_security::{csrf, SecurityContext, SecurityHeaders};
use kernway_server::{BoxFuture, KernwayApp, Middleware, Next, RequestScope};
use kernleaf::{Kernleaf, RenderContext};

/// The auth middleware (KEP-0005/0006): every request, turn the session cookie into
/// a `SecurityContext` and put it in the request scope. Downstream handlers and the
/// template read it from the scope instead of authenticating themselves.
struct Authenticate {
    sessions: Arc<SessionManager>,
}

impl Middleware for Authenticate {
    fn name(&self) -> &'static str {
        "Authenticate"
    }
    fn handle<'a>(&'a self, req: Request, scope: &'a RequestScope, next: Next<'a>) -> BoxFuture<'a, Response> {
        // Compute the context before moving the request into the chain.
        let ctx = {
            let token = req.header("cookie").and_then(session::token_from_cookie);
            self.sessions.authenticate(token)
        };
        scope.set(ctx);
        next.run(req, scope)
    }
}

const LOGIN_PAGE: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Log in</title>
<script src="https://unpkg.com/htmx.org@2.0.3"></script></head>
<body>
<h1>Log in</h1>
<form method="post" action="/login" hx-post="/login" hx-target="#result">
<input name="username" placeholder="Username">
<input name="password" type="password" placeholder="Password">
<button type="submit">Log in</button>
</form>
<div id="result"></div>
</body></html>"##;

const PROTECTED_PAGE: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Home</title></head>
<body>
<h1>Welcome, <span th:text="${user}">user</span></h1>
<div th:authorize="hasRole('ADMIN')"><p>Admin panel — you can see this.</p></div>
<form method="post" action="/logout" hx-post="/logout"><button>Log out</button></form>
</body></html>"##;

/// Build the app bound to `addr`. Shared by the binary and the socket tests.
pub fn build_app(addr: &str) -> KernwayApp {
    let mut engine = Kernleaf::new();
    engine.add("login", LOGIN_PAGE).expect("login template");
    engine.add("protected", PROTECTED_PAGE).expect("protected template");
    let engine = Arc::new(engine);

    // The session subsystem: an in-memory registry, a signing key, hour-long tokens.
    // (A real app loads the key from config/secrets, not a literal.)
    let sessions = Arc::new(SessionManager::new(
        Box::new(MemorySessionStore::new()),
        "demo-signing-key-change-me",
        SessionConfig { token_ttl: Duration::from_secs(3600), ..SessionConfig::default() },
    ));

    let e_login = Arc::clone(&engine);
    let e_prot = Arc::clone(&engine);
    let s_post = Arc::clone(&sessions);
    let s_logout = Arc::clone(&sessions);

    KernwayApp::builder()
        .bind(addr)
        // The auth middleware runs on every request and populates the scope.
        .layer(Authenticate { sessions: Arc::clone(&sessions) })
        .get("/login", move |_req: Request, _scope: &RequestScope| {
            let e = Arc::clone(&e_login);
            async move { render_login(&e) }
        })
        .post("/login", move |req: Request, _scope: &RequestScope| {
            let s = Arc::clone(&s_post);
            async move { do_login(&req, &s) }
        })
        // /protected reads the SecurityContext the middleware set — pulled from the
        // scope synchronously, then owned by the future.
        .get("/protected", move |_req: Request, scope: &RequestScope| {
            let ctx = scope.get::<SecurityContext>().expect("auth middleware set a SecurityContext");
            let e = Arc::clone(&e_prot);
            async move { show_protected(&ctx, &e) }
        })
        .post("/logout", move |req: Request, _scope: &RequestScope| {
            let s = Arc::clone(&s_logout);
            async move { do_logout(&req, &s) }
        })
        .build()
}

/// GET /login — the form, with a fresh CSRF token in both the field and the cookie.
fn render_login(engine: &Kernleaf) -> Response {
    let token = csrf::CsrfToken::generate();
    let html = engine
        .render_with("login", &Value::Null, &RenderContext::new().csrf(token.as_str()))
        .unwrap_or_else(|e| format!("<p>template error: {e}</p>"));
    let mut resp = html_response(html);
    resp.headers.insert("set-cookie", &token.set_cookie(false));
    SecurityHeaders::strict().apply(&mut resp);
    resp
}

/// POST /login — verify CSRF, check credentials, issue a session.
fn do_login(req: &Request, sessions: &SessionManager) -> Response {
    // CSRF first: a state-changing request must carry a matching token.
    if !csrf::verify_request(req) {
        return (StatusCode::FORBIDDEN, "CSRF check failed").into_response();
    }
    let body = std::str::from_utf8(&req.body).unwrap_or("");
    let username = csrf::form_field(body, "username").unwrap_or_default();
    let password = csrf::form_field(body, "password").unwrap_or_default();

    // Demo credential check — a real app verifies a password hash here.
    if username != "alice" || password != "secret" {
        return HtmxResponse::new("<p id=\"result\">Wrong username or password.</p>").into_response();
    }

    let token = match sessions.login("alice", vec!["ADMIN".to_string()], "web") {
        Ok(t) => t,
        Err(_) => return (StatusCode::SERVICE_UNAVAILABLE, "too many sessions").into_response(),
    };

    // Set the session cookie and tell htmx to navigate to the protected page.
    let mut resp = HtmxResponse::new("").redirect("/protected").into_response();
    resp.headers.insert("set-cookie", &session::set_cookie(&token, false));
    resp
}

/// GET /protected — render role-gated content from the `SecurityContext` the auth
/// middleware put in the scope (KEP-0005), or bounce to /login.
fn show_protected(ctx: &SecurityContext, engine: &Kernleaf) -> Response {
    if !ctx.is_authenticated() {
        // Not logged in → send them to the login page.
        return HtmxResponse::new("").redirect("/login").into_response();
    }

    let user = ctx.principal().unwrap_or("").to_string();
    let model = Value::map([("user", Value::from(user))]);
    let html = engine
        .render_with("protected", &model, &RenderContext::new().authorize(ctx))
        .unwrap_or_else(|e| format!("<p>template error: {e}</p>"));
    let mut resp = html_response(html);
    SecurityHeaders::strict().apply(&mut resp);
    resp
}

/// POST /logout — revoke the session and clear the cookie.
fn do_logout(req: &Request, sessions: &SessionManager) -> Response {
    if let Some(token) = req.header("cookie").and_then(session::token_from_cookie) {
        sessions.logout_token(token);
    }
    let mut resp = HtmxResponse::new("").redirect("/login").into_response();
    resp.headers.insert("set-cookie", &session::clear_cookie());
    resp
}

fn html_response(html: String) -> Response {
    Response::new(StatusCode::OK)
        .content_type("text/html; charset=utf-8")
        .body(html.into_bytes())
}
