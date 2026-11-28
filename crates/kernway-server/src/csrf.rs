//! Cross-site request forgery protection as a middleware: every state-changing
//! request must carry the token from the `kw_csrf` cookie back in the
//! `X-CSRF-Token` header or the `_csrf` form field (the double-submit pattern of
//! [`kernway_security::csrf`]), so no route can forget the check.
//!
//! ```rust,ignore
//! app.layer(CsrfProtection::new())            // all POST / PUT / PATCH / DELETE checked
//!    .layer(CsrfProtection::new().exempt("/webhooks/")) // a path a third party posts to
//! ```
//!
//! A request without the cookie gets one on its response, and the token is put in
//! the request scope as a [`CsrfToken`](kernway_security::csrf::CsrfToken) for the page to embed (kernleaf injects it
//! into every state-changing `<form>`). Requests carrying an `Authorization`
//! header are not checked: a browser never adds one to a cross-site request on its
//! own, so they are API calls, not forgeries.

use di_core::RequestScope;
use kernway_core::error::StatusCode;
use kernway_core::layer::BoxFuture;
use kernway_core::request::Request;
use kernway_core::response::Response;
use kernway_security::csrf::{self, CsrfToken};

use crate::middleware::{Middleware, Next};

/// The CSRF middleware. See the [module docs](self).
#[derive(Debug, Clone, Default)]
pub struct CsrfProtection {
    exempt: Vec<String>,
    secure_cookie: bool,
}

impl CsrfProtection {
    /// Check every POST, PUT, PATCH and DELETE.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Do not check paths starting with `prefix` (webhooks a third party posts to).
    #[must_use]
    pub fn exempt(mut self, prefix: impl Into<String>) -> Self {
        self.exempt.push(prefix.into());
        self
    }

    /// Mark the token cookie `Secure` (the site is served over HTTPS).
    #[must_use]
    pub fn secure_cookie(mut self, secure: bool) -> Self {
        self.secure_cookie = secure;
        self
    }

    fn checks(&self, req: &Request) -> bool {
        let safe = matches!(req.method.to_ascii_uppercase().as_str(), "GET" | "HEAD" | "OPTIONS" | "TRACE");
        !safe && req.header("authorization").is_none() && !self.exempt.iter().any(|p| req.path.starts_with(p.as_str()))
    }
}

impl Middleware for CsrfProtection {
    fn name(&self) -> &'static str {
        "CsrfProtection"
    }

    fn handle<'a>(&'a self, req: Request, scope: &'a RequestScope, next: Next<'a>) -> BoxFuture<'a, Response> {
        if self.checks(&req) && !csrf::verify_request(&req) {
            return Box::pin(async {
                Response::new(StatusCode::FORBIDDEN)
                    .content_type("text/plain")
                    .body(b"CSRF token missing or does not match".to_vec())
            });
        }
        let existing = req.header("cookie").and_then(csrf::token_from_cookie).filter(|t| t.len() == 64).map(str::to_owned);
        let (token, fresh) = match existing {
            Some(t) => (CsrfToken::from_value(t), false),
            None => (CsrfToken::generate(), true),
        };
        let cookie = fresh.then(|| token.set_cookie(self.secure_cookie));
        scope.set(token);
        Box::pin(async move {
            let mut response = next.run(req, scope).await;
            if let Some(c) = cookie {
                response.headers.append("set-cookie", &c);
            }
            response
        })
    }
}
