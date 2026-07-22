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
    /// Exact path → indices into `routes`, in registration order.
    ///
    /// A route with no placeholder can only ever match one path, so it does not
    /// need to be compared segment by segment — it needs to be looked up. This
    /// is what keeps a static route off the linear scan.
    static_index: HashMap<String, Vec<usize>>,
    /// Indices of the routes that do contain a placeholder, in registration
    /// order. These still have to be walked and matched.
    dynamic: Vec<usize>,
}

impl Router {
    pub fn new() -> Self {
        Self {
            routes: Vec::new(),
            static_index: HashMap::new(),
            dynamic: Vec::new(),
        }
    }

    /// Add a route.
    pub fn add(&mut self, method: &str, pattern: &str, handler: Handler) {
        let index = self.routes.len();
        if is_static(pattern) {
            self.static_index
                .entry(pattern.to_string())
                .or_default()
                .push(index);
        } else {
            self.dynamic.push(index);
        }
        self.routes.push(Route {
            method:  method.to_uppercase(),
            pattern: pattern.to_string(),
            handler,
        });
    }

    /// Find a route matching the method and path.
    /// Returns (handler, path_params) if found.
    ///
    /// # Which route wins
    /// A route whose pattern has no placeholder — `/users/me` — beats one that
    /// does — `/users/{id}` — whatever order they were registered in. Among
    /// routes of the same kind, the first registered still wins.
    ///
    /// This is what every other router does, and it is almost always what was
    /// meant: `/users/me` is written precisely because it is not to be treated
    /// as an id. It costs the ability to shadow a specific path with a general
    /// one by registering the general one first, which nobody wants on purpose.
    ///
    /// Matching and extraction are separate passes on purpose. Every route
    /// tried pays the match; only the one that wins pays for a map of its
    /// parameters. Building that map inside the loop meant every candidate
    /// allocated one, then threw it away on the next mismatch.
    pub fn find(&self, method: &str, path: &str) -> Option<(Handler, HashMap<String, String>)> {
        // One hash lookup regardless of how many routes the application has.
        if let Some(candidates) = self.static_index.get(path) {
            for &i in candidates {
                let route = &self.routes[i];
                // Compared in place: `method.to_uppercase()` allocated a String
                // per request only to compare it against an already-uppercased
                // field.
                if route.method.eq_ignore_ascii_case(method) {
                    return Some((Arc::clone(&route.handler), HashMap::new()));
                }
            }
            // The path is known but not for this method — a dynamic route may
            // still take it, so this falls through rather than answering 404.
        }

        for &i in &self.dynamic {
            let route = &self.routes[i];
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

/// Whether a pattern has no placeholders, and so matches exactly one path.
fn is_static(pattern: &str) -> bool {
    !pattern.split('/').any(|segment| placeholder(segment).is_some())
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

    /// A router of named routes, so tests can assert *which* one answered.
    fn labelled(routes: &[(&str, &'static str, &'static str)]) -> Router {
        let mut router = Router::new();
        for (method, pattern, label) in routes {
            let label = *label;
            router.add(method, pattern, Arc::new(move |_req, _ctx| {
                kernway_core::response::Response::new(kernway_core::error::StatusCode::OK)
                    .body(label.as_bytes().to_vec())
            }));
        }
        router
    }

    /// Which route answered `method path`, by its label.
    fn hit(router: &Router, method: &str, path: &str) -> Option<(String, HashMap<String, String>)> {
        let (handler, params) = router.find(method, path)?;
        let ctx = di_core::AppContext::new();
        let resp = handler(&kernway_core::request::Request::new(method, path), &ctx);
        Some((String::from_utf8(resp.body).unwrap(), params))
    }

    #[test]
    fn a_static_route_beats_a_dynamic_one_registered_first() {
        // The rule that changed: registration order no longer lets a general
        // pattern shadow a specific path.
        let router = labelled(&[
            ("GET", "/users/{id}", "dynamic"),
            ("GET", "/users/me", "static"),
        ]);
        let (label, params) = hit(&router, "GET", "/users/me").unwrap();
        assert_eq!(label, "static");
        assert!(params.is_empty(), "a static route has no path params");
    }

    #[test]
    fn the_dynamic_route_still_takes_everything_else() {
        let router = labelled(&[
            ("GET", "/users/{id}", "dynamic"),
            ("GET", "/users/me", "static"),
        ]);
        let (label, params) = hit(&router, "GET", "/users/42").unwrap();
        assert_eq!(label, "dynamic");
        assert_eq!(params["id"], "42");
    }

    #[test]
    fn among_dynamic_routes_the_first_registered_still_wins() {
        let router = labelled(&[
            ("GET", "/a/{x}", "first"),
            ("GET", "/a/{y}", "second"),
        ]);
        assert_eq!(hit(&router, "GET", "/a/1").unwrap().0, "first");
    }

    #[test]
    fn among_identical_static_routes_the_first_registered_still_wins() {
        let router = labelled(&[
            ("GET", "/dup", "first"),
            ("GET", "/dup", "second"),
        ]);
        assert_eq!(hit(&router, "GET", "/dup").unwrap().0, "first");
    }

    #[test]
    fn a_known_path_with_the_wrong_method_falls_through_to_a_dynamic_route() {
        // The static index is keyed by path alone, so a hit there that does not
        // match the method must not short-circuit into a 404.
        let router = labelled(&[
            ("GET", "/users/me", "static-get"),
            ("POST", "/users/{id}", "dynamic-post"),
        ]);
        let (label, params) = hit(&router, "POST", "/users/me").unwrap();
        assert_eq!(label, "dynamic-post");
        assert_eq!(params["id"], "me");
    }

    #[test]
    fn a_known_path_with_no_matching_method_at_all_is_a_miss() {
        let router = labelled(&[("GET", "/health", "get")]);
        assert!(router.find("DELETE", "/health").is_none());
    }

    #[test]
    fn is_static_recognises_placeholders_anywhere_in_the_pattern() {
        assert!(is_static("/users/me"));
        assert!(is_static("/"));
        assert!(!is_static("/users/{id}"));
        assert!(!is_static("/{tenant}/users"));
        assert!(!is_static("/a/{b}/c/{d}"));
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
