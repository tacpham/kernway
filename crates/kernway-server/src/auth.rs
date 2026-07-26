//! JWT bearer authentication (feature = `jwt`).
//!
//! [`BearerAuth`] reads `Authorization: Bearer <jwt>`, verifies and validates the token
//! (signature, `HS256`, `exp`/`nbf`), and puts a `SecurityContext` in the request scope
//! (KEP-0005) — exactly where the session auth and the demo header-auth put theirs, so
//! `#[require_role]` and `HttpSecurity` enforce it downstream without caring how the
//! identity arrived. A missing, malformed, or invalid token yields an **anonymous**
//! context (not a rejection): authorization decides what anonymous may reach.
//!
//! The identity mapping is the common one — `sub` → principal, the `roles` claim →
//! roles. That covers most tokens; a bespoke shape is a decode + `scope.set` in your
//! own middleware (KEP-0000: paved road, never walled in).

use di_core::RequestScope;
use kernway_core::layer::BoxFuture;
use kernway_core::request::Request;
use kernway_core::response::Response;
use kernway_security::{Jwt, SecurityContext, Validation};

use crate::middleware::{Middleware, Next};

/// Authenticates requests from a `Bearer` JWT, setting a `SecurityContext`.
pub struct BearerAuth {
    jwt: Jwt,
    validation: Validation,
}

impl BearerAuth {
    /// Authenticate with `secret` (the HS256 signing key) and the default validation
    /// (checks signature, algorithm, `exp`, and `nbf` with 60s leeway).
    #[must_use]
    pub fn new(secret: impl Into<Vec<u8>>) -> Self {
        Self { jwt: Jwt::new(secret), validation: Validation::default() }
    }

    /// Authenticate with an explicit [`Validation`] (e.g. an expected issuer/audience).
    #[must_use]
    pub fn with_validation(secret: impl Into<Vec<u8>>, validation: Validation) -> Self {
        Self { jwt: Jwt::new(secret), validation }
    }
}

impl Middleware for BearerAuth {
    fn name(&self) -> &'static str {
        "BearerAuth"
    }

    fn handle<'a>(&'a self, req: Request, scope: &'a RequestScope, next: Next<'a>) -> BoxFuture<'a, Response> {
        let context = bearer_token(&req)
            .and_then(|token| self.jwt.decode_with(token, unix_now(), &self.validation).ok())
            .map(|claims| {
                let roles = claims.role_list(); // borrow before moving sub out
                SecurityContext::authenticated(claims.sub.unwrap_or_default(), roles)
            })
            .unwrap_or_else(SecurityContext::anonymous);
        scope.set(context);
        next.run(req, scope)
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
