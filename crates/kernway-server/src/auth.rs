//! JWT authentication (feature = `jwt`).
//!
//! [`JwtAuth`] reads a JWT from the request — an `Authorization: Bearer <jwt>` header
//! ([`JwtAuth::bearer`]) or a named cookie ([`JwtAuth::cookie`], for a browser app that
//! keeps the token in an `HttpOnly` cookie) — verifies and validates it (signature,
//! `HS256`, `exp`/`nbf`), and puts a `SecurityContext` in the request scope (KEP-0005),
//! so `#[require_role]` and `HttpSecurity` enforce it downstream without caring how the
//! identity arrived. A missing, malformed, or invalid token yields an **anonymous**
//! context (not a rejection): authorization decides what anonymous may reach.
//!
//! The identity mapping covers **both** axes `HttpSecurity` checks — the `roles` claim →
//! roles (`has_role`) and the `authorities` claim → authorities (`has_authority`, a
//! non-role grant: a subscription tier, a scope, a feature flag). `sub` → principal.
//! The full decoded [`Claims`] is also placed in the scope, so a handler reads any extra
//! claim it minted (a tenant id, a user id) via `scope.get::<Claims>()` without decoding
//! the token a second time.
//!
//! [`BearerAuth`] is the header preset, kept for back-compat (`BearerAuth::new(secret)`
//! == `JwtAuth::bearer(secret)`).

use di_core::RequestScope;
use kernway_core::layer::BoxFuture;
use kernway_core::request::Request;
use kernway_core::response::Response;
use kernway_security::{Jwt, SecurityContext, Validation};

use crate::middleware::{Middleware, Next};

/// Where [`JwtAuth`] looks for the token.
pub enum TokenSource {
    /// An `Authorization: Bearer <token>` header (scheme case-insensitive).
    BearerHeader,
    /// A named cookie (`Cookie: <name>=<token>`) — a browser app's `HttpOnly` session.
    Cookie(String),
}

/// Authenticates requests from a JWT (header or cookie), setting a `SecurityContext`
/// (roles **and** authorities) plus the decoded [`Claims`] in the request scope.
pub struct JwtAuth {
    jwt: Jwt,
    validation: Validation,
    source: TokenSource,
}

impl JwtAuth {
    /// Read the token from an `Authorization: Bearer` header, default validation
    /// (signature, algorithm, `exp`, and `nbf` with 60s leeway).
    #[must_use]
    pub fn bearer(secret: impl Into<Vec<u8>>) -> Self {
        Self {
            jwt: Jwt::new(secret),
            validation: Validation::default(),
            source: TokenSource::BearerHeader,
        }
    }

    /// Read the token from the cookie named `cookie`, default validation. For a browser
    /// app that stores its JWT in an `HttpOnly` cookie rather than sending a header.
    #[must_use]
    pub fn cookie(cookie: impl Into<String>, secret: impl Into<Vec<u8>>) -> Self {
        Self {
            jwt: Jwt::new(secret),
            validation: Validation::default(),
            source: TokenSource::Cookie(cookie.into()),
        }
    }

    /// Replace the validation (e.g. an expected issuer/audience). Chainable.
    #[must_use]
    pub fn with_validation(mut self, validation: Validation) -> Self {
        self.validation = validation;
        self
    }

    /// The raw token for this request per the configured source.
    fn token(&self, req: &Request) -> Option<String> {
        match &self.source {
            TokenSource::BearerHeader => bearer_token(req).map(str::to_owned),
            TokenSource::Cookie(name) => req.cookie(name),
        }
    }
}

impl Middleware for JwtAuth {
    fn name(&self) -> &'static str {
        "JwtAuth"
    }

    fn handle<'a>(
        &'a self,
        req: Request,
        scope: &'a RequestScope,
        next: Next<'a>,
    ) -> BoxFuture<'a, Response> {
        let claims = self
            .token(&req)
            .and_then(|token| self.jwt.decode_with(&token, unix_now(), &self.validation).ok());
        let context = match &claims {
            // Both axes travel into the context: roles → has_role, authorities → has_authority.
            Some(c) => SecurityContext::authenticated(c.sub.clone().unwrap_or_default(), c.role_list())
                .with_authorities(c.authority_list()),
            None => SecurityContext::anonymous(),
        };
        scope.set(context);
        // The full Claims too, so a handler reads its own custom claims (tenant id, user
        // id) from the scope instead of decoding the token again.
        if let Some(c) = claims {
            scope.set(c);
        }
        next.run(req, scope)
    }
}

/// Authenticates requests from a `Bearer` JWT — the header preset of [`JwtAuth`], kept
/// for back-compat. Prefer `JwtAuth::bearer`/`JwtAuth::cookie` in new code.
pub struct BearerAuth(JwtAuth);

impl BearerAuth {
    /// Authenticate with `secret` and default validation. Same as `JwtAuth::bearer`.
    #[must_use]
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self(JwtAuth::bearer(secret))
    }

    /// Authenticate with an explicit [`Validation`] (e.g. an expected issuer/audience).
    #[must_use]
    pub fn with_validation(secret: impl Into<Vec<u8>>, validation: Validation) -> Self {
        Self(JwtAuth::bearer(secret).with_validation(validation))
    }
}

impl Middleware for BearerAuth {
    fn name(&self) -> &'static str {
        "BearerAuth"
    }

    fn handle<'a>(
        &'a self,
        req: Request,
        scope: &'a RequestScope,
        next: Next<'a>,
    ) -> BoxFuture<'a, Response> {
        self.0.handle(req, scope, next)
    }
}

/// The token from an `Authorization: Bearer <token>` header (scheme case-insensitive).
fn bearer_token(req: &Request) -> Option<&str> {
    let header = req.header("authorization")?;
    let (scheme, token) = header.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

/// Unix seconds now (runtime code, so the system clock is available).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::Terminal;
    use di_core::{AppContext, RequestScope};
    use kernway_core::error::StatusCode;
    use kernway_security::Claims;

    const SECRET: &[u8] = b"a-test-signing-secret";
    const FAR_FUTURE: u64 = 4_000_000_000; // ~ year 2096, so `exp` validation passes

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        rt_core::Executor::new().unwrap().block_on(f).unwrap()
    }

    /// Run `mw` for `req` through the chain, with a terminal that reflects the resulting
    /// `SecurityContext` (and any stashed `Claims`) back as response headers so a test
    /// can assert what the auth middleware produced.
    fn run(mw: &impl Middleware, req: Request) -> Response {
        let app = AppContext::new();
        let scope = RequestScope::new(&app);
        let terminal: &Terminal = &|_req, scope| {
            let ctx = scope.get::<SecurityContext>().expect("auth sets a context");
            let mut resp = Response::new(StatusCode::OK);
            resp.headers.insert("x-authed", if ctx.is_authenticated() { "1" } else { "0" });
            resp.headers.insert("x-admin", if ctx.has_role("ADMIN") { "1" } else { "0" });
            resp.headers.insert("x-pro", if ctx.has_authority("PRO") { "1" } else { "0" });
            if let Ok(claims) = scope.get::<Claims>() {
                if let Some(t) = claims.custom.get("tenant_id").and_then(|v| v.as_str()) {
                    resp.headers.insert("x-tenant", t);
                }
            }
            Box::pin(async move { resp }) as BoxFuture<'static, Response>
        };
        let next = Next { rest: &[], terminal };
        block_on(mw.handle(req, &scope, next))
    }

    fn token(claims: Claims) -> String {
        Jwt::new(SECRET.to_vec()).encode(&claims).unwrap()
    }

    #[test]
    fn cookie_source_maps_roles_authorities_and_stashes_claims() {
        let jwt = token(
            Claims::new()
                .subject("alice")
                .expires_at(FAR_FUTURE)
                .roles(["ADMIN"])
                .authorities(["PRO"])
                .claim("tenant_id", "t1"),
        );
        let mut req = Request::new("GET", "/x");
        req.headers.insert("cookie", &format!("jwt-token={jwt}"));
        let resp = run(&JwtAuth::cookie("jwt-token", SECRET.to_vec()), req);
        // Both axes crossed from the token into the context.
        assert_eq!(resp.headers.get("x-authed"), Some("1"));
        assert_eq!(resp.headers.get("x-admin"), Some("1"), "roles → has_role");
        assert_eq!(resp.headers.get("x-pro"), Some("1"), "authorities → has_authority");
        // And a custom claim is readable from the scope without re-decoding.
        assert_eq!(resp.headers.get("x-tenant"), Some("t1"));
    }

    #[test]
    fn bearer_preset_still_authenticates_from_the_header() {
        let jwt = token(Claims::new().subject("bob").expires_at(FAR_FUTURE).roles(["USER"]));
        let mut req = Request::new("GET", "/x");
        req.headers.insert("authorization", &format!("Bearer {jwt}"));
        let resp = run(&BearerAuth::new(SECRET.to_vec()), req);
        assert_eq!(resp.headers.get("x-authed"), Some("1"));
        assert_eq!(resp.headers.get("x-admin"), Some("0"), "USER is not ADMIN");
        assert_eq!(resp.headers.get("x-pro"), Some("0"), "no authorities claim");
    }

    #[test]
    fn a_missing_cookie_is_anonymous() {
        let resp = run(&JwtAuth::cookie("jwt-token", SECRET.to_vec()), Request::new("GET", "/x"));
        assert_eq!(resp.headers.get("x-authed"), Some("0"));
    }

    #[test]
    fn a_token_signed_with_another_key_is_rejected_as_anonymous() {
        let forged = Jwt::new(b"a-different-secret".to_vec())
            .encode(&Claims::new().subject("mallory").expires_at(FAR_FUTURE).roles(["ADMIN"]))
            .unwrap();
        let mut req = Request::new("GET", "/x");
        req.headers.insert("cookie", &format!("jwt-token={forged}"));
        let resp = run(&JwtAuth::cookie("jwt-token", SECRET.to_vec()), req);
        assert_eq!(resp.headers.get("x-authed"), Some("0"), "bad signature → anonymous");
        assert_eq!(resp.headers.get("x-admin"), Some("0"));
    }
}
