//! Router — a segment radix trie.
//!
//! The path is split on `/` and matched one segment at a time down a tree.
//! At each node a static child is tried before the parameter child, so
//! `/users/me` beats `/users/{id}` without depending on registration order —
//! and a static branch that dead-ends falls back to the parameter branch, so
//! `/a/{x}/c` still matches `/a/b/c` even though `/a/b/...` also exists.
//!
//! This replaces the earlier hash-map-plus-linear-scan design, which was O(n)
//! in the number of dynamic routes. The trie is O(path length): a lookup costs
//! the same whether the app has ten routes or ten thousand. Benchmarked against
//! `matchit` (axum's router) in `benches/vs_matchit.rs` — see
//! [KEP-0000 §2](../../../docs/kep/0000-principles.md): the point of writing our
//! own is to be at least as fast as the crate we declined.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};
use std::sync::Arc;

use std::future::Future;

use di_core::RequestScope;
use kernway_core::layer::BoxFuture;
use kernway_core::{request::Request, response::Response};

/// Handler function type — an **async** handler ([KEP-0006]). It receives the
/// request by value and the per-request DI scope ([KEP-0005]), and returns a boxed
/// future. Taking the request by value lets the returned future be `'static` (it
/// owns the request and whatever it pulled out of the scope), which is what keeps
/// the type free of higher-ranked-lifetime gymnastics.
///
/// [KEP-0005]: https://github.com/tacpham/kernway/blob/main/docs/kep/0005-request-scoped-beans.md
/// [KEP-0006]: https://github.com/tacpham/kernway/blob/main/docs/kep/0006-async-handlers.md
pub type Handler =
    Arc<dyn Fn(Request, &RequestScope) -> BoxFuture<'static, Response> + Send + Sync>;

/// Turn a handler closure into a boxed [`Handler`].
///
/// A handler is `Fn(Request, &RequestScope) -> impl Future<Output = Response>`: pull
/// what you need out of the scope synchronously, then `async move` a future that
/// owns it. `IntoHandler` boxes that future, so the author writes the `async` block
/// rather than the `Box::pin`.
pub trait IntoHandler<Marker>: Send + Sync + 'static {
    /// Box this into the stored [`Handler`] form.
    fn into_handler(self) -> Handler;
}

impl<F, Fut> IntoHandler<()> for F
where
    F: Fn(Request, &RequestScope) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    fn into_handler(self) -> Handler {
        Arc::new(move |req: Request, scope: &RequestScope| {
            Box::pin(self(req, scope)) as BoxFuture<'static, Response>
        })
    }
}

/// FNV-1a, for hashing path segments in the trie's static children.
///
/// Path segments are short, and a router does not face adversarial keys the way
/// a user-facing map does — a `/health` cannot be chosen to cause collisions.
/// SipHash (the `HashMap` default) is DoS-resistant and slow; FNV-1a is a few
/// instructions per byte and, measured, roughly halves a static lookup. Written
/// here rather than pulled from `fxhash`/`ahash`, per KEP-0000 §1.
#[derive(Default)]
struct FnvHasher(u64);

impl Hasher for FnvHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        // The offset basis; folded in lazily so `Default` can stay derived at 0.
        let mut h = if self.0 == 0 {
            0xcbf2_9ce4_8422_2325
        } else {
            self.0
        };
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.0 = h;
    }
}

type FnvMap<V> = HashMap<String, V, BuildHasherDefault<FnvHasher>>;

/// One node of the trie. A node both routes further (`statics`/`param`) and can
/// be an endpoint (`handlers` non-empty), since `/users` and `/users/{id}` are a
/// node and its child.
#[derive(Default)]
struct Node {
    /// Children keyed by an exact segment: `users` in `/users/{id}`.
    statics: FnvMap<Node>,
    /// The single parameter child, `{id}`. A node has at most one — two
    /// different param *names* at the same position share it; the first name
    /// registered is the one reported.
    param: Option<Box<ParamChild>>,
    /// Endpoints at this exact path, one per method, in registration order so
    /// the first registered for a method wins.
    handlers: Vec<(String, Handler)>,
}

struct ParamChild {
    /// The parameter name, e.g. `id` — from the first `{id}` registered here.
    name: String,
    node: Node,
}

impl Node {
    /// The handler for `method` at this node, first registered wins.
    fn handler_for(&self, method: &str) -> Option<&Handler> {
        self.handlers
            .iter()
            .find(|(m, _)| m.eq_ignore_ascii_case(method))
            .map(|(_, h)| h)
    }
}

/// Method + path routing table, backed by a radix trie.
pub struct Router {
    root: Node,
}

impl Router {
    /// Create an empty router.
    pub fn new() -> Self {
        Self {
            root: Node::default(),
        }
    }

    /// Add a route.
    ///
    /// Inserting the same pattern twice keeps both handlers; a lookup takes the
    /// first registered for the method, so registration order breaks ties among
    /// identical routes — the same rule the old router had.
    pub fn add(&mut self, method: &str, pattern: &str, handler: Handler) {
        let mut node = &mut self.root;
        for segment in segments(pattern) {
            node = match placeholder(segment) {
                Some(name) => {
                    // Descend into (or create) the parameter child, keeping the
                    // first name registered at this position.
                    let child = node.param.get_or_insert_with(|| {
                        Box::new(ParamChild {
                            name: name.to_string(),
                            node: Node::default(),
                        })
                    });
                    &mut child.node
                }
                None => node.statics.entry(segment.to_string()).or_default(),
            };
        }
        node.handlers.push((method.to_uppercase(), handler));
    }

    /// Find a route matching the method and path.
    /// Returns `(handler, path_params)` if found.
    ///
    /// # Which route wins
    /// A static segment beats a parameter segment at the same position, whatever
    /// order they were registered in — `/users/me` is written precisely because
    /// it is not to be treated as an id. Matching backtracks: if the static
    /// branch reaches a dead end, the parameter branch is tried, so a path that
    /// only the parameter route can satisfy still matches.
    ///
    /// Extraction is not separate work: parameter segments are captured during
    /// the walk, and the `HashMap` is built only for a route that has any — a
    /// static match allocates nothing here.
    pub fn find(&self, method: &str, path: &str) -> Option<(Handler, HashMap<String, String>)> {
        // `params` is the map returned to the caller, filled in place as the
        // walk binds parameters — no intermediate `Vec` to collect from. It
        // stays empty (and `HashMap::new` does not allocate until an insert),
        // so a static match allocates nothing here.
        let mut params: HashMap<String, String> = HashMap::new();
        let handler = walk(&self.root, path, method, &mut params)?;
        Some((Arc::clone(handler), params))
    }
}

/// Walk the trie down `path`, static-first with backtracking, collecting
/// parameter bindings into `params`.
///
/// Takes the remaining path as a `&str` and peels one segment per call, so no
/// segment vector is ever built — a static lookup touches only the trie and
/// allocates nothing. Empty segments (leading, trailing, or doubled `/`) are
/// skipped, matching the pattern side.
///
/// Invariant: on a `None` return, `params` is left exactly as it was found — so
/// a failed static branch does not leak bindings into the parameter branch tried
/// next.
fn walk<'a>(
    node: &'a Node,
    path: &str,
    method: &str,
    params: &mut HashMap<String, String>,
) -> Option<&'a Handler> {
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        // Path consumed — this node is the endpoint if it has the method.
        return node.handler_for(method);
    }
    let (seg, rest) = match path.find('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (path, ""),
    };

    // Static beats parameter: try the exact child first.
    if let Some(child) = node.statics.get(seg) {
        if let Some(h) = walk(child, rest, method, params) {
            return Some(h);
        }
    }

    // Fall back to the parameter child, binding this segment to its name.
    if let Some(pc) = &node.param {
        params.insert(pc.name.clone(), seg.to_string());
        if let Some(h) = walk(&pc.node, rest, method, params) {
            return Some(h);
        }
        params.remove(&pc.name); // backtrack — restore the invariant
    }

    None
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

/// The non-empty path segments: `/users/42` → `["users", "42"]`, `/` → `[]`.
fn segments(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|s| !s.is_empty())
}

/// The name inside a pattern segment, if it is a placeholder: `{id}` → `id`.
fn placeholder(segment: &str) -> Option<&str> {
    segment.strip_prefix('{')?.strip_suffix('}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use di_core::AppContext;

    /// A router of named routes, so tests can assert *which* one answered.
    fn labelled(routes: &[(&str, &'static str, &'static str)]) -> Router {
        let mut router = Router::new();
        for (method, pattern, label) in routes {
            let label = *label;
            router.add(
                method,
                pattern,
                Arc::new(move |_req: Request, _scope: &RequestScope| {
                    Box::pin(async move {
                        Response::new(kernway_core::error::StatusCode::OK)
                            .body(label.as_bytes().to_vec())
                    }) as BoxFuture<'static, Response>
                }),
            );
        }
        router
    }

    /// Which route answered `method path`, by its label, and the params it saw.
    fn hit(router: &Router, method: &str, path: &str) -> Option<(String, HashMap<String, String>)> {
        let (handler, params) = router.find(method, path)?;
        let app = AppContext::new();
        let scope = RequestScope::new(&app);
        let fut = handler(Request::new(method, path), &scope);
        let resp = rt_core::Executor::new().unwrap().block_on(fut).unwrap();
        Some((
            String::from_utf8(resp.body_bytes().to_vec()).unwrap(),
            params,
        ))
    }

    #[test]
    fn exact_match() {
        let r = labelled(&[("GET", "/health", "h")]);
        assert!(hit(&r, "GET", "/health").is_some());
        assert!(hit(&r, "GET", "/other").is_none());
    }

    #[test]
    fn path_param_is_captured() {
        let r = labelled(&[("GET", "/users/{id}", "u")]);
        let (_, params) = hit(&r, "GET", "/users/42").unwrap();
        assert_eq!(params["id"], "42");
    }

    #[test]
    fn multi_param_is_captured() {
        let r = labelled(&[("GET", "/users/{uid}/posts/{pid}", "up")]);
        let (_, params) = hit(&r, "GET", "/users/1/posts/99").unwrap();
        assert_eq!(params["uid"], "1");
        assert_eq!(params["pid"], "99");
    }

    #[test]
    fn a_longer_path_does_not_match_a_shorter_pattern() {
        let r = labelled(&[("GET", "/users/{id}", "u")]);
        assert!(hit(&r, "GET", "/users/1/extra").is_none());
        // And the other direction: the path running out early is a miss.
        let r = labelled(&[("GET", "/users/{id}/posts", "up")]);
        assert!(hit(&r, "GET", "/users/1").is_none());
    }

    #[test]
    fn a_static_route_yields_no_params() {
        let r = labelled(&[("GET", "/health", "h")]);
        let (_, params) = hit(&r, "GET", "/health").unwrap();
        assert!(params.is_empty());
    }

    #[test]
    fn an_unclosed_brace_is_a_literal_segment() {
        // `{id` is not a placeholder, so it only matches itself.
        let r = labelled(&[("GET", "/users/{id", "lit")]);
        assert!(hit(&r, "GET", "/users/{id").is_some());
        assert!(hit(&r, "GET", "/users/42").is_none());
    }

    #[test]
    fn a_static_route_beats_a_dynamic_one_registered_first() {
        // The core rule: registration order does not let a general pattern
        // shadow a specific path.
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
    fn a_static_dead_end_backtracks_to_the_parameter_route() {
        // /a/b/d exists (so "b" is a static child of "a"), but the request is
        // /a/b/c, which only /a/{x}/c can satisfy. The static branch into "b"
        // dead-ends at "c"; matching must fall back to the parameter branch.
        let router = labelled(&[
            ("GET", "/a/b/d", "static-bd"),
            ("GET", "/a/{x}/c", "param-c"),
        ]);
        let (label, params) = hit(&router, "GET", "/a/b/c").unwrap();
        assert_eq!(label, "param-c");
        assert_eq!(params["x"], "b");
    }

    #[test]
    fn among_dynamic_routes_the_first_registered_still_wins() {
        let router = labelled(&[("GET", "/a/{x}", "first"), ("GET", "/a/{y}", "second")]);
        assert_eq!(hit(&router, "GET", "/a/1").unwrap().0, "first");
    }

    #[test]
    fn among_identical_static_routes_the_first_registered_still_wins() {
        let router = labelled(&[("GET", "/dup", "first"), ("GET", "/dup", "second")]);
        assert_eq!(hit(&router, "GET", "/dup").unwrap().0, "first");
    }

    #[test]
    fn a_known_path_with_the_wrong_method_falls_through_to_a_dynamic_route() {
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
    fn the_root_path_routes() {
        let r = labelled(&[("GET", "/", "root")]);
        assert_eq!(hit(&r, "GET", "/").unwrap().0, "root");
    }

    #[test]
    fn method_matching_is_case_insensitive() {
        let r = labelled(&[("GET", "/health", "h")]);
        assert!(r.find("get", "/health").is_some());
    }

    #[test]
    fn find_returns_none_for_unregistered() {
        let mut router = Router::new();
        router.add(
            "GET",
            "/ping",
            Arc::new(|_req: Request, _scope: &RequestScope| {
                Box::pin(async { Response::new(kernway_core::error::StatusCode::OK) })
                    as BoxFuture<'static, Response>
            }),
        );
        assert!(router.find("GET", "/ping").is_some());
        assert!(router.find("POST", "/ping").is_none());
        assert!(router.find("GET", "/other").is_none());
    }
}
