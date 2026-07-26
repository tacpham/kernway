//! # kernway-security
//!
//! The web-security primitives a server-rendered app needs: **security response
//! headers**, **CSRF** token issue and verify, and an **authorization context**
//! that `th:authorize` (in `kernleaf`) consults. Each piece is pure and testable;
//! the only third-party dependency is `getrandom`, for the one thing it would be
//! irresponsible to hand-roll — cryptographic randomness for a CSRF token
//! ([KEP-0000 §1](https://github.com/tacpham/kernway/blob/main/docs/kep/0000-principles.md)).
//!
//! ```
//! use kernway_security::{SecurityHeaders, SecurityContext, csrf};
//!
//! // A token to embed in a form and set as a cookie.
//! let token = csrf::CsrfToken::generate();
//! assert_eq!(token.as_str().len(), 64);
//!
//! // Roles for th:authorize.
//! let ctx = SecurityContext::authenticated("alice", ["ADMIN"]);
//! assert!(ctx.has_role("ADMIN"));
//! ```

#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::fmt::Write as _;

pub mod hash;
pub mod token;
pub mod session;

/// Presence / heartbeat — who is online now, distinct from who has a session
/// (feature = `presence`).
#[cfg(feature = "presence")]
pub mod presence;
#[cfg(feature = "presence")]
pub use presence::{InMemoryPresence, Presence};
#[cfg(all(feature = "presence", feature = "redis"))]
pub use presence::RedisPresence;

/// Redis-backed [`SessionStore`](session::SessionStore) — the distributed backend
/// (feature = `redis`).
#[cfg(feature = "redis")]
pub mod redis_store;
#[cfg(feature = "redis")]
pub use redis_store::RedisSessionStore;

// Used by the `csrf` module and the tests via `use super::*`; the lint can't see
// through the glob re-export, so the import looks unused at the top level.
#[allow(unused_imports)]
use kernway_core::request::Request;
use kernway_core::response::Response;

// ============================================================
// Security response headers
// ============================================================

/// A set of security response headers. [`SecurityHeaders::strict`] gives a secure
/// baseline; the builders relax or tighten individual headers.
#[derive(Debug, Clone, Default)]
pub struct SecurityHeaders {
    content_type_options: bool,
    frame_options: Option<String>,
    csp: Option<String>,
    referrer_policy: Option<String>,
    hsts: Option<String>,
    permissions_policy: Option<String>,
}

impl SecurityHeaders {
    /// No headers — build up from nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// A secure baseline: `nosniff`, `X-Frame-Options: DENY`, a restrictive CSP,
    /// `no-referrer`, one year of HSTS, and a locked-down Permissions-Policy.
    /// Loosen individual pieces with the builders where an app genuinely needs to.
    pub fn strict() -> Self {
        Self {
            content_type_options: true,
            frame_options: Some("DENY".into()),
            csp: Some("default-src 'self'".into()),
            referrer_policy: Some("no-referrer".into()),
            hsts: Some("max-age=31536000; includeSubDomains".into()),
            permissions_policy: Some("geolocation=(), microphone=(), camera=()".into()),
        }
    }

    /// Set `Content-Security-Policy`.
    #[must_use]
    pub fn content_security_policy(mut self, value: impl Into<String>) -> Self {
        self.csp = Some(value.into());
        self
    }

    /// Set `X-Frame-Options` (`DENY`, `SAMEORIGIN`).
    #[must_use]
    pub fn frame_options(mut self, value: impl Into<String>) -> Self {
        self.frame_options = Some(value.into());
        self
    }

    /// Set `Referrer-Policy`.
    #[must_use]
    pub fn referrer_policy(mut self, value: impl Into<String>) -> Self {
        self.referrer_policy = Some(value.into());
        self
    }

    /// Set `Strict-Transport-Security`. Only honoured by browsers over HTTPS.
    #[must_use]
    pub fn hsts(mut self, value: impl Into<String>) -> Self {
        self.hsts = Some(value.into());
        self
    }

    /// Turn `X-Content-Type-Options: nosniff` on or off.
    #[must_use]
    pub fn content_type_options(mut self, on: bool) -> Self {
        self.content_type_options = on;
        self
    }

    /// The headers as `(name, value)` pairs, in a stable order.
    pub fn headers(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if self.content_type_options {
            out.push(("x-content-type-options", "nosniff".to_string()));
        }
        if let Some(v) = &self.frame_options {
            out.push(("x-frame-options", v.clone()));
        }
        if let Some(v) = &self.csp {
            out.push(("content-security-policy", v.clone()));
        }
        if let Some(v) = &self.referrer_policy {
            out.push(("referrer-policy", v.clone()));
        }
        if let Some(v) = &self.hsts {
            out.push(("strict-transport-security", v.clone()));
        }
        if let Some(v) = &self.permissions_policy {
            out.push(("permissions-policy", v.clone()));
        }
        out
    }

    /// Add these headers to a response.
    pub fn apply(&self, resp: &mut Response) {
        for (name, value) in self.headers() {
            resp.headers.insert(name, &value);
        }
    }
}

// `SecurityHeaders` is applied to every response by the middleware
// `kernway_server` implements for it (`app.layer(SecurityHeaders::strict())`) —
// it lives there because the async `Middleware` trait does. `SecurityHeaders`
// itself stays here as the header data + `apply`.

// ============================================================
// CSRF — double-submit token
// ============================================================

/// CSRF token issue and verify, using the **double-submit** pattern: the server
/// generates a token, sets it as a cookie *and* renders it into the form; on a
/// state-changing request it checks the two match. Stateless — no server-side
/// session store — and safe with an `HttpOnly` cookie, because the form field is
/// server-rendered, not read from the cookie by JavaScript.
pub mod csrf {
    use super::*;
    use kernway_core::request::Request;

    /// The cookie name the token is stored under.
    pub const COOKIE: &str = "kw_csrf";
    /// The form field name the token is submitted under.
    pub const FIELD: &str = "_csrf";
    /// The request header the token may be submitted under (e.g. from htmx).
    pub const HEADER: &str = "x-csrf-token";

    /// A CSRF token — 32 bytes of OS randomness, hex-encoded (64 chars).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CsrfToken(String);

    impl CsrfToken {
        /// Generate a fresh, unpredictable token.
        ///
        /// # Panics
        /// If the OS RNG is unavailable — a failure as fundamental as a failed
        /// allocation, and not something to paper over with a weak token.
        pub fn generate() -> Self {
            let mut bytes = [0u8; 32];
            getrandom::getrandom(&mut bytes).expect("OS randomness unavailable");
            let mut hex = String::with_capacity(64);
            for b in bytes {
                let _ = write!(hex, "{b:02x}");
            }
            CsrfToken(hex)
        }

        /// Wrap an existing token value (e.g. one read back from a cookie).
        pub fn from_value(value: impl Into<String>) -> Self {
            CsrfToken(value.into())
        }

        /// The token string, to embed in a form or a header.
        pub fn as_str(&self) -> &str {
            &self.0
        }

        /// The `Set-Cookie` header value. `HttpOnly` and `SameSite=Lax` always;
        /// `Secure` when `secure` (i.e. served over HTTPS).
        pub fn set_cookie(&self, secure: bool) -> String {
            let secure = if secure { "; Secure" } else { "" };
            format!("{COOKIE}={}; Path=/; HttpOnly; SameSite=Lax{secure}", self.0)
        }
    }

    /// Constant-time equality — no early return on the first differing byte, so a
    /// verify cannot be turned into a timing oracle for the token.
    pub fn verify(submitted: &str, expected: &str) -> bool {
        let (a, b) = (submitted.as_bytes(), expected.as_bytes());
        if a.len() != b.len() {
            return false;
        }
        let mut diff = 0u8;
        for i in 0..a.len() {
            diff |= a[i] ^ b[i];
        }
        diff == 0
    }

    /// Pull the CSRF token out of a `Cookie` header value.
    pub fn token_from_cookie(cookie_header: &str) -> Option<&str> {
        cookie_header.split(';').find_map(|pair| {
            let (name, value) = pair.split_once('=')?;
            (name.trim() == COOKIE).then(|| value.trim())
        })
    }

    /// Pull a field out of an `application/x-www-form-urlencoded` body.
    pub fn form_field(body: &str, name: &str) -> Option<String> {
        body.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == name).then(|| url_decode(v))
        })
    }

    /// Verify a state-changing request: the token in the `Cookie` must match the
    /// one submitted via the `X-CSRF-Token` header or the `_csrf` form field.
    /// Absent on either side is a failure.
    pub fn verify_request(req: &Request) -> bool {
        let expected = req.header("cookie").and_then(token_from_cookie);
        let submitted = req.header(HEADER).map(str::to_string).or_else(|| {
            std::str::from_utf8(&req.body).ok().and_then(|b| form_field(b, FIELD))
        });
        match (submitted, expected) {
            (Some(s), Some(e)) => verify(&s, e),
            _ => false,
        }
    }

    fn url_decode(s: &str) -> String {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'+' => {
                    out.push(b' ');
                    i += 1;
                }
                b'%' if i + 2 < bytes.len() => {
                    let hi = (bytes[i + 1] as char).to_digit(16);
                    let lo = (bytes[i + 2] as char).to_digit(16);
                    if let (Some(h), Some(l)) = (hi, lo) {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    } else {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
                b => {
                    out.push(b);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

// ============================================================
// Authorization — the security context
// ============================================================

/// Who the current request is running as, and what they may do. This is what a
/// `th:authorize` in a template consults, and what a route guard checks. It is a
/// plain value (thread-per-core means request state needs no synchronisation,
/// [KEP-0000 §4]) — build it in an auth layer, read it in a handler or template.
///
/// [KEP-0000 §4]: https://github.com/tacpham/kernway/blob/main/docs/kep/0000-principles.md
#[derive(Debug, Clone, Default)]
pub struct SecurityContext {
    authenticated: bool,
    principal: Option<String>,
    roles: HashSet<String>,
}

impl SecurityContext {
    /// An unauthenticated request — no principal, no roles.
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// An authenticated request as `principal` with `roles`.
    pub fn authenticated<I, S>(principal: impl Into<String>, roles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            authenticated: true,
            principal: Some(principal.into()),
            roles: roles.into_iter().map(Into::into).collect(),
        }
    }

    /// Whether the request is authenticated.
    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// The principal (username), if authenticated.
    pub fn principal(&self) -> Option<&str> {
        self.principal.as_deref()
    }

    /// Whether the principal has `role`.
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.contains(role)
    }

    /// Whether the principal has *any* of `roles`.
    pub fn has_any_role(&self, roles: &[&str]) -> bool {
        roles.iter().any(|r| self.roles.contains(*r))
    }

    /// The roles held, in no particular order.
    pub fn roles(&self) -> impl Iterator<Item = &str> {
        self.roles.iter().map(String::as_str)
    }
}

/// So a template's `th:authorize` can consult the context through the spec-crate
/// trait, without `kernleaf` depending on this crate.
impl kernway_core::security::Authorization for SecurityContext {
    fn is_authenticated(&self) -> bool {
        self.authenticated
    }
    fn has_role(&self, role: &str) -> bool {
        self.roles.contains(role)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernway_core::error::StatusCode;

    // --- security headers --------------------------------------------------

    #[test]
    fn strict_headers_have_the_secure_baseline() {
        let h = SecurityHeaders::strict();
        let map: std::collections::HashMap<_, _> = h.headers().into_iter().collect();
        assert_eq!(map["x-content-type-options"], "nosniff");
        assert_eq!(map["x-frame-options"], "DENY");
        assert!(map["content-security-policy"].contains("default-src 'self'"));
        assert_eq!(map["referrer-policy"], "no-referrer");
        assert!(map["strict-transport-security"].contains("max-age="));
    }

    #[test]
    fn an_empty_set_emits_nothing() {
        assert!(SecurityHeaders::new().headers().is_empty());
    }

    #[test]
    fn a_builder_overrides_one_header() {
        let h = SecurityHeaders::strict().frame_options("SAMEORIGIN");
        let map: std::collections::HashMap<_, _> = h.headers().into_iter().collect();
        assert_eq!(map["x-frame-options"], "SAMEORIGIN");
    }

    #[test]
    fn apply_adds_headers_to_a_response() {
        let mut resp = Response::new(StatusCode::OK);
        SecurityHeaders::strict().apply(&mut resp);
        assert_eq!(resp.headers.get("x-frame-options"), Some("DENY"));
        assert_eq!(resp.headers.get("x-content-type-options"), Some("nosniff"));
    }

    // --- csrf --------------------------------------------------------------

    #[test]
    fn a_token_is_64_hex_chars_and_unpredictable() {
        let a = csrf::CsrfToken::generate();
        let b = csrf::CsrfToken::generate();
        assert_eq!(a.as_str().len(), 64);
        assert!(a.as_str().chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two tokens must differ");
    }

    #[test]
    fn verify_is_exact() {
        let t = csrf::CsrfToken::generate();
        assert!(csrf::verify(t.as_str(), t.as_str()));
        assert!(!csrf::verify(t.as_str(), "wrong"));
        assert!(!csrf::verify("short", t.as_str()), "different length is not equal");
    }

    #[test]
    fn the_cookie_is_httponly_and_samesite() {
        let t = csrf::CsrfToken::from_value("abc");
        let secure = t.set_cookie(true);
        assert!(secure.starts_with("kw_csrf=abc; Path=/; HttpOnly; SameSite=Lax; Secure"));
        assert!(!t.set_cookie(false).contains("Secure"));
    }

    #[test]
    fn token_is_read_from_a_cookie_header() {
        let header = "session=xyz; kw_csrf=deadbeef; theme=dark";
        assert_eq!(csrf::token_from_cookie(header), Some("deadbeef"));
        assert_eq!(csrf::token_from_cookie("other=1"), None);
    }

    #[test]
    fn form_field_is_url_decoded() {
        assert_eq!(csrf::form_field("a=1&_csrf=ab%20cd&b=2", "_csrf"), Some("ab cd".to_string()));
        assert_eq!(csrf::form_field("a=1", "_csrf"), None);
    }

    #[test]
    fn verify_request_matches_cookie_against_header_or_form() {
        // Header path.
        let mut req = Request::new("POST", "/save");
        req.headers.insert("cookie", "kw_csrf=tok123");
        req.headers.insert(csrf::HEADER, "tok123");
        assert!(csrf::verify_request(&req));

        // Form path.
        let mut req2 = Request::new("POST", "/save");
        req2.headers.insert("cookie", "kw_csrf=tok123");
        req2.body = b"name=x&_csrf=tok123".to_vec();
        assert!(csrf::verify_request(&req2));

        // Tampered — mismatch fails.
        let mut bad = Request::new("POST", "/save");
        bad.headers.insert("cookie", "kw_csrf=tok123");
        bad.headers.insert(csrf::HEADER, "forged");
        assert!(!csrf::verify_request(&bad));

        // Missing token entirely fails.
        let mut none = Request::new("POST", "/save");
        none.headers.insert("cookie", "kw_csrf=tok123");
        assert!(!csrf::verify_request(&none));
    }

    // --- authorization -----------------------------------------------------

    #[test]
    fn anonymous_has_no_identity_or_roles() {
        let ctx = SecurityContext::anonymous();
        assert!(!ctx.is_authenticated());
        assert_eq!(ctx.principal(), None);
        assert!(!ctx.has_role("ADMIN"));
    }

    #[test]
    fn authenticated_carries_principal_and_roles() {
        let ctx = SecurityContext::authenticated("alice", ["ADMIN", "USER"]);
        assert!(ctx.is_authenticated());
        assert_eq!(ctx.principal(), Some("alice"));
        assert!(ctx.has_role("ADMIN"));
        assert!(!ctx.has_role("SUPERUSER"));
        assert!(ctx.has_any_role(&["SUPERUSER", "USER"]));
        assert!(!ctx.has_any_role(&["SUPERUSER", "GUEST"]));
    }
}
