//! KernwayApp — app builder + HTTP server.
//!
//! The transport is [`rt_net`]: one shard per core, each with its own
//! `SO_REUSEPORT` listener and executor, one task per connection. Handlers and
//! middleware are still synchronous — they run to completion inside the
//! connection's task — so porting the transport did not change a single
//! handler signature. Making them `async` is the next step, and belongs with
//! the `kernway-core` spec work.

use std::any::Any;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskCx, Poll};
use std::time::Duration;

use di_core::{AppContext, RequestScope};
use kernway_config::{Config, FromConfig};
use kernway_core::{error::StatusCode, request::Request, response::{Body, Response}};
use kernway_http::{encode_head, encode_response, encode_response_with, parse_head, Connection, ParsedHead};
use kernway_static::{mime_for, StaticFiles};
use rt_core::Shutdown;
use rt_net::{AsyncTcpStream, ShardConfig};

use crate::{
    middleware::{Middleware, Next},
    router::Router,
};
use kernway_core::layer::BoxFuture;

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

/// Bounds on request bodies — where the line between buffering in memory and streaming
/// to disk falls, and the hard ceiling above which an upload is refused.
#[derive(Debug, Clone)]
pub struct UploadConfig {
    /// Bodies up to this size are read into memory (`Request.body`); larger ones stream
    /// to a temp file (`Request.body_spool`). Keeps the common small-body path allocation-
    /// light while large uploads stay O(chunk). Default: 1 MiB.
    pub max_inmemory_body: usize,
    /// Hard ceiling: a body larger than this is refused with `413`, never spooled — the
    /// backstop against a disk-filling upload. Default: 4 GiB.
    pub max_upload_size: u64,
    /// Directory for spooled upload temp files. Default: [`std::env::temp_dir`].
    pub temp_dir: std::path::PathBuf,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            max_inmemory_body: 1024 * 1024,              // 1 MiB — covers virtually all JSON/form posts
            max_upload_size: 4 * 1024 * 1024 * 1024,     // 4 GiB — generous for media, bounded vs DoS
            temp_dir: std::env::temp_dir(),
        }
    }
}

/// Run the matched handler (the terminal of the middleware chain). The handler
/// owns the request and returns a `'static` future, so nothing here is borrowed
/// across its `await`.
fn run_handler(req: Request, router: &Router, scope: &RequestScope) -> BoxFuture<'static, Response> {
    match router.find(&req.method, &req.path) {
        Some((handler, params)) => {
            let mut req = req;
            req.path_params = params;
            handler(req, scope)
        }
        None => {
            let body = format!(r#"{{"error":"no route for {} {}"}}"#, req.method, req.path);
            Box::pin(async move {
                Response::new(StatusCode::NOT_FOUND).content_type("application/json").body(body.into_bytes())
            })
        }
    }
}

/// App builder — fluent API similar to Spring Boot.
pub struct AppBuilder {
    addr:         String,
    /// Whether `bind` was called — if not, the address may come from config.
    addr_explicit: bool,
    router:       Router,
    context:      AppContext,
    middlewares:  Vec<Arc<dyn Middleware>>,
    static_files: Option<Arc<StaticFiles>>,
    precompressed: bool,
    file_chunk:   usize,
    upload:       UploadConfig,
    shards:       Option<usize>,
    keep_alive:   KeepAliveConfig,
    drain:        Duration,
    /// The application config; loaded from disk + env at `build` if not provided.
    config:       Option<Config>,
    /// Deferred typed-config bean registrations, run once the config is resolved.
    config_beans: Vec<Box<dyn FnOnce(&Config, &mut AppContext)>>,
    /// Custom response for a caught handler panic (`on_panic`); default 500 if unset.
    panic_handler: Option<Box<dyn Fn(&str) -> Response + Send + Sync>>,
}

impl AppBuilder {
    /// Start a builder with the defaults: `0.0.0.0:8080`, one shard per
    /// available core, and the standard keep-alive and drain timeouts.
    pub fn new() -> Self {
        Self {
            addr: "0.0.0.0:8080".to_string(),
            addr_explicit: false,
            router: Router::new(),
            context: AppContext::new(),
            middlewares: Vec::new(),
            static_files: None,
            precompressed: false,
            file_chunk: FILE_CHUNK,
            upload: UploadConfig::default(),
            shards: None,
            keep_alive: KeepAliveConfig::default(),
            drain: rt_net::DEFAULT_DRAIN_TIMEOUT,
            config: None,
            config_beans: Vec::new(),
            panic_handler: None,
        }
    }

    /// Listening address. Overrides any `server.address`/`server.port` from config.
    pub fn bind(mut self, addr: &str) -> Self {
        self.addr = addr.to_string();
        self.addr_explicit = true;
        self
    }

    /// Provide the application [`Config`]. If omitted, [`Config::load`] reads
    /// `application.properties` + the profile file + `KW_` env at `build` time.
    pub fn config(mut self, config: Config) -> Self {
        self.config = Some(config);
        self
    }

    /// Register a typed config bean (`#[configuration]`): it is built from the
    /// resolved config with [`FromConfig`] and made injectable like any bean.
    pub fn configure<T: FromConfig + Send + Sync + 'static>(mut self) -> Self {
        self.config_beans.push(Box::new(|config, context| {
            let _ = context.register_instance::<T>(Arc::new(T::from_config(config)));
        }));
        self
    }

    /// Customise the response when a handler panics (default: a 500 RFC 7807). The
    /// closure receives the panic message; return whatever response you want —
    /// central, customisable error handling for the unexpected-failure case.
    ///
    /// ```rust,ignore
    /// KernwayApp::builder().on_panic(|msg| {
    ///     eprintln!("panic: {msg}");
    ///     Response::new(StatusCode::INTERNAL_SERVER_ERROR).body(b"oops".to_vec())
    /// })
    /// ```
    pub fn on_panic<F>(mut self, handler: F) -> Self
    where
        F: Fn(&str) -> Response + Send + Sync + 'static,
    {
        self.panic_handler = Some(Box::new(handler));
        self
    }

    /// Mount a `#[controller]` — register all of its `#[route]` methods (Spring's
    /// component-scan of a `@Controller`, one call).
    pub fn controller<C: Controller>(self, controller: Arc<C>) -> Self {
        controller.register(self)
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

    /// Serve static files from `root` for any GET the router does not handle.
    ///
    /// A request is tried against the router first, so a dynamic route always
    /// wins; only misses fall through to the filesystem. `/` and any path ending
    /// in `/` serve `index.html` from that directory. Path traversal, dotfiles,
    /// and malformed encodings are rejected before any file is opened — see
    /// [`kernway_static::StaticFiles::resolve`].
    ///
    /// ```no_run
    /// # use kernway_server::KernwayApp;
    /// KernwayApp::builder()
    ///     .static_files("public")   // drop index.html, css, js in ./public
    ///     .build()
    ///     .run()
    ///     .unwrap();
    /// ```
    pub fn static_files(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.static_files = Some(Arc::new(StaticFiles::new(root)));
        self
    }

    /// Serve a precompressed `.br`/`.gz` sitting next to a compressible file when
    /// the client accepts it — no CPU spent compressing on the request path, just
    /// a file a build step produced ([KEP-0000 §4]).
    ///
    /// Off by default; order-independent with [`static_files`](Self::static_files).
    /// Only text-tier types are probed (see [`kernway_static::is_compressible`]),
    /// and negotiated responses carry `Vary: Accept-Encoding`.
    ///
    /// ```no_run
    /// # use kernway_server::KernwayApp;
    /// KernwayApp::builder()
    ///     .static_files("public")   // ship app.js.br beside app.js at build time
    ///     .precompressed()
    ///     .build()
    ///     .run()
    ///     .unwrap();
    /// ```
    ///
    /// [KEP-0000 §4]: https://github.com/tacpham/kernway/blob/main/docs/kep/0000-principles.md
    pub fn precompressed(mut self) -> Self {
        self.precompressed = true;
        self
    }

    /// Chunk size for streaming a file body, in bytes (default 64 KiB).
    ///
    /// Each chunk is one `read` on the blocking pool and one socket `write`, so
    /// this trades syscall count against per-connection memory. The default was
    /// measured against a large-file download — see
    /// [BENCHMARKS.md](https://github.com/tacpham/kernway/blob/main/docs/design/BENCHMARKS.md);
    /// override it only if a profile of your workload says to.
    pub fn file_chunk_size(mut self, bytes: usize) -> Self {
        self.file_chunk = bytes.max(1);
        self
    }

    /// Largest request body read into memory, in bytes (default 1 MiB). Bodies over this
    /// stream to a temp file instead (reached via `UploadFile`/`Multipart`), so memory
    /// stays O(chunk) for large uploads. Raise it if your handlers routinely take larger
    /// in-memory bodies (e.g. big JSON); lower it to push more uploads straight to disk.
    pub fn max_inmemory_body(mut self, bytes: usize) -> Self {
        self.upload.max_inmemory_body = bytes;
        self
    }

    /// Hard ceiling on a request body, in bytes (default 4 GiB). A larger body is refused
    /// with `413 Payload Too Large` before any of it is spooled — the backstop against a
    /// disk-filling upload. Size it to your largest legitimate upload.
    pub fn max_upload_size(mut self, bytes: u64) -> Self {
        self.upload.max_upload_size = bytes;
        self
    }

    /// Directory for spooled upload temp files (default [`std::env::temp_dir`]). Point it
    /// at the same filesystem as your final storage so `UploadFile::persist` can rename
    /// rather than copy across devices.
    pub fn upload_temp_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.upload.temp_dir = dir.into();
        self
    }

    /// Register a GET route.
    pub fn get<M>(mut self, pattern: &str, handler: impl crate::router::IntoHandler<M>) -> Self {
        self.router.add("GET", pattern, handler.into_handler());
        self
    }

    /// Register a POST route.
    pub fn post<M>(mut self, pattern: &str, handler: impl crate::router::IntoHandler<M>) -> Self {
        self.router.add("POST", pattern, handler.into_handler());
        self
    }

    /// Register a PUT route.
    pub fn put<M>(mut self, pattern: &str, handler: impl crate::router::IntoHandler<M>) -> Self {
        self.router.add("PUT", pattern, handler.into_handler());
        self
    }

    /// Register a DELETE route.
    pub fn delete<M>(mut self, pattern: &str, handler: impl crate::router::IntoHandler<M>) -> Self {
        self.router.add("DELETE", pattern, handler.into_handler());
        self
    }

    /// Register a PATCH route.
    pub fn patch<M>(mut self, pattern: &str, handler: impl crate::router::IntoHandler<M>) -> Self {
        self.router.add("PATCH", pattern, handler.into_handler());
        self
    }

    /// Build and return KernwayApp.
    pub fn build(mut self) -> KernwayApp {
        // Resolve the config (explicit, else load from disk + env), and let it
        // drive logging and — unless `bind` was called — the listen address.
        let config = self.config.take().unwrap_or_else(Config::load);
        init_logging_from_config(&config);
        if !self.addr_explicit {
            if let Some(addr) = address_from_config(&config) {
                self.addr = addr;
            }
        }
        // Build the typed config beans, then register the Config itself so a
        // handler can inject `&Config`.
        for register in std::mem::take(&mut self.config_beans) {
            register(&config, &mut self.context);
        }
        let _ = self.context.register_instance::<Config>(Arc::new(config));
        // The custom panic response (if any), as a bean the dispatch resolves.
        if let Some(handler) = self.panic_handler.take() {
            let _ = self.context.register_instance::<PanicHandler>(Arc::new(PanicHandler(handler)));
        }

        // Apply the precompression toggle here so it is independent of whether
        // `.precompressed()` was called before or after `.static_files()`.
        let static_files = match (self.static_files, self.precompressed) {
            (Some(sf), true) => Some(Arc::new((*sf).clone().precompressed())),
            (other, _) => other,
        };
        KernwayApp {
            addr: self.addr,
            shards: self.shards,
            keep_alive: self.keep_alive,
            drain: self.drain,
            shutdown: Shutdown::new(),
            router: Arc::new(self.router),
            context: Arc::new(self.context),
            middlewares: Arc::new(self.middlewares),
            static_files,
            file_chunk: self.file_chunk,
            upload: Arc::new(self.upload),
        }
    }
}

/// Install the process logger from config — `logging.level` is the default level,
/// each `logging.level.<module>` an override, `logging.format` picks Pretty/JSON.
/// The `KW_LOG` env var, if set, is the explicit top override (a quick full spec,
/// like `RUST_LOG`) and wins over the config-derived filter. A no-op if a logger
/// was already installed (an explicit `kernway_log::init`).
fn init_logging_from_config(config: &Config) {
    let filter = match std::env::var("KW_LOG") {
        // KW_LOG is the deliberate override — use it verbatim.
        Ok(spec) if !spec.trim().is_empty() => kernway_log::Filter::parse(&spec),
        // Otherwise build the spec from `logging.level` + each `logging.level.*`.
        // (Per-module env overrides still arrive via KW_LOGGING__LEVEL__* → config.)
        _ => {
            let mut spec = config.get_str("logging.level").unwrap_or("info").to_string();
            for (module, level) in config.with_prefix("logging.level.") {
                spec.push(',');
                spec.push_str(module);
                spec.push('=');
                spec.push_str(level);
            }
            kernway_log::Filter::parse(&spec)
        }
    };
    let format = match config.get_str("logging.format") {
        Some("json") => kernway_log::Format::Json,
        _ => kernway_log::Format::Pretty,
    };
    kernway_log::init(kernway_log::Logger::new(filter, format));
}

/// A listen address from config: `server.address`, else `server.host:server.port`
/// (host defaults to `0.0.0.0` when only a port is given). `None` if neither is set.
fn address_from_config(config: &Config) -> Option<String> {
    if let Some(address) = config.get_str("server.address") {
        return Some(address.to_string());
    }
    let host = config.get_str("server.host");
    match (host, config.get_str("server.port")) {
        (Some(host), Some(port)) => Some(format!("{host}:{port}")),
        (None, Some(port)) => Some(format!("0.0.0.0:{port}")),
        _ => None,
    }
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// A group of routes on a struct — Spring's `@Controller`. Written with
/// `#[controller("/prefix")]` on an `impl` block; each `#[route(METHOD, "/path")]`
/// method becomes a handler, and `#[require_role("ROLE")]` guards it. The struct's
/// fields are its dependencies (injected at construction). Mount with
/// [`AppBuilder::controller`].
pub trait Controller {
    /// Register this controller's routes onto the builder — generated by
    /// `#[controller]`.
    fn register(self: Arc<Self>, app: AppBuilder) -> AppBuilder;
}

/// Whether the request carries `role` — the `SecurityContext` the auth middleware
/// put in the scope (KEP-0005) has it. No context (unauthenticated) → not allowed.
/// This is what `#[require_role("ROLE")]` compiles to, so a controller crate needs
/// only `kernway-server`, not `kernway-security`, in scope.
pub fn role_allowed(scope: &RequestScope, role: &str) -> bool {
    scope
        .get::<kernway_security::SecurityContext>()
        .map(|ctx| ctx.has_role(role))
        .unwrap_or(false)
}

/// The `403` a failed `#[require_role]` returns — RFC 7807, no detail leaked.
#[must_use]
pub fn forbidden() -> Response {
    Response::new(StatusCode::FORBIDDEN)
        .content_type("application/json; charset=utf-8")
        .body(br#"{"status":403,"title":"Forbidden","detail":"insufficient role"}"#.to_vec())
}

/// The `401` for a request that needs a login but has none — RFC 7807.
#[must_use]
pub fn unauthorized() -> Response {
    Response::new(StatusCode::UNAUTHORIZED)
        .content_type("application/json; charset=utf-8")
        .body(br#"{"status":401,"title":"Unauthorized","detail":"authentication required"}"#.to_vec())
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
    static_files: Option<Arc<StaticFiles>>,
    file_chunk:   usize,
    upload:       Arc<UploadConfig>,
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
            kernway_log::warn!(target: "kernway_server", "Ctrl+C will not shut down gracefully: {e}");
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
        let static_files = self.static_files.clone();
        let keep_alive = self.keep_alive;
        let file_chunk = self.file_chunk;
        let upload = Arc::clone(&self.upload);
        let shutdown = self.shutdown.clone();

        let result = rt_net::run_shards_with_shutdown(config, self.shutdown, move |stream| {
            let router = Arc::clone(&router);
            let context = Arc::clone(&context);
            let middlewares = Arc::clone(&middlewares);
            let static_files = static_files.clone();
            let upload = Arc::clone(&upload);
            let shutdown = shutdown.clone();
            async move {
                serve_connection(stream, router, context, middlewares, static_files, keep_alive, file_chunk, upload, shutdown).await;
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
    static_files: Option<Arc<StaticFiles>>,
    keep_alive: KeepAliveConfig,
    file_chunk: usize,
    upload: Arc<UploadConfig>,
    shutdown: Shutdown,
) {
    let mut buf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut served: u32 = 0;
    // The peer address is per-connection; stamp it on each request so the app can
    // resolve the real client (directly, or via forwarded headers behind a proxy).
    let peer = stream.peer_addr().ok();

    loop {
        // --- Read the head, then the body (small → memory, large → temp file) ---
        let (mut request, consumed) = {
            // Read until the head (request line + headers) is buffered. The body is
            // handled separately below, so a multi-GB upload never grows this buffer.
            let (head_req, head_end, content_length) = loop {
                match parse_head(&buf) {
                    Ok(ParsedHead::Complete { request, head_end, content_length }) => {
                        break (request, head_end, content_length)
                    }
                    Ok(ParsedHead::Incomplete) => {}
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

            // Refuse an oversized upload before reading (or spooling) any of its body.
            if content_length as u64 > upload.max_upload_size {
                let response = Response::new(StatusCode::PAYLOAD_TOO_LARGE)
                    .content_type("text/plain")
                    .body(b"upload exceeds the configured maximum".to_vec());
                let _ = stream.write_all(&encode_response(&response)).await;
                return close(&mut stream);
            }

            if content_length <= upload.max_inmemory_body {
                // Small body: finish buffering it in memory (the fast path).
                let total = head_end + content_length;
                while buf.len() < total {
                    match rt_core::timeout(keep_alive.idle_timeout, stream.read(&mut chunk)).await {
                        Err(_) | Ok(Ok(0)) | Ok(Err(_)) => return close(&mut stream),
                        Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let mut request = head_req;
                request.body = buf[head_end..total].to_vec();
                (request, total)
            } else {
                // Large body: stream it to a temp file — memory stays O(chunk). This
                // drains the head+body from `buf`, leaving any pipelined tail.
                match spool_body(&mut stream, &mut buf, head_end, content_length, &upload, file_chunk, keep_alive.idle_timeout)
                    .await
                {
                    Ok(spooled) => {
                        let mut request = head_req;
                        request.body_spool = Some(spooled);
                        (request, 0) // `buf` already drained by spool_body
                    }
                    Err(_) => return close(&mut stream),
                }
            }
        };

        // --- Answer it ---
        request.remote_addr = peer;
        served += 1;
        let client_wants_more = request.wants_keep_alive();
        // A server on its way out answers this request and says so, rather than
        // inviting the client to send another down a connection about to close
        // — the race that turns a rolling restart into stray 502s.
        let persist = keep_alive.enabled
            && client_wants_more
            && served < keep_alive.max_requests
            && !shutdown.is_triggered();

        // Captured before the match: a `None` from `try_static` moves `request`
        // into `handle`, so the method must be read first.
        let is_head = request.method.eq_ignore_ascii_case("HEAD");

        // A static-file hit is served from the blocking pool so the read never
        // stalls this shard; a miss (no root configured, an unsupported method, a
        // route claims the path, or no such file) falls through to the router.
        let response = match try_static(static_files.as_deref(), &router, &request).await {
            Some(file_response) => file_response,
            None => handle(request, &router, &context, &middlewares).await,
        };
        let connection = if persist { Connection::KeepAlive } else { Connection::Close };
        if write_response(&mut stream, &response, connection, is_head, file_chunk).await.is_err() {
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

/// Stream a request body from the socket straight to a temporary file, in `file_chunk`
/// reads, each file write on the blocking pool so the shard never stalls. Memory is
/// O(chunk), not O(body) — the inbound mirror of [`stream_file`].
///
/// Consumes the head and exactly `content_length` body bytes from `buf`, leaving any
/// pipelined bytes of the *next* request at the front of `buf`.
async fn spool_body(
    stream: &mut AsyncTcpStream,
    buf: &mut Vec<u8>,
    head_end: usize,
    content_length: usize,
    upload: &UploadConfig,
    file_chunk: usize,
    idle_timeout: Duration,
) -> io::Result<kernway_core::request::SpooledBody> {
    use std::io::Write; // for `file.flush()` on the blocking pool
    let blocking_gone = || io::Error::new(io::ErrorKind::Other, "blocking pool unavailable");
    let path = upload_temp_path(&upload.temp_dir);

    // Create the temp file on the blocking pool.
    let mut file = {
        let path = path.clone();
        match rt_core::spawn_blocking(move || std::fs::File::create(&path)).await {
            Some(result) => result?,
            None => return Err(blocking_gone()),
        }
    };

    let mut written = 0usize;

    // Body bytes already read alongside the head go to the file first.
    let body_have = buf.len() - head_end;
    let from_buf = content_length.min(body_have);
    if from_buf > 0 {
        let data = buf[head_end..head_end + from_buf].to_vec();
        file = write_all_blocking(file, data).await?;
        written += from_buf;
    }
    // Everything past the body is the next pipelined request — keep it as the new `buf`.
    *buf = buf.split_off(head_end + from_buf);

    // Stream the rest of the body from the socket.
    let mut chunk = vec![0u8; file_chunk];
    while written < content_length {
        let n = match rt_core::timeout(idle_timeout, stream.read(&mut chunk)).await {
            Ok(Ok(n)) => n,
            _ => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "upload stalled or closed")),
        };
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed mid-upload"));
        }
        let take = (content_length - written).min(n);
        file = write_all_blocking(file, chunk[..take].to_vec()).await?;
        written += take;
        if n > take {
            // Overshoot past the body belongs to the next request.
            buf.extend_from_slice(&chunk[take..n]);
        }
    }

    // Flush before a handler is handed the path.
    match rt_core::spawn_blocking(move || file.flush()).await {
        Some(result) => result?,
        None => return Err(blocking_gone()),
    }

    Ok(kernway_core::request::SpooledBody { path, len: content_length as u64 })
}

/// Append `data` to `file` on the blocking pool, handing the file back to keep writing.
async fn write_all_blocking(file: std::fs::File, data: Vec<u8>) -> io::Result<std::fs::File> {
    use std::io::Write;
    match rt_core::spawn_blocking(move || {
        let mut file = file;
        file.write_all(&data).map(|()| file)
    })
    .await
    {
        Some(result) => result,
        None => Err(io::Error::new(io::ErrorKind::Other, "blocking pool unavailable")),
    }
}

/// A unique temp-file path for a spooled upload (`<dir>/kernway-upload-<pid>-<nanos>-<n>.tmp`).
fn upload_temp_path(dir: &std::path::Path) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!("kernway-upload-{}-{}-{}.tmp", std::process::id(), nanos, n))
}

/// Default chunk size for streaming a file body, overridable with
/// [`AppBuilder::file_chunk_size`].
///
/// Measured, not guessed (see BENCHMARKS.md, `stream_chunk`): download throughput
/// climbs steeply with chunk size — the old 64 KiB sat at ~41% of peak, because
/// each chunk is a `spawn_blocking` hop whose overhead dominates when the chunk is
/// small — and peaks near 4 MiB before cache pressure drops it. 256 KiB is the
/// default: ~1.75× the throughput of 64 KiB while keeping per-connection memory
/// bounded (it multiplies by concurrency). A download-heavy server can raise it.
const FILE_CHUNK: usize = 256 * 1024;

/// Write a response. An in-memory body goes out in one buffer (head and bytes
/// coalesced); a [`Body::File`] streams — the head first, then the file in
/// bounded chunks, each read on the blocking pool so it never stalls the shard.
async fn write_response(
    stream: &mut AsyncTcpStream,
    response: &Response,
    connection: Connection,
    is_head: bool,
    file_chunk: usize,
) -> std::io::Result<()> {
    // A HEAD gets exactly the headers a GET would — including the Content-Length
    // the body *would* have — and no body. `encode_head` takes the length
    // separately, so a File's length is sent without the File being read.
    if is_head {
        let head = encode_head(response, connection, response.body.len());
        return stream.write_all(&head).await;
    }
    match &response.body {
        Body::File { path, len, range } => {
            let head = encode_head(response, connection, response.body.len());
            stream.write_all(&head).await?;
            stream_file(stream, path.clone(), *range, *len, file_chunk).await
        }
        Body::Empty | Body::Bytes(_) => {
            stream.write_all(&encode_response_with(response, connection)).await
        }
    }
}

/// Stream a file (or a byte range of it) to the socket, chunk by chunk. Each
/// open, seek, and read runs on the blocking pool via `spawn_blocking`; only the
/// socket write is on the shard. Memory is O(chunk), not O(file).
///
/// The head — with its `Content-Length` — is already on the wire, so a read
/// failure mid-stream cannot be signalled in band; the connection is simply
/// closed (returning `Ok`, since there is nothing more to do).
async fn stream_file(
    stream: &mut AsyncTcpStream,
    path: std::path::PathBuf,
    range: Option<(u64, u64)>,
    len: u64,
    file_chunk: usize,
) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};

    let start = range.map_or(0, |(s, _)| s);
    let total = range.map_or(len, |(s, e)| e - s);

    // Open and seek on the blocking pool.
    let opened = rt_core::spawn_blocking(move || {
        let mut f = std::fs::File::open(&path)?;
        if start > 0 {
            f.seek(SeekFrom::Start(start))?;
        }
        std::io::Result::Ok(f)
    })
    .await;
    let mut file = match opened {
        Some(Ok(f)) => f,
        _ => return Ok(()), // open failed after the head went out — close
    };

    let mut remaining = total;
    while remaining > 0 {
        let want = remaining.min(file_chunk as u64) as usize;
        // Read one chunk on the blocking pool; move the file in and back out.
        let read = rt_core::spawn_blocking(move || {
            let mut buf = vec![0u8; want];
            let mut got = 0;
            while got < buf.len() {
                match file.read(&mut buf[got..])? {
                    0 => break, // EOF
                    n => got += n,
                }
            }
            buf.truncate(got);
            std::io::Result::Ok((buf, file))
        })
        .await;
        let (chunk, returned) = match read {
            Some(Ok(x)) => x,
            _ => return Ok(()),
        };
        if chunk.is_empty() {
            break; // EOF before the expected length — stop
        }
        file = returned;
        stream.write_all(&chunk).await?;
        remaining -= chunk.len() as u64;
    }
    Ok(())
}

/// What the blocking file lookup decided. Note it carries no file *contents* —
/// the file is named, not read; the connection task streams it (KEP-0002).
enum StaticOutcome {
    /// The client's cached copy is current — send `304` with the validator, no body.
    NotModified {
        etag: String,
        /// Emit `Vary: Accept-Encoding` — set when the resource was negotiated.
        vary_encoding: bool,
    },
    /// Send the file with a `200`, streamed. `path`/`len` name it for the body.
    File {
        path: std::path::PathBuf,
        len: u64,
        etag: String,
        mime: &'static str,
        /// The `Content-Encoding` token when a precompressed variant is served
        /// (`"br"`/`"gzip"`); `None` for the identity file.
        encoding: Option<&'static str>,
        /// Emit `Vary: Accept-Encoding` — set when the resource was negotiated,
        /// even on the identity fallback, so a cache never serves a `.br` body
        /// to a client that cannot decode it.
        vary_encoding: bool,
    },
}

impl StaticOutcome {
    fn into_response(self) -> Response {
        match self {
            StaticOutcome::NotModified { etag, vary_encoding } => {
                let mut r = Response::new(StatusCode::NOT_MODIFIED);
                r.headers.insert("etag", &etag);
                r.headers.insert("cache-control", &"no-cache".to_string());
                if vary_encoding {
                    r.headers.insert("vary", &"Accept-Encoding".to_string());
                }
                r
            }
            StaticOutcome::File { path, len, etag, mime, encoding, vary_encoding } => {
                // `Body::File`: the response names the file; the connection task
                // streams it in bounded chunks off the blocking pool, so a large
                // download is never read whole into memory.
                let mut r = Response::new(StatusCode::OK).content_type(mime).file(path, len);
                r.headers.insert("etag", &etag);
                // `no-cache` means "cache, but revalidate every time" — the browser
                // re-asks with If-None-Match and gets a 304 when nothing changed.
                r.headers.insert("cache-control", &"no-cache".to_string());
                // The extension-derived type is authoritative; stop the browser sniffing.
                r.headers.insert("x-content-type-options", &"nosniff".to_string());
                // Advertise range support so clients (video players, resumers) ask.
                r.headers.insert("accept-ranges", &"bytes".to_string());
                // A precompressed variant: the body is `.br`/`.gz` bytes, but the
                // Content-Type stayed the *original* type — the client decodes,
                // then interprets. `Content-Encoding` tells it how.
                if let Some(enc) = encoding {
                    r.headers.insert("content-encoding", &enc.to_string());
                }
                if vary_encoding {
                    r.headers.insert("vary", &"Accept-Encoding".to_string());
                }
                r
            }
        }
    }
}

/// Resolve, verify, and stat a static file — but do **not** read it. Runs on the
/// blocking pool. `None` means "not served here" — the caller falls through to
/// the router, which 404s.
///
/// The symlink re-check is here rather than in `kernway-static` because it needs
/// I/O: `resolve` guarantees *lexical* containment, but a file inside the root
/// can be a symlink pointing outside it, and only `canonicalize` sees that.
fn load_static(
    root: &std::path::Path,
    path: std::path::PathBuf,
    if_none_match: Option<&str>,
    accept_encoding: Option<&str>,
) -> Option<StaticOutcome> {
    // Resolve symlinks and `.`/`..` for real, then require the result to stay
    // under the canonical root. A file that links outside the root fails here —
    // this is the defence lexical checks cannot provide. `canonicalize` also
    // errors for a missing file, which is simply a miss.
    let canon = std::fs::canonicalize(&path).ok()?;
    let canon_root = std::fs::canonicalize(root).ok()?;
    if !canon.starts_with(&canon_root) {
        return None;
    }

    let meta = std::fs::metadata(&canon).ok()?;
    if !meta.is_file() {
        return None;
    }

    // The Content-Type is always the *original* file's — a `.br` variant of
    // `app.js` is still JavaScript once decoded.
    let mime = mime_for(&path);

    // Content negotiation. Only attempted for a compressible type, and only when
    // the caller enabled precompression (so `accept_encoding` is `None` and this
    // whole block is skipped on the default path). `vary_encoding` is set for
    // every compressible resource once negotiation is in play — including the
    // identity fallback below — so a shared cache keys on `Accept-Encoding`.
    let vary_encoding = accept_encoding.is_some() && kernway_static::is_compressible(mime);

    // Default to the identity file; a matching variant replaces it.
    let mut served = canon.clone();
    let mut served_meta = meta;
    let mut encoding: Option<&'static str> = None;

    if vary_encoding {
        if let Some(ae) = accept_encoding {
            for enc in kernway_static::accepted_encodings(ae) {
                let variant = with_suffix(&canon, enc.extension());
                // The variant gets the same symlink re-check as the original: a
                // `.br` that links outside the root must not escape it either.
                let Ok(vc) = std::fs::canonicalize(&variant) else { continue };
                if !vc.starts_with(&canon_root) {
                    continue;
                }
                let Ok(vm) = std::fs::metadata(&vc) else { continue };
                if vm.is_file() {
                    served = vc;
                    served_meta = vm;
                    encoding = Some(enc.token());
                    break;
                }
            }
        }
    }

    // The validator comes from the file *actually served*, so `br`, `gz`, and the
    // identity each get a distinct ETag — a cache cannot confuse one for another.
    let mtime_nanos = served_meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let etag = kernway_static::etag(served_meta.len(), mtime_nanos);

    // Conditional request: if the client's validator still matches, answer 304
    // and stream nothing — the whole point of caching.
    if let Some(inm) = if_none_match {
        if kernway_static::etag_matches(inm, &etag) {
            return Some(StaticOutcome::NotModified { etag, vary_encoding });
        }
    }

    Some(StaticOutcome::File {
        path: served,
        len: served_meta.len(),
        etag,
        mime,
        encoding,
        vary_encoding,
    })
}

/// Append a suffix to a path's filename: `/root/app.js` + `.br` → `/root/app.js.br`.
/// `PathBuf::set_extension`/`push` would replace or nest, not append, so this
/// works on the raw `OsString`.
fn with_suffix(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    std::path::PathBuf::from(s)
}

/// Try to answer a request from the static file root.
///
/// Returns `Some` only for a GET the router does not claim, whose path resolves
/// to a readable file under the root. Everything else — no root, a non-GET
/// method, a route that owns the path, a rejected or escaping path, a missing
/// file — is `None`, and the caller falls through to the router.
///
/// All I/O (canonicalize, stat, read) runs on the blocking pool via
/// [`rt_core::spawn_blocking`], so it never stalls the shard ([KEP-0000 §4]).
/// Handles GET and HEAD, conditional (`If-None-Match` → 304), and a single byte
/// `Range` (→ 206, or 416 if unsatisfiable). The HEAD/body distinction is the
/// caller's — [`write_response`] writes head-only for a HEAD.
///
/// [KEP-0000 §4]: https://github.com/tacpham/kernway/blob/main/docs/kep/0000-principles.md
async fn try_static(
    static_files: Option<&StaticFiles>,
    router: &Router,
    request: &Request,
) -> Option<Response> {
    let sf = static_files?;

    // GET and HEAD are served from the filesystem; other methods fall through.
    let method = &request.method;
    if !method.eq_ignore_ascii_case("GET") && !method.eq_ignore_ascii_case("HEAD") {
        return None;
    }
    // The router wins: a dynamic route for this path takes precedence over a
    // file that happens to share it.
    if router.find(method, &request.path).is_some() {
        return None;
    }
    // Reject hostile or malformed paths before any I/O. A rejection is a miss —
    // indistinguishable from "no such file", so a 404 either way reveals nothing.
    let path = sf.resolve(&request.path).ok()?;
    let root = sf.root().to_path_buf();
    let if_none_match = request.headers.get("if-none-match").map(str::to_string);
    // Negotiate only when this root serves precompressed variants — otherwise
    // `load_static` gets `None` and skips the extra `stat`s entirely. When it is
    // on, pass `Some` even if the request sent no `Accept-Encoding` (as `""`):
    // that still marks the resource negotiated, so its response carries a
    // consistent `Vary: Accept-Encoding` and a shared cache keys on the encoding.
    let accept_encoding = sf
        .serves_precompressed()
        .then(|| request.headers.get("accept-encoding").unwrap_or("").to_string());

    // `spawn_blocking` yields `None` if the closure panicked; `load_static`
    // yields `None` for any miss. Both fall through to the router.
    let outcome = rt_core::spawn_blocking(move || {
        load_static(&root, path, if_none_match.as_deref(), accept_encoding.as_deref())
    })
    .await??;

    let mut response = outcome.into_response();

    // A byte range applies only to a 200 file body. Capture the length first so
    // the borrow of `response.body` ends before the mutation.
    let file_len = if let Body::File { len, .. } = &response.body {
        Some(*len)
    } else {
        None
    };
    if let (Some(len), Some(range_header)) = (file_len, request.headers.get("range")) {
        apply_range(&mut response, range_header, len);
    }

    Some(response)
}

/// A parsed single byte range against a resource of known length.
enum RangeSpec {
    /// No usable range — serve the full `200` (bad syntax, or multi-range, which
    /// this cut answers with the whole body as RFC 9110 §14.2 permits).
    Full,
    /// Send `[start, end)` — half-open. `206`.
    Satisfiable { start: u64, end: u64 },
    /// The range lies outside the resource — `416`.
    Unsatisfiable,
}

/// Parse a single `Range: bytes=…` value against a resource of length `len`.
///
/// Supports `bytes=start-end`, `bytes=start-` (to the end), and `bytes=-suffix`
/// (the last N bytes). A missing `bytes=` unit, malformed digits, or a
/// comma-separated multi-range all return [`RangeSpec::Full`] — the server then
/// sends the whole body, which the spec allows. A start past the end is
/// [`RangeSpec::Unsatisfiable`].
fn parse_range(header: &str, len: u64) -> RangeSpec {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return RangeSpec::Full;
    };
    let spec = spec.trim();
    if spec.contains(',') {
        return RangeSpec::Full; // multi-range: single-range only in this cut
    }
    let Some((a, b)) = spec.split_once('-') else {
        return RangeSpec::Full;
    };

    let (start, end_incl) = if a.is_empty() {
        // Suffix range: the last `n` bytes.
        let Ok(n) = b.parse::<u64>() else { return RangeSpec::Full };
        if n == 0 {
            return RangeSpec::Unsatisfiable;
        }
        if n >= len {
            (0, len.saturating_sub(1))
        } else {
            (len - n, len - 1)
        }
    } else {
        let Ok(start) = a.parse::<u64>() else { return RangeSpec::Full };
        let end_incl = if b.is_empty() {
            len.saturating_sub(1)
        } else {
            let Ok(end) = b.parse::<u64>() else { return RangeSpec::Full };
            end.min(len.saturating_sub(1))
        };
        (start, end_incl)
    };

    if len == 0 || start >= len || start > end_incl {
        return RangeSpec::Unsatisfiable;
    }
    RangeSpec::Satisfiable { start, end: end_incl + 1 }
}

/// Turn a `200` file response into a `206` (or `416`) per the `Range` header.
fn apply_range(response: &mut Response, header: &str, len: u64) {
    match parse_range(header, len) {
        RangeSpec::Full => {} // leave the 200 as-is
        RangeSpec::Satisfiable { start, end } => {
            response.status = StatusCode::PARTIAL_CONTENT;
            response
                .headers
                .insert("content-range", &format!("bytes {}-{}/{}", start, end - 1, len));
            if let Body::File { range, .. } = &mut response.body {
                *range = Some((start, end));
            }
        }
        RangeSpec::Unsatisfiable => {
            response.status = StatusCode::RANGE_NOT_SATISFIABLE;
            response
                .headers
                .insert("content-range", &format!("bytes */{len}"));
            response.body = Body::Empty;
        }
    }
}

/// Run the async middleware chain and the matched route ([KEP-0006]), with panic
/// isolation: a handler (or middleware) that panics becomes a `500`, so one bad
/// request cannot take down the connection task or the requests sharing it.
async fn handle(
    request: Request,
    router: &Router,
    context: &AppContext,
    middlewares: &[Arc<dyn Middleware>],
) -> Response {
    // Build and run the whole chain *inside* the wrapped future, so a panic caught
    // by `CatchUnwind` covers not just an `.await` deep in a handler but also the
    // eager work of dispatch itself (scope, terminal, `Next::run`).
    let inner: BoxFuture<'_, Response> = Box::pin(async move {
        // One request scope per request (KEP-0005), over the application context.
        // Middleware may set request-scoped beans on it; the handler resolves them.
        let scope = RequestScope::new(context);
        let terminal = move |req: Request, scope: &RequestScope| run_handler(req, router, scope);
        Next { rest: middlewares, terminal: &terminal }.run(request, &scope).await
    });
    // A custom panic response, if the app registered one (`on_panic`).
    let on_panic = context.get::<PanicHandler>().ok();
    CatchUnwind { inner, on_panic }.await
}

/// A caller-supplied response for a caught panic — set with
/// [`AppBuilder::on_panic`], registered as a bean so the dispatch can find it.
pub(crate) struct PanicHandler(pub(crate) Box<dyn Fn(&str) -> Response + Send + Sync>);

/// Wraps the request future so a panic on any poll is caught and turned into a
/// `500` — the async equivalent of the sync path's per-request `catch_unwind`
/// (the KEP-0002/KEP-0006 follow-on). Universal: every request goes through it,
/// independent of which middleware are installed. A caller's `on_panic` handler
/// customises the response; otherwise a default RFC 7807 `500`.
struct CatchUnwind<'a> {
    inner: BoxFuture<'a, Response>,
    on_panic: Option<Arc<PanicHandler>>,
}

impl Future for CatchUnwind<'_> {
    type Output = Response;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<Response> {
        let this = self.as_mut().get_mut(); // `inner` is a `Pin<Box<…>>`, so `Unpin`.
        // `AssertUnwindSafe`: on a caught panic we return a 500 and drop the future,
        // never re-polling it, so no observer sees a broken half-state.
        match catch_unwind(AssertUnwindSafe(|| this.inner.as_mut().poll(cx))) {
            Ok(poll) => poll,
            Err(payload) => {
                let message = panic_message(payload.as_ref());
                kernway_log::error!(target: "kernway_server", "handler panicked: {message}");
                let response = match &this.on_panic {
                    Some(handler) => (handler.0)(&message),
                    None => panic_response(),
                };
                Poll::Ready(response)
            }
        }
    }
}

/// A best-effort message from a panic payload (`&str`/`String`, else a placeholder).
fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// The `500` a caught panic becomes — RFC 7807, no internal detail leaked to the client.
fn panic_response() -> Response {
    Response::new(StatusCode::INTERNAL_SERVER_ERROR)
        .content_type("application/json; charset=utf-8")
        .body(
            br#"{"status":500,"title":"Internal Server Error","detail":"the request handler failed"}"#.to_vec(),
        )
}

/// Wrap a synchronous test body as an async [`Handler`](crate::router::Handler).
#[cfg(test)]
fn sync_handler(
    f: impl Fn(Request, &RequestScope) -> Response + Send + Sync + 'static,
) -> crate::router::Handler {
    Arc::new(move |req, scope| {
        let resp = f(req, scope);
        Box::pin(async move { resp })
    })
}

/// Block on a future in a sync test.
#[cfg(test)]
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    rt_core::Executor::new().unwrap().block_on(f).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(path: &str) -> Request {
        Request::new("GET", path)
    }

    #[test]
    fn config_drives_the_address_and_registers_beans() {
        // A typed config bean (what #[configuration] generates by hand here).
        struct DbConfig {
            url: String,
        }
        impl FromConfig for DbConfig {
            fn from_config(config: &Config) -> Self {
                DbConfig { url: config.get_str("db.url").unwrap_or_default().to_string() }
            }
        }

        let config = Config::builder()
            .parse("server.port = 7654\ndb.url = postgres://localhost/app")
            .build();
        let app = KernwayApp::builder().config(config).configure::<DbConfig>().build();

        // No .bind() → the address came from server.port.
        assert_eq!(app.addr, "0.0.0.0:7654");
        // The Config itself is an injectable bean.
        assert_eq!(app.context.get::<Config>().unwrap().get_str("db.url"), Some("postgres://localhost/app"));
        // The typed config bean was built from the config and registered.
        assert_eq!(app.context.get::<DbConfig>().unwrap().url, "postgres://localhost/app");
    }

    #[test]
    fn an_explicit_bind_wins_over_config() {
        let config = Config::builder().parse("server.port = 1111").build();
        let app = KernwayApp::builder().bind("127.0.0.1:2222").config(config).build();
        assert_eq!(app.addr, "127.0.0.1:2222", "explicit bind is not overridden by config");
    }

    #[test]
    fn unmatched_route_is_a_404_with_a_json_body() {
        let router = Router::new();
        let ctx = AppContext::new();
        let response = block_on(handle(get("/nope"), &router, &ctx, &[]));
        assert_eq!(response.status, StatusCode::NOT_FOUND);
        assert!(String::from_utf8_lossy(response.body_bytes()).contains("no route for GET /nope"));
    }

    #[test]
    fn a_matched_route_runs_its_handler() {
        let mut router = Router::new();
        router.add("GET", "/ping", sync_handler(|_req, _ctx| {
            Response::new(StatusCode::OK).body(b"pong".to_vec())
        }));
        let response = block_on(handle(get("/ping"), &router, &AppContext::new(), &[]));
        assert_eq!(response.body_bytes(), b"pong");
    }

    #[test]
    fn path_params_reach_the_handler() {
        let mut router = Router::new();
        router.add("GET", "/users/{id}", sync_handler(|req: Request, _ctx: &RequestScope| {
            Response::new(StatusCode::OK).body(req.path_params["id"].clone().into_bytes())
        }));
        let response = block_on(handle(get("/users/42"), &router, &AppContext::new(), &[]));
        assert_eq!(response.body_bytes(), b"42");
    }

    #[test]
    fn a_panicking_handler_becomes_a_500() {
        let mut router = Router::new();
        router.add("GET", "/boom", sync_handler(|_req, _ctx| -> Response {
            panic!("handler exploded")
        }));
        let response = block_on(handle(get("/boom"), &router, &AppContext::new(), &[]));
        assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn role_allowed_reads_the_role_from_the_scope() {
        // What #[require_role] compiles to. No context (no auth) → not allowed.
        let app = AppContext::new();
        let scope = RequestScope::new(&app);
        assert!(!role_allowed(&scope, "ADMIN"), "no SecurityContext → denied");

        // An authenticated context with the role → allowed; another role → denied.
        scope.set(kernway_security::SecurityContext::authenticated("u", ["ADMIN"]));
        assert!(role_allowed(&scope, "ADMIN"));
        assert!(!role_allowed(&scope, "USER"));
    }

    #[test]
    fn on_panic_customises_the_panic_response() {
        let mut router = Router::new();
        router.add("GET", "/boom", sync_handler(|_req, _ctx| -> Response { panic!("kaboom") }));
        // Register a custom panic handler the way `build()` does from `on_panic`.
        let mut context = AppContext::new();
        context
            .register_instance::<PanicHandler>(Arc::new(PanicHandler(Box::new(|message: &str| {
                Response::new(StatusCode::SERVICE_UNAVAILABLE).body(format!("caught: {message}").into_bytes())
            }))))
            .unwrap();

        let response = block_on(handle(get("/boom"), &router, &context, &[]));
        assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE, "custom status used");
        assert!(
            String::from_utf8_lossy(response.body_bytes()).contains("caught: kaboom"),
            "custom body carries the panic message"
        );
    }

    #[test]
    fn middleware_wraps_the_handler() {
        struct Tag;
        impl Middleware for Tag {
            fn name(&self) -> &'static str { "Tag" }
            fn handle<'a>(&'a self, req: Request, scope: &'a RequestScope, next: Next<'a>) -> BoxFuture<'a, Response> {
                Box::pin(async move {
                    let mut resp = next.run(req, scope).await;
                    resp.headers.insert("x-tag", "seen");
                    resp
                })
            }
        }

        let mut router = Router::new();
        router.add("GET", "/x", sync_handler(|_req, _ctx| Response::new(StatusCode::OK)));
        let layers: Vec<Arc<dyn Middleware>> = vec![Arc::new(Tag)];
        let response = block_on(handle(get("/x"), &router, &AppContext::new(), &layers));
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
            serve_connection(stream, router, context, middlewares, None, keep_alive, FILE_CHUNK, Arc::new(UploadConfig::default()), Shutdown::new()).await;
        })
        .unwrap();

        client.join().unwrap()
    }

    #[test]
    fn serves_a_real_http_request_over_the_async_transport() {
        let mut router = Router::new();
        router.add("GET", "/hello", sync_handler(|_req, _ctx| {
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
        router.add("POST", "/echo", sync_handler(|req: Request, _ctx: &RequestScope| {
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

    /// Serve one connection with owned request bytes and a custom upload config.
    fn serve_upload(router: Router, request: Vec<u8>, upload: UploadConfig) -> String {
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
            sock.write_all(&request).unwrap();
            let mut got = Vec::new();
            sock.read_to_end(&mut got).unwrap();
            String::from_utf8_lossy(&got).into_owned()
        });
        let router = Arc::new(router);
        let context = Arc::new(AppContext::new());
        let middlewares: Arc<Vec<Arc<dyn Middleware>>> = Arc::new(Vec::new());
        ex.block_on(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_connection(stream, router, context, middlewares, None, KeepAliveConfig::default(), FILE_CHUNK, Arc::new(upload), Shutdown::new()).await;
        })
        .unwrap();
        client.join().unwrap()
    }

    #[test]
    fn a_large_body_spools_to_disk_not_memory() {
        // Threshold 1 KiB → a 100 KiB body streams to a temp file. The handler reports
        // what it saw: an empty in-memory body, and a spool file holding every byte.
        let mut router = Router::new();
        router.add("POST", "/up", sync_handler(|req: Request, _ctx: &RequestScope| {
            let on_disk = req.body_spool.as_ref().map(|s| (s.len, std::fs::read(&s.path).map(|v| v.len()).unwrap_or(0)));
            Response::new(StatusCode::OK).body(format!("body={} spool={on_disk:?}", req.body.len()).into_bytes())
        }));

        let payload = vec![b'm'; 100 * 1024];
        let mut request = format!("POST /up HTTP/1.1\r\nHost: x\r\ncontent-length: {}\r\n\r\n", payload.len()).into_bytes();
        request.extend_from_slice(&payload);

        let upload = UploadConfig { max_inmemory_body: 1024, ..UploadConfig::default() };
        let got = serve_upload(router, request, upload);
        assert!(
            got.ends_with(&format!("body=0 spool=Some(({}, {}))", payload.len(), payload.len())),
            "large body must be spooled whole to disk with an empty in-memory body: {got:?}"
        );
    }

    #[test]
    fn a_small_body_stays_in_memory() {
        // Under the threshold → the existing fast path: in `req.body`, no spool.
        let mut router = Router::new();
        router.add("POST", "/up", sync_handler(|req: Request, _ctx: &RequestScope| {
            Response::new(StatusCode::OK).body(format!("body={} spool={}", req.body.len(), req.body_spool.is_some()).into_bytes())
        }));
        let request = b"POST /up HTTP/1.1\r\nHost: x\r\ncontent-length: 5\r\n\r\nhello".to_vec();
        let upload = UploadConfig { max_inmemory_body: 1024, ..UploadConfig::default() };
        let got = serve_upload(router, request, upload);
        assert!(got.ends_with("body=5 spool=false"), "small body stays in memory: {got:?}");
    }

    #[test]
    fn an_upload_over_the_ceiling_is_refused_with_413() {
        let payload = vec![b'x'; 5000];
        let mut request = format!("POST /up HTTP/1.1\r\nHost: x\r\ncontent-length: {}\r\n\r\n", payload.len()).into_bytes();
        request.extend_from_slice(&payload);
        // Ceiling 1000 < 5000 → 413 before any body is spooled.
        let upload = UploadConfig { max_inmemory_body: 100, max_upload_size: 1000, ..UploadConfig::default() };
        let got = serve_upload(Router::new(), request, upload);
        assert!(got.starts_with("HTTP/1.1 413"), "over-limit upload must be refused: {got:?}");
    }

    #[test]
    fn upload_file_persist_moves_the_spooled_file() {
        let dir = std::env::temp_dir();
        let src = dir.join(format!("kernway-test-src-{}.tmp", std::process::id()));
        let dst = dir.join(format!("kernway-test-dst-{}.tmp", std::process::id()));
        std::fs::write(&src, b"song bytes").unwrap();

        let mut req = Request::new("POST", "/up");
        req.body_spool = Some(kernway_core::request::SpooledBody { path: src.clone(), len: 10 });
        let upload = crate::upload::UploadFile::from_request(&req).unwrap();
        assert_eq!(upload.len(), 10);

        let dst_move = dst.clone();
        rt_core::Executor::new()
            .unwrap()
            .block_on(async move { upload.persist(dst_move).await.unwrap() })
            .unwrap();

        assert_eq!(std::fs::read(&dst).unwrap(), b"song bytes", "content moved intact");
        assert!(!src.exists(), "source temp file was moved away");
        std::fs::remove_file(&dst).ok();
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
        router.add("GET", "/n", sync_handler(|_req, _ctx| {
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
            serve_connection(stream, router, context, middlewares, None, keep_alive, FILE_CHUNK, Arc::new(UploadConfig::default()), shutdown).await;
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
            kernway_http::Parsed::Complete { request, .. } => assert_eq!(request.version, HttpVersion::Http10),
            kernway_http::Parsed::Incomplete => panic!("expected a complete request"),
        }
    }
}

#[cfg(test)]
mod static_file_tests {
    use super::*;
    use std::fs;

    /// A fresh, empty temp directory unique to this test and process.
    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("kw-static-{}-{}-{}", tag, std::process::id(), line!()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn serves_a_file_with_an_etag_and_mime() {
        let root = tmpdir("serve");
        fs::write(root.join("a.txt"), b"hello").unwrap();

        match load_static(&root, root.join("a.txt"), None, None).expect("should serve") {
            StaticOutcome::File { path, len, etag, mime, encoding, .. } => {
                // The file is named, not read — verify by size and by reading it here.
                assert_eq!(len, 5);
                assert_eq!(std::fs::read(&path).unwrap(), b"hello");
                assert!(etag.starts_with('"') && etag.ends_with('"'), "etag quoted: {etag}");
                assert_eq!(mime, "text/plain; charset=utf-8");
                assert_eq!(encoding, None, "identity file carries no Content-Encoding");
            }
            StaticOutcome::NotModified { .. } => panic!("expected a file, got 304"),
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_matching_etag_yields_304_without_rereading() {
        let root = tmpdir("cond");
        fs::write(root.join("a.txt"), b"hello").unwrap();

        let etag = match load_static(&root, root.join("a.txt"), None, None).unwrap() {
            StaticOutcome::File { etag, .. } => etag,
            StaticOutcome::NotModified { .. } => unreachable!(),
        };
        match load_static(&root, root.join("a.txt"), Some(&etag), None).unwrap() {
            StaticOutcome::NotModified { etag: e, .. } => assert_eq!(e, etag),
            StaticOutcome::File { .. } => panic!("a current cache should get 304, not the body"),
        }
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_stale_etag_serves_the_body() {
        let root = tmpdir("stale");
        fs::write(root.join("a.txt"), b"hello").unwrap();
        assert!(matches!(
            load_static(&root, root.join("a.txt"), Some("\"stale-0\""), None).unwrap(),
            StaticOutcome::File { .. }
        ));
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_missing_file_is_a_miss() {
        let root = tmpdir("missing");
        assert!(load_static(&root, root.join("nope.txt"), None, None).is_none());
        fs::remove_dir_all(&root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_escaping_the_root_is_rejected() {
        // The lexical check in `resolve` cannot see this: the path is a plain
        // name under the root, but the file it names links outside. Only the
        // canonicalize-and-recheck in `load_static` catches it.
        let root = tmpdir("symlink");
        let outside = tmpdir("symlink-out");
        fs::write(outside.join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("leak.txt")).unwrap();

        assert!(
            load_static(&root, root.join("leak.txt"), None, None).is_none(),
            "a symlink pointing outside the root must not be served, even though its target exists"
        );

        fs::remove_dir_all(&root).ok();
        fs::remove_dir_all(&outside).ok();
    }

    // --- precompressed variants -------------------------------------------

    /// The `.br` is preferred over `.gz`, its bytes are what gets served, the
    /// Content-Type stays the original, and the ETag is the variant's own.
    #[test]
    fn a_brotli_variant_is_preferred_and_served_with_the_original_type() {
        let root = tmpdir("precomp-br");
        fs::write(root.join("app.js"), b"the original javascript source").unwrap();
        fs::write(root.join("app.js.gz"), b"GZIPPED").unwrap();
        fs::write(root.join("app.js.br"), b"BROTLI").unwrap();

        match load_static(&root, root.join("app.js"), None, Some("br, gzip")).unwrap() {
            StaticOutcome::File { path, len, mime, encoding, vary_encoding, etag } => {
                assert_eq!(std::fs::read(&path).unwrap(), b"BROTLI", "the .br bytes are served");
                assert_eq!(len, 6);
                assert_eq!(mime, "text/javascript; charset=utf-8", "type is the original's");
                assert_eq!(encoding, Some("br"));
                assert!(vary_encoding);
                // ETag is the variant's (len 6), not the identity's (len 30).
                assert!(etag.starts_with("\"6-"), "etag from the served file: {etag}");
            }
            StaticOutcome::NotModified { .. } => panic!("expected the file"),
        }
        fs::remove_dir_all(&root).ok();
    }

    /// Only gzip on offer, or only gzip accepted → the `.gz` is served.
    #[test]
    fn gzip_is_served_when_brotli_is_absent_or_unaccepted() {
        let root = tmpdir("precomp-gz");
        fs::write(root.join("app.css"), b"body{}").unwrap();
        fs::write(root.join("app.css.gz"), b"GZ").unwrap();
        fs::write(root.join("app.css.br"), b"BR").unwrap();

        // Client refuses brotli.
        match load_static(&root, root.join("app.css"), None, Some("br;q=0, gzip")).unwrap() {
            StaticOutcome::File { encoding, path, .. } => {
                assert_eq!(encoding, Some("gzip"));
                assert_eq!(std::fs::read(&path).unwrap(), b"GZ");
            }
            StaticOutcome::NotModified { .. } => panic!("expected the file"),
        }
        fs::remove_dir_all(&root).ok();
    }

    /// No variant on disk → the identity file, but still `Vary: Accept-Encoding`
    /// so a cache does not later feed a `.br` to a client that cannot decode it.
    #[test]
    fn identity_is_served_when_no_variant_exists_but_still_varies() {
        let root = tmpdir("precomp-none");
        fs::write(root.join("app.js"), b"source").unwrap();

        match load_static(&root, root.join("app.js"), None, Some("br, gzip")).unwrap() {
            StaticOutcome::File { encoding, vary_encoding, len, .. } => {
                assert_eq!(encoding, None, "no variant → identity");
                assert_eq!(len, 6);
                assert!(vary_encoding, "a negotiated resource varies even on the fallback");
            }
            StaticOutcome::NotModified { .. } => panic!("expected the file"),
        }
        fs::remove_dir_all(&root).ok();
    }

    /// A binary type is never probed for a variant — no `.stat`, no `Vary` — even
    /// if a stray `.gz` sits next to it.
    #[test]
    fn already_compressed_media_is_not_negotiated() {
        let root = tmpdir("precomp-bin");
        fs::write(root.join("logo.png"), b"\x89PNG....").unwrap();
        fs::write(root.join("logo.png.gz"), b"NOPE").unwrap();

        match load_static(&root, root.join("logo.png"), None, Some("gzip")).unwrap() {
            StaticOutcome::File { encoding, vary_encoding, path, .. } => {
                assert_eq!(encoding, None, "png is already compressed — serve it as-is");
                assert!(!vary_encoding);
                assert_eq!(std::fs::read(&path).unwrap(), b"\x89PNG....");
            }
            StaticOutcome::NotModified { .. } => panic!("expected the file"),
        }
        fs::remove_dir_all(&root).ok();
    }

    /// A negotiated root with a client that sent no `Accept-Encoding` (passed as
    /// `Some("")`) still serves identity *and* marks `Vary` — so the same URL
    /// answers with a consistent `Vary` whether or not the header was present.
    #[test]
    fn an_empty_accept_encoding_still_varies_on_a_negotiated_root() {
        let root = tmpdir("precomp-empty");
        fs::write(root.join("app.js"), b"source").unwrap();
        fs::write(root.join("app.js.br"), b"BR").unwrap();

        match load_static(&root, root.join("app.js"), None, Some("")).unwrap() {
            StaticOutcome::File { encoding, vary_encoding, .. } => {
                assert_eq!(encoding, None, "no encoding accepted → identity");
                assert!(vary_encoding, "still Vary, so a cache keys on Accept-Encoding");
            }
            StaticOutcome::NotModified { .. } => panic!("expected the file"),
        }
        fs::remove_dir_all(&root).ok();
    }

    /// Precompression off (`accept_encoding = None`) → the old behaviour exactly,
    /// no probing regardless of what sits on disk.
    #[test]
    fn variants_are_ignored_when_precompression_is_off() {
        let root = tmpdir("precomp-off");
        fs::write(root.join("app.js"), b"source").unwrap();
        fs::write(root.join("app.js.br"), b"BROTLI").unwrap();

        match load_static(&root, root.join("app.js"), None, None).unwrap() {
            StaticOutcome::File { encoding, vary_encoding, path, .. } => {
                assert_eq!(encoding, None);
                assert!(!vary_encoding);
                assert_eq!(std::fs::read(&path).unwrap(), b"source", "identity, not the .br");
            }
            StaticOutcome::NotModified { .. } => panic!("expected the file"),
        }
        fs::remove_dir_all(&root).ok();
    }

    // --- the real thing: static files over an actual socket ----------------

    /// Serve one connection with a static root over a real TCP socket, and
    /// return exactly what the client received. This exercises the whole path a
    /// deployed server runs — accept, parse, resolve, stat, read, encode, write
    /// — not just `load_static` in isolation. Keep-alive off, so the client
    /// reads to EOF.
    fn serve_static(root: std::path::PathBuf, request: String) -> String {
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
            sock.write_all(request.as_bytes()).unwrap();
            let mut got = String::new();
            sock.read_to_string(&mut got).unwrap();
            got
        });

        let router = Arc::new(Router::new());
        let context = Arc::new(AppContext::new());
        let middlewares: Arc<Vec<Arc<dyn Middleware>>> = Arc::new(Vec::new());
        let static_files = Some(Arc::new(StaticFiles::new(root)));
        let keep_alive = KeepAliveConfig { enabled: false, ..Default::default() };
        ex.block_on(async move {
            let (stream, _) = listener.accept().await.unwrap();
            serve_connection(stream, router, context, middlewares, static_files, keep_alive, FILE_CHUNK, Arc::new(UploadConfig::default()), Shutdown::new()).await;
        })
        .unwrap();

        client.join().unwrap()
    }

    /// The `etag:` value from a raw response, quotes included.
    fn etag_of(response: &str) -> String {
        response
            .lines()
            .find(|l| l.to_ascii_lowercase().starts_with("etag:"))
            .and_then(|l| l.splitn(2, ':').nth(1))
            .map(|v| v.trim().to_string())
            .expect("response has an etag")
    }

    #[test]
    fn serves_a_real_file_over_http() {
        let root = tmpdir("http-serve");
        fs::write(root.join("index.html"), b"<h1>hi</h1>").unwrap();

        let got = serve_static(root.clone(), "GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_string());
        assert!(got.starts_with("HTTP/1.1 200 OK\r\n"), "got {got:?}");
        assert!(got.contains("content-type: text/html"), "got {got:?}");
        assert!(got.to_ascii_lowercase().contains("etag:"), "got {got:?}");
        assert!(got.contains("x-content-type-options: nosniff"), "got {got:?}");
        assert!(got.ends_with("<h1>hi</h1>"), "got {got:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn streams_a_large_file_in_chunks_over_http() {
        // Larger than FILE_CHUNK (256 KiB), so the streaming loop runs several
        // read-a-chunk/write-a-chunk iterations — the path a real download takes,
        // and the reason Body::File exists (never the whole file in memory).
        let root = tmpdir("large");
        let content = "x".repeat(700_000); // ~2.7 chunks at the 256 KiB default
        fs::write(root.join("big.txt"), &content).unwrap();

        let got = serve_static(root.clone(), "GET /big.txt HTTP/1.1\r\nHost: x\r\n\r\n".to_string());
        assert!(got.starts_with("HTTP/1.1 200 OK\r\n"), "got head");
        assert!(got.contains("content-length: 700000"), "content-length must be the file size");
        assert!(
            got.ends_with(&content),
            "the whole file must arrive intact across chunk boundaries (got {} body bytes)",
            got.len() - got.find("\r\n\r\n").map_or(0, |i| i + 4)
        );

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn head_returns_the_length_without_a_body_over_http() {
        let root = tmpdir("head");
        fs::write(root.join("a.txt"), b"hello").unwrap();

        let got = serve_static(root.clone(), "HEAD /a.txt HTTP/1.1\r\nHost: x\r\n\r\n".to_string());
        assert!(got.starts_with("HTTP/1.1 200 OK\r\n"), "got {got:?}");
        assert!(got.contains("content-length: 5"), "HEAD must send the length: {got:?}");
        assert!(got.contains("accept-ranges: bytes"), "and advertise ranges: {got:?}");
        // Head only — the response ends at the blank line, no "hello".
        assert!(got.ends_with("\r\n\r\n"), "HEAD must send no body: {got:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_byte_range_returns_206_over_http() {
        let root = tmpdir("range");
        fs::write(root.join("f.txt"), b"0123456789abcdef").unwrap(); // 16 bytes

        let got = serve_static(
            root.clone(),
            "GET /f.txt HTTP/1.1\r\nHost: x\r\nRange: bytes=4-7\r\n\r\n".to_string(),
        );
        assert!(got.starts_with("HTTP/1.1 206 Partial Content\r\n"), "got {got:?}");
        assert!(got.contains("content-range: bytes 4-7/16"), "got {got:?}");
        assert!(got.contains("content-length: 4"), "got {got:?}");
        assert!(got.ends_with("4567"), "the 4 bytes [4,8) must be sent: {got:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_unsatisfiable_range_returns_416_over_http() {
        let root = tmpdir("range416");
        fs::write(root.join("f.txt"), b"short").unwrap(); // 5 bytes

        let got = serve_static(
            root.clone(),
            "GET /f.txt HTTP/1.1\r\nHost: x\r\nRange: bytes=100-200\r\n\r\n".to_string(),
        );
        assert!(got.starts_with("HTTP/1.1 416 Range Not Satisfiable\r\n"), "got {got:?}");
        assert!(got.contains("content-range: bytes */5"), "must state the real length: {got:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parse_range_cases() {
        use super::{parse_range, RangeSpec};
        let len = 100;
        assert!(matches!(parse_range("bytes=0-9", len), RangeSpec::Satisfiable { start: 0, end: 10 }));
        assert!(matches!(parse_range("bytes=90-", len), RangeSpec::Satisfiable { start: 90, end: 100 }));
        assert!(matches!(parse_range("bytes=-10", len), RangeSpec::Satisfiable { start: 90, end: 100 }));
        // end past the resource is clamped to the last byte.
        assert!(matches!(parse_range("bytes=50-999", len), RangeSpec::Satisfiable { start: 50, end: 100 }));
        // start past the end → unsatisfiable.
        assert!(matches!(parse_range("bytes=100-200", len), RangeSpec::Unsatisfiable));
        // bad syntax / multi-range / wrong unit → serve full.
        assert!(matches!(parse_range("bytes=abc", len), RangeSpec::Full));
        assert!(matches!(parse_range("bytes=0-9,20-29", len), RangeSpec::Full));
        assert!(matches!(parse_range("items=0-9", len), RangeSpec::Full));
    }

    #[test]
    fn a_conditional_request_gets_304_over_http() {
        let root = tmpdir("http-304");
        fs::write(root.join("index.html"), b"the page").unwrap();

        let first = serve_static(root.clone(), "GET / HTTP/1.1\r\nHost: x\r\n\r\n".to_string());
        let etag = etag_of(&first);

        let second = serve_static(
            root.clone(),
            format!("GET / HTTP/1.1\r\nHost: x\r\nIf-None-Match: {etag}\r\n\r\n"),
        );
        assert!(second.starts_with("HTTP/1.1 304 Not Modified\r\n"), "got {second:?}");
        // 304 carries no body — the client's cached copy is current.
        assert!(!second.ends_with("the page"), "304 must not send the body: {second:?}");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_traversal_attempt_over_http_is_a_404() {
        let root = tmpdir("http-traversal");
        fs::write(root.join("index.html"), b"x").unwrap();

        // Raw, un-normalised path on the wire — the server, not a client library,
        // must reject it.
        let got = serve_static(
            root.clone(),
            "GET /%2e%2e/%2e%2e/etc/passwd HTTP/1.1\r\nHost: x\r\n\r\n".to_string(),
        );
        assert!(got.starts_with("HTTP/1.1 404"), "got {got:?}");

        fs::remove_dir_all(&root).ok();
    }
}
