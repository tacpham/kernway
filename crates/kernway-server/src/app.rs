//! KernwayApp — app builder + HTTP server.
//!
//! The transport is [`rt_net`]: one shard per core, each with its own
//! `SO_REUSEPORT` listener and executor, one task per connection. Handlers and
//! middleware are still synchronous — they run to completion inside the
//! connection's task — so porting the transport did not change a single
//! handler signature. Making them `async` is the next step, and belongs with
//! the `kernway-core` spec work.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use di_core::AppContext;
use kernway_core::{error::StatusCode, request::Request, response::Response};
use kernway_http::{encode_response, parse_bytes, Parsed};
use rt_net::{AsyncTcpStream, ShardConfig};

use crate::{middleware::Middleware, router::Router};

/// Read buffer growth step per connection.
const READ_CHUNK: usize = 8 * 1024;

fn apply_middleware(
    middlewares: &[Arc<dyn Middleware>],
    req: &mut Request,
    endpoint: &dyn Fn(&mut Request) -> Response,
) -> Response {
    match middlewares.split_first() {
        Some((first, rest)) => first.handle(req, &|next_req| apply_middleware(rest, next_req, endpoint)),
        None => endpoint(req),
    }
}

/// App builder — fluent API similar to Spring Boot.
pub struct AppBuilder {
    addr:        String,
    router:      Router,
    context:     AppContext,
    middlewares: Vec<Arc<dyn Middleware>>,
    shards:      Option<usize>,
}

impl AppBuilder {
    pub fn new() -> Self {
        Self {
            addr: "0.0.0.0:8080".to_string(),
            router: Router::new(),
            context: AppContext::new(),
            middlewares: Vec::new(),
            shards: None,
        }
    }

    /// Listening address.
    pub fn bind(mut self, addr: &str) -> Self {
        self.addr = addr.to_string();
        self
    }

    /// Number of shards (threads). Defaults to one per CPU.
    pub fn workers(mut self, workers: usize) -> Self {
        self.shards = Some(workers);
        self
    }

    /// Inject an AppContext with existing beans.
    pub fn context(mut self, ctx: AppContext) -> Self {
        self.context = ctx;
        self
    }

    /// Register a middleware layer.
    pub fn layer(mut self, middleware: impl Middleware) -> Self {
        self.middlewares.push(Arc::new(middleware));
        self
    }

    /// Register a GET route.
    pub fn get(mut self, pattern: &str, handler: impl Fn(&Request, &AppContext) -> Response + Send + Sync + 'static) -> Self {
        self.router.add("GET", pattern, Arc::new(handler));
        self
    }

    /// Register a POST route.
    pub fn post(mut self, pattern: &str, handler: impl Fn(&Request, &AppContext) -> Response + Send + Sync + 'static) -> Self {
        self.router.add("POST", pattern, Arc::new(handler));
        self
    }

    /// Register a PUT route.
    pub fn put(mut self, pattern: &str, handler: impl Fn(&Request, &AppContext) -> Response + Send + Sync + 'static) -> Self {
        self.router.add("PUT", pattern, Arc::new(handler));
        self
    }

    /// Register a DELETE route.
    pub fn delete(mut self, pattern: &str, handler: impl Fn(&Request, &AppContext) -> Response + Send + Sync + 'static) -> Self {
        self.router.add("DELETE", pattern, Arc::new(handler));
        self
    }

    /// Register a PATCH route.
    pub fn patch(mut self, pattern: &str, handler: impl Fn(&Request, &AppContext) -> Response + Send + Sync + 'static) -> Self {
        self.router.add("PATCH", pattern, Arc::new(handler));
        self
    }

    /// Build and return KernwayApp.
    pub fn build(self) -> KernwayApp {
        KernwayApp {
            addr: self.addr,
            shards: self.shards,
            router: Arc::new(self.router),
            context: Arc::new(self.context),
            middlewares: Arc::new(self.middlewares),
        }
    }
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// HTTP application.
pub struct KernwayApp {
    addr:        String,
    shards:      Option<usize>,
    router:      Arc<Router>,
    context:     Arc<AppContext>,
    middlewares: Arc<Vec<Arc<dyn Middleware>>>,
}

impl KernwayApp {
    pub fn builder() -> AppBuilder {
        AppBuilder::new()
    }

    /// Start the server — blocks until the process is stopped.
    ///
    /// Returns an error if the address cannot be parsed or bound; a failure on
    /// one connection never takes the server down.
    pub fn run(self) -> io::Result<()> {
        let addr: SocketAddr = self
            .addr
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("bad address {}: {e}", self.addr)))?;

        let mut config = ShardConfig::new(addr);
        if let Some(shards) = self.shards {
            config = config.shards(shards);
        }

        println!("🚀  Kernway listening on http://{addr}");
        println!("     {} shard(s), press Ctrl+C to stop\n", config.shards);

        let router = Arc::clone(&self.router);
        let context = Arc::clone(&self.context);
        let middlewares = Arc::clone(&self.middlewares);

        rt_net::run_shards(config, move |stream| {
            let router = Arc::clone(&router);
            let context = Arc::clone(&context);
            let middlewares = Arc::clone(&middlewares);
            async move {
                serve_connection(stream, router, context, middlewares).await;
            }
        })
    }
}

/// Read one request off `stream`, answer it, and close.
///
/// `connection: close` is still the contract (keep-alive lands in v0.4), so the
/// task ends after a single exchange.
async fn serve_connection(
    mut stream: AsyncTcpStream,
    router: Arc<Router>,
    context: Arc<AppContext>,
    middlewares: Arc<Vec<Arc<dyn Middleware>>>,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut chunk = vec![0u8; READ_CHUNK];

    let request = loop {
        match parse_bytes(&buf) {
            Ok(Parsed::Complete { request, .. }) => break request,
            Ok(Parsed::Incomplete) => {}
            Err(err) => {
                let response = Response::new(StatusCode::BAD_REQUEST)
                    .content_type("text/plain")
                    .body(err.to_string().into_bytes());
                let _ = stream.write_all(&encode_response(&response)).await;
                return;
            }
        }
        match stream.read(&mut chunk).await {
            // EOF before a complete request: the client went away, or it is a
            // port scan. Nothing to answer.
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };

    let response = handle(request, &router, &context, &middlewares);
    let _ = stream.write_all(&encode_response(&response)).await;
    // Half-close so the peer sees EOF immediately rather than waiting on a
    // timeout for the `connection: close` we promised.
    let _ = stream.shutdown(std::net::Shutdown::Write);
}

/// Run the middleware chain and the matched route.
///
/// A panicking handler becomes a 500 and takes down only its own connection —
/// on a shared shard, letting it unwind would kill every other connection on
/// that core.
fn handle(
    mut request: Request,
    router: &Router,
    context: &AppContext,
    middlewares: &[Arc<dyn Middleware>],
) -> Response {
    let endpoint = |req: &mut Request| match router.find(&req.method, &req.path) {
        Some((handler, params)) => {
            req.path_params = params;
            handler(req, context)
        }
        None => Response::new(StatusCode::NOT_FOUND)
            .content_type("application/json")
            .body(format!(r#"{{"error":"no route for {} {}"}}"#, req.method, req.path).into_bytes()),
    };

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        apply_middleware(middlewares, &mut request, &endpoint)
    }))
    .unwrap_or_else(|_| {
        Response::new(StatusCode::INTERNAL_SERVER_ERROR)
            .content_type("application/json")
            .body(br#"{"error":"internal server error"}"#.to_vec())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(path: &str) -> Request {
        Request::new("GET", path)
    }

    #[test]
    fn unmatched_route_is_a_404_with_a_json_body() {
        let router = Router::new();
        let ctx = AppContext::new();
        let response = handle(get("/nope"), &router, &ctx, &[]);
        assert_eq!(response.status, StatusCode::NOT_FOUND);
        assert!(String::from_utf8_lossy(&response.body).contains("no route for GET /nope"));
    }

    #[test]
    fn a_matched_route_runs_its_handler() {
        let mut router = Router::new();
        router.add("GET", "/ping", Arc::new(|_req, _ctx| {
            Response::new(StatusCode::OK).body(b"pong".to_vec())
        }));
        let response = handle(get("/ping"), &router, &AppContext::new(), &[]);
        assert_eq!(response.body, b"pong");
    }

    #[test]
    fn path_params_reach_the_handler() {
        let mut router = Router::new();
        router.add("GET", "/users/{id}", Arc::new(|req: &Request, _ctx: &AppContext| {
            Response::new(StatusCode::OK).body(req.path_params["id"].clone().into_bytes())
        }));
        let response = handle(get("/users/42"), &router, &AppContext::new(), &[]);
        assert_eq!(response.body, b"42");
    }

    #[test]
    fn a_panicking_handler_becomes_a_500() {
        // On a shared shard an unwinding handler would otherwise take every
        // other connection on that core down with it.
        let mut router = Router::new();
        router.add("GET", "/boom", Arc::new(|_req, _ctx| -> Response {
            panic!("handler exploded")
        }));
        let response = handle(get("/boom"), &router, &AppContext::new(), &[]);
        assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn middleware_wraps_the_handler() {
        struct Tag;
        impl Middleware for Tag {
            fn name(&self) -> &'static str { "Tag" }
            fn handle(&self, req: &mut Request, next: &dyn Fn(&mut Request) -> Response) -> Response {
                let mut resp = next(req);
                resp.headers.insert("x-tag".into(), "seen".into());
                resp
            }
        }

        let mut router = Router::new();
        router.add("GET", "/x", Arc::new(|_req, _ctx| Response::new(StatusCode::OK)));
        let layers: Vec<Arc<dyn Middleware>> = vec![Arc::new(Tag)];
        let response = handle(get("/x"), &router, &AppContext::new(), &layers);
        assert_eq!(response.headers.get("x-tag").unwrap(), "seen");
    }

    /// Serve exactly one connection on an ephemeral port and return what the
    /// client received — the real socket → parse → handle → encode path.
    fn round_trip(router: Router, raw_request: &'static str) -> String {
        use std::io::{Read, Write};

        let ex = rt_core::Executor::new().unwrap();
        let (mut listener, addr) = ex
            .block_on(async {
                let l = rt_net::AsyncTcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
                let a = l.local_addr().unwrap();
                (l, a)
            })
            .unwrap();

        let client = std::thread::spawn(move || {
            let mut sock = std::net::TcpStream::connect(addr).unwrap();
            sock.write_all(raw_request.as_bytes()).unwrap();
            let mut got = String::new();
            sock.read_to_string(&mut got).unwrap();
            got
        });

        let router = Arc::new(router);
        let context = Arc::new(AppContext::new());
        let middlewares: Arc<Vec<Arc<dyn Middleware>>> = Arc::new(Vec::new());
        ex.block_on(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_connection(stream, router, context, middlewares).await;
        })
        .unwrap();

        client.join().unwrap()
    }

    #[test]
    fn serves_a_real_http_request_over_the_async_transport() {
        let mut router = Router::new();
        router.add("GET", "/hello", Arc::new(|_req, _ctx| {
            Response::new(StatusCode::OK)
                .content_type("text/plain")
                .body(b"hi".to_vec())
        }));

        let got = round_trip(router, "GET /hello HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(got.starts_with("HTTP/1.1 200 OK\r\n"), "got {got:?}");
        assert!(got.contains("content-length: 2\r\n"));
        assert!(got.ends_with("hi"));
    }

    #[test]
    fn serves_a_post_body_to_the_handler() {
        let mut router = Router::new();
        router.add("POST", "/echo", Arc::new(|req: &Request, _ctx: &AppContext| {
            Response::new(StatusCode::OK).body(req.body.clone())
        }));

        let got = round_trip(
            router,
            "POST /echo HTTP/1.1\r\ncontent-length: 5\r\n\r\nhello",
        );
        assert!(got.ends_with("hello"), "got {got:?}");
    }

    #[test]
    fn a_malformed_request_gets_a_400_rather_than_a_dropped_connection() {
        let got = round_trip(Router::new(), "GARBAGE\r\n\r\n");
        assert!(got.starts_with("HTTP/1.1 400 Bad Request\r\n"), "got {got:?}");
    }

    #[test]
    fn a_bad_address_is_reported_instead_of_panicking() {
        let err = KernwayApp::builder().bind("not-an-address").build().run().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
