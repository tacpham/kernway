//! CORS — Cross-Origin Resource Sharing (Spring's `cors()`).
//!
//! A browser blocks a page on `https://app.example` from reading a response from
//! `https://api.other` unless the response *opts in* with `Access-Control-Allow-*`
//! headers. [`Cors`] is the middleware that sets them: it answers the **preflight**
//! `OPTIONS` request the browser sends before a non-simple request, and stamps the
//! allow headers on the actual response.
//!
//! Secure by default: nothing is allowed until you configure it. An origin not on the
//! allow-list simply gets no CORS headers (the browser then blocks the read), and
//! `*` is never combined with credentials (the spec forbids it — with credentials the
//! matched origin is echoed instead).
//!
//! ```rust,ignore
//! Cors::new()
//!     .allow_origin("https://app.example")
//!     .allow_methods(["GET", "POST"])
//!     .allow_headers(["content-type", "authorization"])
//!     .allow_credentials(true)
//!     .max_age(3600)
//! ```

use di_core::RequestScope;
use kernway_core::error::StatusCode;
use kernway_core::layer::BoxFuture;
use kernway_core::request::Request;
use kernway_core::response::Response;

use crate::middleware::{Middleware, Next};

/// Which origins are allowed.
#[derive(Clone)]
enum Origins {
    /// Any origin (`*`, or the echoed origin when credentials are on).
    Any,
    /// An explicit allow-list (exact match).
    List(Vec<String>),
}

/// Which request headers a preflight allows.
#[derive(Clone)]
enum AllowHeaders {
    /// Echo whatever the preflight asks for (effectively "any").
    MirrorRequest,
    /// An explicit list.
    List(Vec<String>),
}

/// CORS middleware. Build it with the `allow_*` methods; it answers preflight
/// `OPTIONS` requests and adds the allow headers to every cross-origin response.
pub struct Cors {
    origins: Origins,
    methods: Vec<String>,
    headers: AllowHeaders,
    exposed: Vec<String>,
    credentials: bool,
    max_age: Option<u64>,
}

impl Cors {
    /// A closed CORS policy — no origin allowed until you add one. `GET`/`HEAD`/`POST`
    /// are allowed by default (the "simple" methods); add more with
    /// [`allow_methods`](Self::allow_methods).
    #[must_use]
    pub fn new() -> Self {
        Self {
            origins: Origins::List(Vec::new()),
            methods: vec!["GET".into(), "HEAD".into(), "POST".into()],
            headers: AllowHeaders::List(Vec::new()),
            exposed: Vec::new(),
            credentials: false,
            max_age: None,
        }
    }

    /// A permissive policy for local development: any origin, any header, the common
    /// methods. **Not** for production — pair a wildcard origin with real allow-lists
    /// before shipping.
    #[must_use]
    pub fn permissive() -> Self {
        Self {
            origins: Origins::Any,
            methods: ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]
                .iter()
                .map(|s| (*s).into())
                .collect(),
            headers: AllowHeaders::MirrorRequest,
            exposed: Vec::new(),
            credentials: false,
            max_age: None,
        }
    }

    /// Allow one origin (exact, e.g. `https://app.example`). Call repeatedly to add
    /// several.
    #[must_use]
    pub fn allow_origin(mut self, origin: impl Into<String>) -> Self {
        match &mut self.origins {
            Origins::List(list) => list.push(origin.into()),
            Origins::Any => {} // already any
        }
        self
    }

    /// Allow **any** origin. Reflected per-request when credentials are on (since `*`
    /// is illegal with credentials).
    #[must_use]
    pub fn allow_any_origin(mut self) -> Self {
        self.origins = Origins::Any;
        self
    }

    /// Set the allowed methods (replaces the default `GET`/`HEAD`/`POST`).
    #[must_use]
    pub fn allow_methods<I, S>(mut self, methods: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.methods = methods.into_iter().map(Into::into).collect();
        self
    }

    /// Set the allowed request headers.
    #[must_use]
    pub fn allow_headers<I, S>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.headers = AllowHeaders::List(headers.into_iter().map(Into::into).collect());
        self
    }

    /// Mirror whatever headers the preflight asks for (effectively allow any header).
    #[must_use]
    pub fn allow_any_header(mut self) -> Self {
        self.headers = AllowHeaders::MirrorRequest;
        self
    }

    /// Response headers the browser may expose to script (beyond the safe-listed ones).
    #[must_use]
    pub fn expose_headers<I, S>(mut self, headers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.exposed = headers.into_iter().map(Into::into).collect();
        self
    }

    /// Allow credentials (cookies, `Authorization`) on cross-origin requests. Forces
    /// the matched origin to be echoed rather than `*`.
    #[must_use]
    pub fn allow_credentials(mut self, yes: bool) -> Self {
        self.credentials = yes;
        self
    }

    /// How long (seconds) the browser may cache a preflight result.
    #[must_use]
    pub fn max_age(mut self, secs: u64) -> Self {
        self.max_age = Some(secs);
        self
    }

    /// The `Access-Control-Allow-Origin` value for `origin`, or `None` if not allowed.
    fn allow_origin_value(&self, origin: &str) -> Option<String> {
        match &self.origins {
            // With credentials `*` is illegal, so echo the specific origin.
            Origins::Any if self.credentials => Some(origin.to_string()),
            Origins::Any => Some("*".to_string()),
            Origins::List(list) => list.iter().any(|o| o == origin).then(|| origin.to_string()),
        }
    }

    /// Stamp the response headers common to preflight and actual responses.
    fn stamp_common(&self, resp: &mut Response, allow_origin: &str) {
        resp.headers
            .insert("access-control-allow-origin", allow_origin);
        if self.credentials {
            resp.headers
                .insert("access-control-allow-credentials", "true");
        }
        // The response varies by Origin, so caches must key on it.
        resp.headers.insert("vary", "Origin");
    }

    /// Build the `204` preflight response.
    fn preflight(&self, req: &Request, allow_origin: &str) -> Response {
        let mut resp = Response::new(StatusCode::NO_CONTENT);
        self.stamp_common(&mut resp, allow_origin);
        resp.headers
            .insert("access-control-allow-methods", &self.methods.join(", "));

        let allow_headers = match &self.headers {
            AllowHeaders::List(list) => list.join(", "),
            // Echo the requested headers (what the browser asked to send).
            AllowHeaders::MirrorRequest => req
                .header("access-control-request-headers")
                .unwrap_or("")
                .to_string(),
        };
        if !allow_headers.is_empty() {
            resp.headers
                .insert("access-control-allow-headers", &allow_headers);
        }
        if let Some(secs) = self.max_age {
            resp.headers
                .insert("access-control-max-age", &secs.to_string());
        }
        resp
    }
}

impl Default for Cors {
    fn default() -> Self {
        Self::new()
    }
}

impl Middleware for Cors {
    fn name(&self) -> &'static str {
        "Cors"
    }

    fn handle<'a>(
        &'a self,
        req: Request,
        scope: &'a RequestScope,
        next: Next<'a>,
    ) -> BoxFuture<'a, Response> {
        // Resolve the origin decision synchronously (borrows req).
        let allow_origin = req
            .header("origin")
            .and_then(|o| self.allow_origin_value(o));
        let is_preflight = req.method.eq_ignore_ascii_case("OPTIONS")
            && req.header("access-control-request-method").is_some();

        // A preflight (even from a disallowed origin) is answered here, never routed.
        if is_preflight {
            let response = match &allow_origin {
                Some(origin) => self.preflight(&req, origin),
                // Not allowed: a bare 204 with no allow headers — the browser blocks.
                None => Response::new(StatusCode::NO_CONTENT),
            };
            return Box::pin(async move { response });
        }

        // Actual request: run it, then add the allow headers if the origin is allowed.
        let exposed = (!self.exposed.is_empty()).then(|| self.exposed.join(", "));
        Box::pin(async move {
            let mut response = next.run(req, scope).await;
            if let Some(origin) = allow_origin {
                self.stamp_common(&mut response, &origin);
                if let Some(exposed) = exposed {
                    response
                        .headers
                        .insert("access-control-expose-headers", &exposed);
                }
            }
            response
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::Terminal;
    use di_core::AppContext;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    fn block<T>(fut: impl Future<Output = T>) -> T {
        let mut fut = Box::pin(fut);
        let waker = Waker::noop();
        match fut.as_mut().poll(&mut Context::from_waker(waker)) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("cors resolves synchronously"),
        }
    }

    /// A request with the given method and headers, routed through `cors` to a
    /// trivial handler, returning the response.
    fn run(cors: &Cors, method: &str, headers: &[(&str, &str)]) -> Response {
        let mut req = Request::new(method, "/api");
        for (k, v) in headers {
            req.headers.insert(k, v);
        }
        let app = AppContext::new();
        let scope = RequestScope::new(&app);
        let terminal: &Terminal = &|_req, _scope| {
            Box::pin(async { Response::new(StatusCode::OK).body(b"ok".to_vec()) })
                as BoxFuture<'static, Response>
        };
        block(cors.handle(
            req,
            &scope,
            Next {
                rest: &[],
                terminal,
            },
        ))
    }

    #[test]
    fn an_allowed_origin_gets_the_allow_header() {
        let cors = Cors::new().allow_origin("https://app.example");
        let resp = run(&cors, "GET", &[("origin", "https://app.example")]);
        assert_eq!(
            resp.headers.get("access-control-allow-origin"),
            Some("https://app.example")
        );
        assert_eq!(resp.headers.get("vary"), Some("Origin"));
    }

    #[test]
    fn a_disallowed_origin_gets_no_cors_headers() {
        let cors = Cors::new().allow_origin("https://app.example");
        let resp = run(&cors, "GET", &[("origin", "https://evil.example")]);
        assert_eq!(
            resp.headers.get("access-control-allow-origin"),
            None,
            "not allow-listed"
        );
        // The request still ran (CORS is a browser policy, not server enforcement).
        assert_eq!(resp.status, StatusCode::OK);
    }

    #[test]
    fn a_preflight_is_answered_204_with_methods_and_headers() {
        let cors = Cors::new()
            .allow_origin("https://app.example")
            .allow_methods(["GET", "POST", "DELETE"])
            .allow_headers(["content-type", "authorization"])
            .max_age(600);
        let resp = run(
            &cors,
            "OPTIONS",
            &[
                ("origin", "https://app.example"),
                ("access-control-request-method", "DELETE"),
            ],
        );
        assert_eq!(resp.status, StatusCode::NO_CONTENT);
        assert_eq!(
            resp.headers.get("access-control-allow-origin"),
            Some("https://app.example")
        );
        assert_eq!(
            resp.headers.get("access-control-allow-methods"),
            Some("GET, POST, DELETE")
        );
        assert_eq!(
            resp.headers.get("access-control-allow-headers"),
            Some("content-type, authorization")
        );
        assert_eq!(resp.headers.get("access-control-max-age"), Some("600"));
    }

    #[test]
    fn credentials_echo_the_origin_never_wildcard() {
        let cors = Cors::new().allow_any_origin().allow_credentials(true);
        let resp = run(&cors, "GET", &[("origin", "https://app.example")]);
        assert_eq!(
            resp.headers.get("access-control-allow-origin"),
            Some("https://app.example"),
            "with credentials the origin is echoed, not *"
        );
        assert_eq!(
            resp.headers.get("access-control-allow-credentials"),
            Some("true")
        );
    }

    #[test]
    fn any_origin_without_credentials_is_wildcard() {
        let cors = Cors::permissive();
        let resp = run(&cors, "GET", &[("origin", "https://anywhere.example")]);
        assert_eq!(resp.headers.get("access-control-allow-origin"), Some("*"));
    }

    #[test]
    fn a_preflight_mirrors_requested_headers_when_permissive() {
        let cors = Cors::permissive();
        let resp = run(
            &cors,
            "OPTIONS",
            &[
                ("origin", "https://x.example"),
                ("access-control-request-method", "PUT"),
                ("access-control-request-headers", "x-custom, content-type"),
            ],
        );
        assert_eq!(
            resp.headers.get("access-control-allow-headers"),
            Some("x-custom, content-type")
        );
    }
}
