//! kernleaf × kernway-security, with the *real* types — the integration the unit
//! tests (which use a local mock) do not cover.
//!
//! Proves the security half of a login flow end to end: a `SecurityContext` drives
//! `th:authorize`, a real `CsrfToken` is auto-injected into the form, and the
//! server-side `verify_request` accepts the matching submission and rejects a
//! forged one.

use kernleaf::{Kernleaf, RenderContext};
use kernway_core::request::Request;
use kernway_core::template::Value;
use kernway_security::{csrf, SecurityContext};

/// A page that shows different content per role, plus a login form (for the
/// anonymous case) that must carry a CSRF field.
const PAGE: &str = "\
<div th:authorize=\"isAuthenticated()\">Welcome<span th:authorize=\"hasRole('ADMIN')\"> admin</span></div>\
<form th:authorize=\"isAnonymous()\" method=\"post\" action=\"/login\" hx-post=\"/login\">\
<input name=\"username\"><button>Login</button>\
</form>";

fn engine() -> Kernleaf {
    let mut e = Kernleaf::new();
    e.add("page", PAGE).unwrap();
    e
}

#[test]
fn an_admin_sees_the_panel_and_no_login_form() {
    let e = engine();
    let ctx = SecurityContext::authenticated("alice", ["ADMIN"]);
    let out = e
        .render_with("page", &Value::Null, &RenderContext::new().authorize(&ctx))
        .unwrap();
    // Authenticated → greeting; ADMIN role → the extra span; authenticated → no form.
    assert_eq!(out, "<div>Welcome<span> admin</span></div>");
}

#[test]
fn a_non_admin_is_authenticated_but_has_no_panel() {
    let e = engine();
    let ctx = SecurityContext::authenticated("bob", ["USER"]);
    let out = e
        .render_with("page", &Value::Null, &RenderContext::new().authorize(&ctx))
        .unwrap();
    assert!(out.contains("Welcome"), "greeting shown");
    assert!(!out.contains("admin"), "no admin panel for a USER: {out}");
}

#[test]
fn an_anonymous_visitor_gets_the_form_with_a_real_csrf_token() {
    let e = engine();
    let token = csrf::CsrfToken::generate();
    let anon = SecurityContext::anonymous();

    let out = e
        .render_with(
            "page",
            &Value::Null,
            &RenderContext::new().authorize(&anon).csrf(token.as_str()),
        )
        .unwrap();

    assert!(!out.contains("Welcome"), "anonymous sees no greeting");
    assert!(out.contains("<form"), "the login form is shown");
    // The auto-injected CSRF field carries the real 64-hex token.
    let field = format!(
        "<input type=\"hidden\" name=\"_csrf\" value=\"{}\">",
        token.as_str()
    );
    assert!(
        out.contains(&field),
        "form must carry the CSRF token: {out}"
    );
}

#[test]
fn the_rendered_token_round_trips_through_verify_request() {
    // The token the page rendered is what a legit POST submits — and it verifies.
    let token = csrf::CsrfToken::generate();

    // A legitimate submission: the same token in the cookie and the form body.
    let mut good = Request::new("POST", "/login");
    good.headers.insert("cookie", &token.set_cookie(false));
    good.body = format!("username=alice&_csrf={}", token.as_str()).into_bytes();
    assert!(csrf::verify_request(&good), "a matching token must verify");

    // A forged submission: attacker guesses a different token.
    let mut forged = Request::new("POST", "/login");
    forged.headers.insert("cookie", &token.set_cookie(false));
    forged.body = b"username=alice&_csrf=deadbeef".to_vec();
    assert!(
        !csrf::verify_request(&forged),
        "a mismatched token must be rejected"
    );
}
