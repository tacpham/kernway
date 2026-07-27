#![allow(clippy::doc_overindented_list_items)] // intentional aligned continuation lines
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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kernway_core::error::StatusCode;
use kernway_core::request::Request;
use kernway_core::response::{IntoResponse, Response};
use kernway_core::template::Value;
use kernway_htmx::HtmxResponse;
use kernway_security::session::{self, SessionConfig, SessionManager, MemorySessionStore};
use kernway_security::{csrf, InMemoryPresence, Presence, SecurityContext, SecurityHeaders};
use kernway_server::{BoxFuture, KernwayApp, Middleware, Next, RequestScope};
use kernleaf::{Kernleaf, RenderContext};

/// Unix seconds now — the clock for heartbeats.
fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

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
        Box::pin(async move {
            // Authenticate (awaits the session store) before moving the request on.
            let ctx = {
                let token = req.header("cookie").and_then(session::token_from_cookie);
                self.sessions.authenticate(token).await
            };
            scope.set(ctx);
            next.run(req, scope).await
        })
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
<html lang="en"><head><meta charset="utf-8"><title>Home</title>
<script src="https://unpkg.com/htmx.org@2.0.3"></script></head>
<body>
<h1>Welcome, <span th:text="${user}">user</span></h1>
<div th:authorize="hasRole('ADMIN')"><p>Admin panel — you can see this.</p></div>

<!-- Presence: POST a heartbeat on load and every 10s, so the server knows this
     tab is alive. hx-swap="none" — the 204 reply changes nothing on the page. -->
<div hx-post="/heartbeat" hx-trigger="load, every 10s" hx-swap="none"></div>

<!-- Who is online right now — a fragment refreshed every 5s (htmx polling). -->
<section>
  <h2>Who's online</h2>
  <div id="online" hx-get="/who" hx-trigger="load, every 5s">Loading…</div>
</section>

<form method="post" action="/logout" hx-post="/logout"><button>Log out</button></form>
</body></html>"##;

/// The `/who` fragment — the online list, rendered with kernleaf's `th:each`
/// (standard Thymeleaf iteration). Swapped into `#online` by the poll above.
const WHO_FRAGMENT: &str = r##"<p><strong th:text="${count}">0</strong> online</p>
<ul>
  <li th:each="u : ${users}" th:text="${u}">name</li>
</ul>"##;

/// Build the app bound to `addr`. Shared by the binary and the socket tests.
pub fn build_app(addr: &str) -> KernwayApp {
    let mut engine = Kernleaf::new();
    engine.add("login", LOGIN_PAGE).expect("login template");
    engine.add("protected", PROTECTED_PAGE).expect("protected template");
    engine.add("who", WHO_FRAGMENT).expect("who template");
    let engine = Arc::new(engine);

    // The session subsystem: an in-memory registry, a signing key, hour-long tokens.
    // (A real app loads the key from config/secrets, not a literal.)
    let sessions = Arc::new(SessionManager::new(
        Box::new(MemorySessionStore::new()),
        "demo-signing-key-change-me",
        SessionConfig { token_ttl: Duration::from_secs(3600), ..SessionConfig::default() },
    ));

    // Presence: who is *online* (beat within 30s), separate from who has a session.
    // In-memory here; swap `InMemoryPresence` for `RedisPresence` to share liveness
    // across instances — the handlers only see the `Presence` trait.
    let presence: Arc<dyn Presence> = Arc::new(InMemoryPresence::new(Duration::from_secs(30)));

    let e_login = Arc::clone(&engine);
    let e_prot = Arc::clone(&engine);
    let e_who = Arc::clone(&engine);
    let s_post = Arc::clone(&sessions);
    let s_logout = Arc::clone(&sessions);
    let p_beat = Arc::clone(&presence);
    let p_who = Arc::clone(&presence);

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
            async move { do_login(&req, &s).await }
        })
        // /protected reads the SecurityContext the middleware set — pulled from the
        // scope synchronously, then owned by the future.
        .get("/protected", move |_req: Request, scope: &RequestScope| {
            let ctx = scope.get::<SecurityContext>().expect("auth middleware set a SecurityContext");
            let e = Arc::clone(&e_prot);
            async move { show_protected(&ctx, &e) }
        })
        // Heartbeat: the logged-in user's tab beating. Reads the SecurityContext
        // from the scope; anonymous requests are a no-op.
        .post("/heartbeat", move |_req: Request, scope: &RequestScope| {
            let ctx = scope.get::<SecurityContext>().expect("auth middleware set a SecurityContext");
            let p = Arc::clone(&p_beat);
            async move { do_heartbeat(&ctx, p.as_ref()).await }
        })
        // Who's online now — the fragment the protected page polls. Behind auth:
        // the online list is activity information, not for anonymous callers.
        .get("/who", move |_req: Request, scope: &RequestScope| {
            let ctx = scope.get::<SecurityContext>().expect("auth middleware set a SecurityContext");
            let p = Arc::clone(&p_who);
            let e = Arc::clone(&e_who);
            async move { show_who(&ctx, p.as_ref(), &e).await }
        })
        .post("/logout", move |req: Request, _scope: &RequestScope| {
            let s = Arc::clone(&s_logout);
            async move { do_logout(&req, &s).await }
        })
        .build()
}

/// POST /heartbeat — mark the authenticated user alive as of now.
async fn do_heartbeat(ctx: &SecurityContext, presence: &dyn Presence) -> Response {
    if let Some(user) = ctx.principal() {
        // Best-effort: a presence-store hiccup should not fail the page.
        if let Err(err) = presence.heartbeat(user, now()).await {
            kernway_log::warn!(target: "login_htmx", "heartbeat failed: {err}");
        }
    }
    Response::new(StatusCode::NO_CONTENT)
}

/// GET /who — the online list as a kernleaf fragment (`th:each`), for authenticated
/// callers only (the list is activity information, not public).
async fn show_who(ctx: &SecurityContext, presence: &dyn Presence, engine: &Kernleaf) -> Response {
    if !ctx.is_authenticated() {
        return html_response("<p>Log in to see who's online.</p>".to_string());
    }
    let users = presence.online(now()).await.unwrap_or_default();
    let count = users.len();
    let model = Value::map([
        ("count", Value::Int(count as i64)),
        ("users", Value::seq(users.into_iter().map(Value::from))),
    ]);
    let html = engine
        .render_with("who", &model, &RenderContext::new())
        .unwrap_or_else(|e| format!("<p>template error: {e}</p>"));
    html_response(html)
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
async fn do_login(req: &Request, sessions: &SessionManager) -> Response {
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

    let token = match sessions.login("alice", vec!["ADMIN".to_string()], "web").await {
        Ok(t) => t,
        // The store (Redis, when configured) was unreachable, or the registry is
        // full — either way the login did not take, so say so rather than pretend.
        Err(session::LoginError::AtCapacity) => {
            return (StatusCode::SERVICE_UNAVAILABLE, "too many sessions").into_response();
        }
        Err(session::LoginError::Store(_)) => {
            return (StatusCode::SERVICE_UNAVAILABLE, "the session store is unavailable").into_response();
        }
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
async fn do_logout(req: &Request, sessions: &SessionManager) -> Response {
    if let Some(token) = req.header("cookie").and_then(session::token_from_cookie) {
        // Best-effort server-side revocation: if the store is down we still clear
        // the cookie below (client-side logout), but log that the session lingers.
        if let Err(err) = sessions.logout_token(token).await {
            kernway_log::warn!(target: "login_htmx", "logout could not revoke the session: {err}");
        }
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
