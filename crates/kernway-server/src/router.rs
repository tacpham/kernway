//! Router — path pattern matching.

use std::collections::HashMap;
use std::sync::Arc;

use di_core::AppContext;
use kernway_core::{request::Request, response::Response};

/// Handler function type — receives a request and context, returns a response.
pub type Handler = Arc<dyn Fn(&Request, &AppContext) -> Response + Send + Sync>;

struct Route {
    method:  String,
    pattern: String,  // e.g. "/users/{id}"
    handler: Handler,
}

pub struct Router {
    routes: Vec<Route>,
}

impl Router {
    pub fn new() -> Self {
        Self { routes: Vec::new() }
    }

    /// Add a route.
    pub fn add(&mut self, method: &str, pattern: &str, handler: Handler) {
        self.routes.push(Route {
            method:  method.to_uppercase(),
            pattern: pattern.to_string(),
            handler,
        });
    }

    /// Find a route matching the method and path.
    /// Returns (handler, path_params) if found.
    ///
    /// Routes are tried in registration order, so the first one added wins.
    ///
    /// Matching and extraction are separate passes on purpose. Every route
    /// tried pays the match; only the one that wins pays for a map of its
    /// parameters. Building that map inside the loop meant every candidate
    /// allocated one, then threw it away on the next mismatch.
    pub fn find(&self, method: &str, path: &str) -> Option<(Handler, HashMap<String, String>)> {
        for route in &self.routes {
            // Compared in place: `method.to_uppercase()` allocated a String per
            // request only to compare it against an already-uppercased field.
            if !route.method.eq_ignore_ascii_case(method) { continue; }
            if !matches_pattern(&route.pattern, path) { continue; }
            return Some((Arc::clone(&route.handler), extract_params(&route.pattern, path)));
        }
        None
    }
}

impl Default for Router {
    fn default() -> Self { Self::new() }
}

/// The name inside a pattern segment, if it is a placeholder: `{id}` → `id`.
fn placeholder(segment: &str) -> Option<&str> {
    segment.strip_prefix('{')?.strip_suffix('}')
}

/// Whether `pattern` matches `path` — `/users/{id}` against `/users/42`.
///
/// Walks the two segment iterators in step rather than collecting them. The
/// old form built a `Vec` for each side, which meant two heap allocations for
/// every route the router tried and then discarded, on every request.
fn matches_pattern(pattern: &str, path: &str) -> bool {
    let mut pat = pattern.split('/');
    let mut pth = path.split('/');
    loop {
        match (pat.next(), pth.next()) {
            (None, None) => return true,
            // Unequal segment counts: one iterator runs out before the other.
            (None, Some(_)) | (Some(_), None) => return false,
            (Some(p), Some(s)) => {
                if placeholder(p).is_none() && p != s {
                    return false;
                }
            }
        }
    }
}

/// The path parameters of a pattern known to match: `/users/{id}` + `/users/42`
/// → `{ "id": "42" }`.
///
/// Only ever called on the route that won, so a pattern with no placeholders
/// costs nothing here — `HashMap::new` does not allocate until something is
/// inserted.
fn extract_params(pattern: &str, path: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for (pat, pth) in pattern.split('/').zip(path.split('/')) {
        if let Some(key) = placeholder(pat) {
            params.insert(key.to_string(), pth.to_string());
        }
    }
    params
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        assert!(matches_pattern("/health", "/health"));
        assert!(!matches_pattern("/health", "/other"));
    }

    #[test]
    fn path_param_match() {
        assert!(matches_pattern("/users/{id}", "/users/42"));
        let params = extract_params("/users/{id}", "/users/42");
        assert_eq!(params.get("id").unwrap(), "42");
    }

    #[test]
    fn multi_param_match() {
        let pattern = "/users/{uid}/posts/{pid}";
        assert!(matches_pattern(pattern, "/users/1/posts/99"));
        let params = extract_params(pattern, "/users/1/posts/99");
        assert_eq!(params["uid"], "1");
        assert_eq!(params["pid"], "99");
    }

    #[test]
    fn length_mismatch() {
        assert!(!matches_pattern("/users/{id}", "/users/1/extra"));
        // The other direction too: the path running out first must not match
        // by silently ignoring the pattern's remaining segments.
        assert!(!matches_pattern("/users/{id}/posts", "/users/1"));
    }

    #[test]
    fn a_static_route_yields_no_params() {
        let params = extract_params("/health", "/health");
        assert!(params.is_empty());
    }

    #[test]
    fn an_unclosed_brace_is_a_literal_segment() {
        // `{id` is not a placeholder, so it only matches itself.
        assert!(matches_pattern("/users/{id", "/users/{id"));
        assert!(!matches_pattern("/users/{id", "/users/42"));
    }

    #[test]
    fn first_registered_route_wins() {
        let mut router = Router::new();
        let ok = |body: &'static str| -> Handler {
            Arc::new(move |_req, _ctx| {
                kernway_core::response::Response::new(kernway_core::error::StatusCode::OK)
                    .body(body.as_bytes().to_vec())
            })
        };
        router.add("GET", "/users/{id}", ok("dynamic"));
        router.add("GET", "/users/me", ok("static"));
        let (handler, params) = router.find("GET", "/users/me").unwrap();
        let ctx = di_core::AppContext::new();
        let resp = handler(&kernway_core::request::Request::new("GET", "/users/me"), &ctx);
        assert_eq!(resp.body, b"dynamic", "registration order decides, not specificity");
        assert_eq!(params.get("id").unwrap(), "me");
    }

    #[test]
    fn router_find_registered_route() {
        let mut router = Router::new();
        router.add("GET", "/ping", Arc::new(|_req, _ctx| {
            kernway_core::response::Response::new(kernway_core::error::StatusCode::OK)
        }));
        assert!(router.find("GET", "/ping").is_some());
        assert!(router.find("POST", "/ping").is_none());
        assert!(router.find("GET", "/other").is_none());
    }

    #[test]
    fn router_find_returns_path_params() {
        let mut router = Router::new();
        router.add("GET", "/users/{id}", Arc::new(|_req, _ctx| {
            kernway_core::response::Response::new(kernway_core::error::StatusCode::OK)
        }));
        let (_, params) = router.find("GET", "/users/99").unwrap();
        assert_eq!(params.get("id").unwrap(), "99");
    }

    #[test]
    fn router_find_case_insensitive_method() {
        let mut router = Router::new();
        router.add("GET", "/health", Arc::new(|_req, _ctx| {
            kernway_core::response::Response::new(kernway_core::error::StatusCode::OK)
        }));
        assert!(router.find("get", "/health").is_some());
    }
}
