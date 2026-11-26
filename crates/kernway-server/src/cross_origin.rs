//! Cross-origin isolation — `COOP` / `COEP` / `CORP` response headers.
//!
//! A browser only hands a page `SharedArrayBuffer` (and so multi-threaded
//! WebAssembly, e.g. a chess engine running in the page) when the document is
//! **cross-origin isolated**: served with
//!
//! - `Cross-Origin-Opener-Policy: same-origin`, and
//! - `Cross-Origin-Embedder-Policy: require-corp` (or `credentialless`).
//!
//! Under `require-corp`, every subresource the page loads (wasm, js, workers,
//! fonts) must in turn opt in with `Cross-Origin-Resource-Policy`, so this
//! middleware stamps that too (`same-origin` by default).
//!
//! Headers the handler already set are left alone, and an optional path filter
//! limits the headers to part of the site (e.g. just the engine page and its
//! assets), since isolation breaks cross-origin embeds the rest of the site may use.
//!
//! ```rust,ignore
//! CrossOriginIsolation::new()
//!     .coep_credentialless()
//!     .corp(Corp::CrossOrigin)
//!     .paths(["/play/**", "/engine/**"])
//! ```

use di_core::RequestScope;
use kernway_core::layer::BoxFuture;
use kernway_core::request::Request;
use kernway_core::response::Response;

use crate::middleware::{Middleware, Next};

const COOP: &str = "cross-origin-opener-policy";
const COEP: &str = "cross-origin-embedder-policy";
const CORP: &str = "cross-origin-resource-policy";

/// A `Cross-Origin-Resource-Policy` value — who may load this resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corp {
    /// Only same-origin documents (`same-origin`).
    SameOrigin,
    /// Same-site documents (`same-site`).
    SameSite,
    /// Any origin (`cross-origin`) — for assets served to other sites too.
    CrossOrigin,
}

impl Corp {
    fn as_str(self) -> &'static str {
        match self {
            Corp::SameOrigin => "same-origin",
            Corp::SameSite => "same-site",
            Corp::CrossOrigin => "cross-origin",
        }
    }
}

/// Middleware that makes responses cross-origin isolated (COOP + COEP, plus CORP
/// for subresources). Never overwrites a header the handler already set.
#[derive(Debug, Clone)]
pub struct CrossOriginIsolation {
    coep: &'static str,
    corp: Option<Corp>,
    /// Ant-style patterns (`/a/**`, `/a/*`, exact); empty = every path.
    paths: Vec<String>,
}

impl Default for CrossOriginIsolation {
    fn default() -> Self {
        Self {
            coep: "require-corp",
            corp: Some(Corp::SameOrigin),
            paths: Vec::new(),
        }
    }
}

impl CrossOriginIsolation {
    /// `COOP: same-origin`, `COEP: require-corp`, `CORP: same-origin`, on every path.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Use `COEP: credentialless` instead of `require-corp`: cross-origin
    /// subresources load without credentials rather than needing a CORP opt-in.
    #[must_use]
    pub fn coep_credentialless(mut self) -> Self {
        self.coep = "credentialless";
        self
    }

    /// The `Cross-Origin-Resource-Policy` value to stamp (default `same-origin`).
    #[must_use]
    pub fn corp(mut self, corp: Corp) -> Self {
        self.corp = Some(corp);
        self
    }

    /// Do not add a `Cross-Origin-Resource-Policy` header at all.
    #[must_use]
    pub fn without_corp(mut self) -> Self {
        self.corp = None;
        self
    }

    /// Only apply to requests whose path matches one of these Ant-style patterns
    /// (`/a/**` = `/a` and below, `/a/*` = one segment below, otherwise exact).
    /// Repeated calls add patterns. With no patterns, every path is covered.
    #[must_use]
    pub fn paths<I, S>(mut self, patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.paths.extend(patterns.into_iter().map(Into::into));
        self
    }

    fn applies_to(&self, path: &str) -> bool {
        self.paths.is_empty() || self.paths.iter().any(|p| path_matches(p, path))
    }

    /// Add the headers to `response`, skipping any already present.
    pub fn apply(&self, response: &mut Response) {
        let headers = &mut response.headers;
        if !headers.contains_key(COOP) {
            headers.insert(COOP, "same-origin");
        }
        if !headers.contains_key(COEP) {
            headers.insert(COEP, self.coep);
        }
        if let Some(corp) = self.corp {
            if !headers.contains_key(CORP) {
                headers.insert(CORP, corp.as_str());
            }
        }
    }
}

impl Middleware for CrossOriginIsolation {
    fn name(&self) -> &'static str {
        "CrossOriginIsolation"
    }

    fn handle<'a>(
        &'a self,
        req: Request,
        scope: &'a RequestScope,
        next: Next<'a>,
    ) -> BoxFuture<'a, Response> {
        let applies = self.applies_to(&req.path);
        Box::pin(async move {
            let mut response = next.run(req, scope).await;
            if applies {
                self.apply(&mut response);
            }
            response
        })
    }
}

/// Ant-style path match (same rules as `HttpSecurity`): `**` / `/**` matches all,
/// `/a/**` matches `/a` and any descendant, `/a/*` exactly one more segment,
/// otherwise an exact match.
fn path_matches(pattern: &str, path: &str) -> bool {
    if pattern == "**" || pattern == "/**" {
        return true;
    }
    let below =
        |prefix: &str| path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/');
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return path == prefix || below(prefix);
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        if below(prefix) {
            let rest = &path[prefix.len() + 1..];
            return !rest.is_empty() && !rest.contains('/');
        }
        return false;
    }
    pattern == path
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::Terminal;
    use di_core::AppContext;
    use kernway_core::error::StatusCode;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    fn block<T>(fut: impl Future<Output = T>) -> T {
        let mut fut = Box::pin(fut);
        let waker = Waker::noop();
        match fut.as_mut().poll(&mut Context::from_waker(waker)) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("cross-origin isolation resolves synchronously"),
        }
    }

    /// Route a GET for `path` through `mw` to a handler that sets `preset` headers.
    fn run(mw: &CrossOriginIsolation, path: &str, preset: &'static [(&str, &str)]) -> Response {
        let req = Request::new("GET", path);
        let app = AppContext::new();
        let scope = RequestScope::new(&app);
        let terminal: &Terminal = &move |_req, _scope| {
            Box::pin(async move {
                let mut resp = Response::new(StatusCode::OK).body(b"ok".to_vec());
                for (k, v) in preset {
                    resp.headers.insert(k, v);
                }
                resp
            }) as BoxFuture<'static, Response>
        };
        block(mw.handle(
            req,
            &scope,
            Next {
                rest: &[],
                terminal,
            },
        ))
    }

    fn count(resp: &Response, name: &str) -> usize {
        resp.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .count()
    }

    #[test]
    fn adds_isolation_headers_to_a_normal_response() {
        let resp = run(&CrossOriginIsolation::new(), "/", &[]);
        assert_eq!(resp.headers.get(COOP), Some("same-origin"));
        assert_eq!(resp.headers.get(COEP), Some("require-corp"));
        assert_eq!(resp.headers.get(CORP), Some("same-origin"));
        assert_eq!(resp.status, StatusCode::OK);
    }

    #[test]
    fn builder_options_change_the_values() {
        let mw = CrossOriginIsolation::new()
            .coep_credentialless()
            .corp(Corp::CrossOrigin);
        let resp = run(&mw, "/engine.wasm", &[]);
        assert_eq!(resp.headers.get(COEP), Some("credentialless"));
        assert_eq!(resp.headers.get(CORP), Some("cross-origin"));

        let resp = run(&CrossOriginIsolation::new().without_corp(), "/", &[]);
        assert_eq!(resp.headers.get(CORP), None);
        assert_eq!(resp.headers.get(COOP), Some("same-origin"));
    }

    #[test]
    fn handler_set_headers_are_neither_overridden_nor_duplicated() {
        let resp = run(
            &CrossOriginIsolation::new(),
            "/",
            &[
                ("Cross-Origin-Opener-Policy", "same-origin-allow-popups"),
                ("cross-origin-resource-policy", "cross-origin"),
            ],
        );
        assert_eq!(resp.headers.get(COOP), Some("same-origin-allow-popups"));
        assert_eq!(resp.headers.get(CORP), Some("cross-origin"));
        // The one the handler did not set is still added.
        assert_eq!(resp.headers.get(COEP), Some("require-corp"));
        for name in [COOP, COEP, CORP] {
            assert_eq!(count(&resp, name), 1, "{name} appears once");
        }
    }

    #[test]
    fn path_filter_limits_where_headers_are_added() {
        let mw = CrossOriginIsolation::new().paths(["/play/**", "/assets/*"]);
        for covered in ["/play", "/play/room/1", "/assets/sf.wasm"] {
            let resp = run(&mw, covered, &[]);
            assert_eq!(resp.headers.get(COOP), Some("same-origin"), "{covered}");
        }
        for skipped in ["/", "/player", "/assets/deep/x.js", "/api/play"] {
            let resp = run(&mw, skipped, &[]);
            assert_eq!(resp.headers.get(COOP), None, "{skipped}");
            assert_eq!(resp.headers.get(COEP), None, "{skipped}");
            assert_eq!(resp.headers.get(CORP), None, "{skipped}");
        }
    }
}
