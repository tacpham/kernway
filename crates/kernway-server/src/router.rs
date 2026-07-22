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
    pub fn find(&self, method: &str, path: &str) -> Option<(Handler, HashMap<String, String>)> {
        let method = method.to_uppercase();
        for route in &self.routes {
            if route.method != method { continue; }
            if let Some(params) = match_pattern(&route.pattern, path) {
                return Some((Arc::clone(&route.handler), params));
            }
        }
        None
    }
}

impl Default for Router {
    fn default() -> Self { Self::new() }
}

/// Pattern matching: "/users/{id}" matches "/users/42" → { "id": "42" }
fn match_pattern(pattern: &str, path: &str) -> Option<HashMap<String, String>> {
    let pat_segs: Vec<&str> = pattern.split('/').collect();
    let pth_segs: Vec<&str> = path.split('/').collect();

    if pat_segs.len() != pth_segs.len() {
        return None;
    }

    let mut params = HashMap::new();
    for (pat, pth) in pat_segs.iter().zip(pth_segs.iter()) {
        if pat.starts_with('{') && pat.ends_with('}') {
            // Path param
            let key = &pat[1..pat.len() - 1];
            params.insert(key.to_string(), pth.to_string());
        } else if pat != pth {
            return None;
        }
    }
    Some(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match() {
        assert!(match_pattern("/health", "/health").is_some());
        assert!(match_pattern("/health", "/other").is_none());
    }

    #[test]
    fn path_param_match() {
        let params = match_pattern("/users/{id}", "/users/42").unwrap();
        assert_eq!(params.get("id").unwrap(), "42");
    }

    #[test]
    fn multi_param_match() {
        let params = match_pattern("/users/{uid}/posts/{pid}", "/users/1/posts/99").unwrap();
        assert_eq!(params["uid"], "1");
        assert_eq!(params["pid"], "99");
    }

    #[test]
    fn length_mismatch() {
        assert!(match_pattern("/users/{id}", "/users/1/extra").is_none());
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
