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
use std::time::Duration;

use di_core::AppContext;
use kernway_core::{error::StatusCode, request::Request, response::Response};
use kernway_http::{encode_response, encode_response_with, parse_bytes, Connection, Parsed};
use rt_core::Shutdown;
use rt_net::{AsyncTcpStream, ShardConfig};

use crate::{middleware::Middleware, router::Router};

/// Read buffer growth step per connection.
const READ_CHUNK: usize = 8 * 1024;

/// Bounds on persistent connections.
#[derive(Debug, Clone, Copy)]
pub struct KeepAliveConfig {
    /// Serve more than one request per connection.
    pub enabled: bool,
    /// How long to wait for the next request before closing an idle connection.
    pub idle_timeout: Duration,
    /// Most requests to serve on one connection before closing it.
    pub max_requests: u32,
}

impl Default for KeepAliveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Long enough for a browser to reuse the connection across a page
            // load, short enough that abandoned sockets do not accumulate.
            idle_timeout: Duration::from_secs(5),
            max_requests: 1000,
        }
    }
}

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
    addr:         String,
    router:       Router,
    context:      AppContext,
    middlewares:  Vec<Arc<dyn Middleware>>,
    shards:       Option<usize>,
    keep_alive:   KeepAliveConfig,
    drain:        Duration,
}

impl AppBuilder {
    /// Start a builder with the defaults: `0.0.0.0:8080`, one shard per
    /// available core, and the standard keep-alive and drain timeouts.
    pub fn new() -> Self {
        Self {
            addr: "0.0.0.0:8080".to_string(),
            router: Router::new(),
            context: AppContext::new(),
            middlewares: Vec::new(),
            shards: None,
            keep_alive: KeepAliveConfig::default(),
            drain: rt_net::DEFAULT_DRAIN_TIMEOUT,
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

    /// Tune persistent connections (idle timeout, request cap, on/off).
    pub fn keep_alive(mut self, keep_alive: KeepAliveConfig) -> Self {
        self.keep_alive = keep_alive;
        self
    }

    /// How long in-flight requests get to finish after shutdown is signalled.
    ///
    /// Keep it below the grace period your orchestrator allows (Kubernetes
    /// gives 30s before `SIGKILL`), or the drain is cut off by a kill anyway.
    pub fn drain_timeout(mut self, timeout: Duration) -> Self {
        self.drain = timeout;
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
            keep_alive: self.keep_alive,
            drain: self.drain,
            shutdown: Shutdown::new(),
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
    keep_alive:  KeepAliveConfig,
    drain:       Duration,
    shutdown:     Shutdown,
    router:       Arc<Router>,
    context:      Arc<AppContext>,
    middlewares:  Arc<Vec<Arc<dyn Middleware>>>,
}

impl KernwayApp {
    /// Begin configuring an application.
    pub fn builder() -> AppBuilder {
        AppBuilder::new()
    }

    /// A handle that stops this server.
    ///
    /// Take it *before* [`run`](Self::run), which consumes the app: an admin
    /// endpoint, a test harness, or a supervisor thread triggers it, and every
    /// shard drains and returns.
    ///
    /// ```no_run
    /// # use kernway_server::KernwayApp;
    /// let app = KernwayApp::builder().build();
    /// let stop = app.shutdown_handle();
    /// std::thread::spawn(move || stop.trigger());
    /// app.run().unwrap();
    /// ```
    pub fn shutdown_handle(&self) -> Shutdown {
        self.shutdown.clone()
    }

    /// Start the server, stopping on Ctrl+C (`SIGINT`) or `SIGTERM`.
    ///
    /// Returns once every shard has drained. An error means the address could
    /// not be parsed or bound; a failure on one connection never takes the
    /// server down.
    ///
    /// On a platform with no interrupt support the server still runs — it just
    /// has to be stopped through [`shutdown_handle`](Self::shutdown_handle) or
    /// by killing the process, and says so on stderr rather than pretending the
    /// handler was installed.
    pub fn run(self) -> io::Result<()> {
        let shutdown = self.shutdown.clone();
        if let Err(e) = rt_core::on_interrupt(move || shutdown.trigger()) {
            eprintln!("kernway: Ctrl+C will not shut down gracefully ({e})");
        }
        self.run_until_shutdown()
    }

    /// Like [`run`](Self::run), but without installing any signal handler —
    /// only [`shutdown_handle`](Self::shutdown_handle) stops it.
    ///
    /// This is what an app that already owns its own signal handling wants, and
    /// what tests want: installing a process-wide handler from a test would
    /// change how the whole test binary responds to Ctrl+C.
    pub fn run_until_shutdown(self) -> io::Result<()> {
        let addr: SocketAddr = self
            .addr
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("bad address {}: {e}", self.addr)))?;

        let mut config = ShardConfig::new(addr).drain_timeout(self.drain);
        if let Some(shards) = self.shards {
            config = config.shards(shards);
        }

        println!("🚀  Kernway listening on http://{addr}");
        println!("     {} shard(s), press Ctrl+C to stop\n", config.shards);

        let router = Arc::clone(&self.router);
        let context = Arc::clone(&self.context);
        let middlewares = Arc::clone(&self.middlewares);
        let keep_alive = self.keep_alive;
        let shutdown = self.shutdown.clone();

        let result = rt_net::run_shards_with_shutdown(config, self.shutdown, move |stream| {
            let router = Arc::clone(&router);
            let context = Arc::clone(&context);
            let middlewares = Arc::clone(&middlewares);
            let shutdown = shutdown.clone();
            async move {
                serve_connection(stream, router, context, middlewares, keep_alive, shutdown).await;
            }
        });

        if result.is_ok() {
            println!("👋  Kernway stopped");
        }
        result
    }
}

/// Serve requests on `stream` until the connection closes.
///
/// Persistent by default for HTTP/1.1 (RFC 9112 §9.3). Two bounds keep a
/// kept-alive connection from being a free resource hold:
///
/// - [`KeepAliveConfig::idle_timeout`] — a client that opens a connection and
///   goes quiet is dropped instead of pinning a task and an fd forever;
/// - [`KeepAliveConfig::max_requests`] — an upper bound per connection, so
///   buffers and any per-connection state are eventually reclaimed.
async fn serve_connection(
    mut stream: AsyncTcpStream,
    router: Arc<Router>,
    context: Arc<AppContext>,
    middlewares: Arc<Vec<Arc<dyn Middleware>>>,
    keep_alive: KeepAliveConfig,
    shutdown: Shutdown,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut served: u32 = 0;

    loop {
        // --- Read until one whole request is buffered ---
        let (request, consumed) = loop {
            match parse_bytes(&buf) {
                Ok(Parsed::Complete { request, consumed }) => break (request, consumed),
                Ok(Parsed::Incomplete) => {}
                Err(err) => {
                    let response = Response::new(StatusCode::BAD_REQUEST)
                        .content_type("text/plain")
                        .body(err.to_string().into_bytes());
                    let _ = stream.write_all(&encode_response(&response)).await;
                    return close(&mut stream);
                }
            }
            // The idle timer covers waiting for the *first* byte of a request as
            // well as a stalled one mid-way, which is what a slowloris does.
            let read = rt_core::timeout(keep_alive.idle_timeout, stream.read(&mut chunk));
            // A *kept-alive* connection sitting idle is the common case at
            // shutdown: it holds no request, so waiting out its idle timeout
            // would spend the whole drain budget on a client with nothing to
            // say. Racing the signal closes it at once.
            //
            // Two cases are deliberately excluded:
            //
            // - a non-empty buffer — a half-read request is work in flight, and
            //   finishing it is the point of draining;
            // - `served == 0` — a connection accepted moments before the signal,
            //   whose first request is still on the wire. Closing it would turn
            //   a request the client already sent into a connection reset, which
            //   is the one failure a graceful shutdown is supposed to prevent.
            //   It gets the normal idle timeout; the shard's drain deadline is
            //   the outer bound if it never speaks.
            let read = if buf.is_empty() && served > 0 {
                match rt_core::until_shutdown(&shutdown, read).await {
                    Some(read) => read,
                    None => return close(&mut stream),
                }
            } else {
                read.await
            };
            match read {
                // Timed out, EOF, or a broken connection: nothing left to serve.
                Err(_) | Ok(Ok(0)) | Ok(Err(_)) => return close(&mut stream),
                Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            }
        };

        // --- Answer it ---
        served += 1;
        let client_wants_more = request.wants_keep_alive();
        // A server on its way out answers this request and says so, rather than
        // inviting the client to send another down a connection about to close
        // — the race that turns a rolling restart into stray 502s.
        let persist = keep_alive.enabled
            && client_wants_more
            && served < keep_alive.max_requests
            && !shutdown.is_triggered();

        let response = handle(request, &router, &context, &middlewares);
        let connection = if persist { Connection::KeepAlive } else { Connection::Close };
        if stream
            .write_all(&encode_response_with(&response, connection))
            .await
            .is_err()
        {
            return; // peer vanished mid-write; nothing to half-close
        }

        if !persist {
            return close(&mut stream);
        }

        // Drop this request's bytes; anything left is a pipelined next request
        // and must survive into the following iteration.
        buf.drain(..consumed);
    }
}

/// Half-close the write side so the peer sees EOF at once instead of waiting on
/// its own timeout for the `connection: close` we announced.
fn close(stream: &mut AsyncTcpStream) {
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
    ///
    /// Keep-alive off: the client reads to EOF, so a persistent connection
    /// would make every caller wait out the idle timeout.
    fn round_trip(router: Router, raw_request: &'static str) -> String {
        round_trip_with(router, raw_request, KeepAliveConfig { enabled: false, ..Default::default() })
    }

    fn round_trip_with(router: Router, raw_request: &'static str, keep_alive: KeepAliveConfig) -> String {
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
            serve_connection(stream, router, context, middlewares, keep_alive, Shutdown::new()).await;
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

#[cfg(test)]
mod keep_alive_tests {
    use super::*;
    use kernway_core::request::HttpVersion;
    use std::io::{Read, Write};
    use std::time::Instant;

    fn ping_router() -> Router {
        let mut router = Router::new();
        router.add("GET", "/n", Arc::new(|_req, _ctx| {
            Response::new(StatusCode::OK).body(b"ok".to_vec())
        }));
        router
    }

    /// Serve one connection with `keep_alive`, driving it from a client closure.
    fn with_server<T: Send + 'static>(
        keep_alive: KeepAliveConfig,
        client: impl FnOnce(std::net::TcpStream) -> T + Send + 'static,
    ) -> T {
        with_server_shutdown(keep_alive, Shutdown::new(), |sock, _shutdown| client(sock))
    }

    /// As above, but the client closure also gets the shutdown handle — so a
    /// test can stop the server from the client's side, mid-connection, the way
    /// a real signal arrives while a connection is open.
    fn with_server_shutdown<T: Send + 'static>(
        keep_alive: KeepAliveConfig,
        shutdown: Shutdown,
        client: impl FnOnce(std::net::TcpStream, Shutdown) -> T + Send + 'static,
    ) -> T {
        let ex = rt_core::Executor::new().unwrap();
        let (mut listener, addr) = ex
            .block_on(async {
                let l = rt_net::AsyncTcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
                let a = l.local_addr().unwrap();
                (l, a)
            })
            .unwrap();

        let for_client = shutdown.clone();
        let handle = std::thread::spawn(move || {
            client(std::net::TcpStream::connect(addr).unwrap(), for_client)
        });

        let router = Arc::new(ping_router());
        let context = Arc::new(AppContext::new());
        let middlewares: Arc<Vec<Arc<dyn Middleware>>> = Arc::new(Vec::new());
        ex.block_on(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_connection(stream, router, context, middlewares, keep_alive, shutdown).await;
        })
        .unwrap();

        handle.join().unwrap()
    }

    /// Read one response off `sock`, using content-length to know where it ends
    /// — the same framing a real client relies on to reuse the connection.
    fn read_one(sock: &mut std::net::TcpStream) -> String {
        let mut got = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let n = sock.read(&mut byte).unwrap();
            assert_ne!(n, 0, "connection closed mid-response: {:?}", String::from_utf8_lossy(&got));
            got.push(byte[0]);
            if let Some(head_end) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&got[..head_end]).to_lowercase();
                let len: usize = head
                    .split("content-length:")
                    .nth(1)
                    .and_then(|rest| rest.split("\r\n").next())
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if got.len() == head_end + 4 + len {
                    return String::from_utf8(got).unwrap();
                }
            }
        }
    }

    #[test]
    fn three_requests_are_served_on_one_connection() {
        let responses = with_server(KeepAliveConfig::default(), |mut sock| {
            let mut out = Vec::new();
            for _ in 0..3 {
                sock.write_all(b"GET /n HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
                out.push(read_one(&mut sock));
            }
            // Tell the server we are done so it does not sit out the idle timeout.
            sock.write_all(b"GET /n HTTP/1.1\r\nHost: x\r\nconnection: close\r\n\r\n").unwrap();
            out.push(read_one(&mut sock));
            out
        });

        for response in &responses[..3] {
            assert!(response.contains("connection: keep-alive"), "got {response:?}");
            assert!(response.ends_with("ok"));
        }
        assert!(responses[3].contains("connection: close"), "got {:?}", responses[3]);
    }

    #[test]
    fn pipelined_requests_in_one_packet_are_both_answered() {
        // Both requests arrive in a single read; the leftover bytes of the
        // first read must survive into the next loop iteration.
        let responses = with_server(KeepAliveConfig::default(), |mut sock| {
            sock.write_all(
                b"GET /n HTTP/1.1\r\nHost: x\r\n\r\nGET /n HTTP/1.1\r\nHost: x\r\nconnection: close\r\n\r\n",
            )
            .unwrap();
            (read_one(&mut sock), read_one(&mut sock))
        });
        assert!(responses.0.ends_with("ok"));
        assert!(responses.1.ends_with("ok"));
        assert!(responses.1.contains("connection: close"));
    }

    #[test]
    fn an_http10_client_gets_a_closed_connection_by_default() {
        let response = with_server(KeepAliveConfig::default(), |mut sock| {
            sock.write_all(b"GET /n HTTP/1.0\r\nHost: x\r\n\r\n").unwrap();
            let mut got = String::new();
            sock.read_to_string(&mut got).unwrap(); // returns only on EOF
            got
        });
        assert!(response.contains("connection: close"), "got {response:?}");
    }

    #[test]
    fn an_idle_connection_is_closed_after_the_timeout() {
        // Without this bound a client could hold a task and an fd indefinitely
        // by connecting and saying nothing.
        let started = Instant::now();
        let keep_alive = KeepAliveConfig {
            idle_timeout: Duration::from_millis(150),
            ..Default::default()
        };
        let response = with_server(keep_alive, |mut sock| {
            sock.write_all(b"GET /n HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut got = String::new();
            sock.read_to_string(&mut got).unwrap(); // blocks until the server gives up
            got
        });
        assert!(response.contains("connection: keep-alive"));
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(150), "closed too early: {waited:?}");
        assert!(waited < Duration::from_secs(3), "idle timeout did not fire: {waited:?}");
    }

    #[test]
    fn max_requests_closes_the_connection_even_if_the_client_wants_more() {
        let keep_alive = KeepAliveConfig { max_requests: 2, ..Default::default() };
        let responses = with_server(keep_alive, |mut sock| {
            let mut out = Vec::new();
            for _ in 0..2 {
                sock.write_all(b"GET /n HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
                out.push(read_one(&mut sock));
            }
            out
        });
        assert!(responses[0].contains("connection: keep-alive"));
        assert!(
            responses[1].contains("connection: close"),
            "the cap must be announced, not just enforced silently: {:?}",
            responses[1]
        );
    }

    #[test]
    fn keep_alive_can_be_turned_off_entirely() {
        let config = KeepAliveConfig { enabled: false, ..Default::default() };
        let response = with_server(config, |mut sock| {
            sock.write_all(b"GET /n HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut got = String::new();
            sock.read_to_string(&mut got).unwrap();
            got
        });
        assert!(response.contains("connection: close"), "got {response:?}");
    }

    #[test]
    fn an_idle_kept_alive_connection_closes_as_soon_as_shutdown_is_signalled() {
        // The common shape at shutdown: a browser holding a connection open with
        // nothing to send. Waiting out its idle timeout would spend the whole
        // drain budget on a client that has no work in flight at all.
        let keep_alive = KeepAliveConfig {
            idle_timeout: Duration::from_secs(30),
            ..Default::default()
        };
        let (response, waited) = with_server_shutdown(keep_alive, Shutdown::new(), |mut sock, shutdown| {
            sock.write_all(b"GET /n HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let first = read_one(&mut sock);
            let started = Instant::now();
            shutdown.trigger();
            let mut rest = String::new();
            sock.read_to_string(&mut rest).unwrap(); // returns on EOF
            (first, started.elapsed())
        });

        assert!(response.contains("connection: keep-alive"));
        assert!(
            waited < Duration::from_secs(5),
            "the connection sat out its 30s idle timeout instead of closing: {waited:?}"
        );
    }

    #[test]
    fn a_request_arriving_during_shutdown_is_answered_and_the_connection_closed() {
        // Answering and announcing `close` is what keeps a rolling restart from
        // producing stray errors: the client is told not to reuse this socket
        // instead of discovering it the hard way on its next request.
        let shutdown = Shutdown::new();
        shutdown.trigger();
        let response = with_server_shutdown(KeepAliveConfig::default(), shutdown, |mut sock, _| {
            sock.write_all(b"GET /n HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut got = String::new();
            sock.read_to_string(&mut got).unwrap();
            got
        });
        assert!(response.contains("HTTP/1.1 200 OK"), "got {response:?}");
        assert!(response.ends_with("ok"), "the request was dropped: {response:?}");
        assert!(response.contains("connection: close"), "got {response:?}");
    }

    #[test]
    fn a_half_sent_request_is_still_finished_after_the_signal() {
        // Shutdown must not cut a request that is mid-flight on the wire —
        // that is precisely the in-flight work the drain exists to protect.
        let shutdown = Shutdown::new();
        let response = with_server_shutdown(KeepAliveConfig::default(), shutdown, |mut sock, shutdown| {
            sock.write_all(b"GET /n HTTP/1.1\r\nHo").unwrap();
            sock.flush().unwrap();
            std::thread::sleep(Duration::from_millis(30)); // the server is now mid-request
            shutdown.trigger();
            sock.write_all(b"st: x\r\n\r\n").unwrap();
            let mut got = String::new();
            sock.read_to_string(&mut got).unwrap();
            got
        });
        assert!(response.ends_with("ok"), "the half-read request was abandoned: {response:?}");
    }

    #[test]
    fn the_parser_reports_the_version_the_client_sent() {
        let req = kernway_http::parse_bytes(b"GET / HTTP/1.0\r\n\r\n").unwrap();
        match req {
            Parsed::Complete { request, .. } => assert_eq!(request.version, HttpVersion::Http10),
            Parsed::Incomplete => panic!("expected a complete request"),
        }
    }
}
