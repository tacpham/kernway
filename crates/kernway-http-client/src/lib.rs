//! # kernway-http-client
//!
//! The **outbound** half of HTTP: making requests *to* other services, on the same
//! Kernway async runtime as the server (no tokio). The server (`kernway-http`) reads a
//! request someone sent us and writes a response; this is the mirror image — it writes
//! a request to a remote host and reads their response. A backend needs both: to call
//! an OAuth2 token endpoint, a payment or email API, or fetch JWKS for RS256.
//!
//! ```rust,ignore
//! let client = HttpClient::new();
//! let resp = client.get("http://example.com/status").await?;
//! assert_eq!(resp.status, 200);
//! let tokens = client.post_form("https://oauth2.example/token", &[("code", code)]).await?;
//! ```
//!
//! This first cut speaks plain HTTP/1.1 (parsing, `Content-Length`, chunked, read-to-
//! close). `https://` requires TLS, which lands in a follow-up on `rustls`; until then
//! an `https` URL returns [`HttpError::Tls`].

#![forbid(unsafe_code)]

mod compress;
mod cookie;
mod tls;

use std::collections::HashMap;
use std::fmt;
use std::io::ErrorKind;
use std::net::ToSocketAddrs;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use rt_net::AsyncTcpStream;

/// Default cap on establishing a connection (TCP connect + TLS handshake).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default cap on the whole request (connect through reading the full response).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default maximum number of redirects to follow before returning the 3xx response.
pub const DEFAULT_MAX_REDIRECTS: usize = 10;
/// Default number of idle keep-alive connections kept per origin.
pub const DEFAULT_MAX_IDLE_PER_HOST: usize = 8;
/// Drop a pooled connection idle longer than this (servers close idle keep-alives).
const POOL_IDLE_TIMEOUT_MS: u64 = 20_000;

/// A live connection — a plain socket, or a TLS session over one. Both expose the
/// same `read`/`write_all`, so the HTTP layer is oblivious to which it holds.
enum Conn {
    Plain(AsyncTcpStream),
    Tls(Box<tls::AsyncTlsStream>),
}

impl Conn {
    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf).await,
            Conn::Tls(s) => s.read(buf).await,
        }
    }

    async fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Conn::Plain(s) => s.write_all(buf).await,
            Conn::Tls(s) => s.write_all(buf).await,
        }
    }
}

/// The pooling key: one bucket of idle connections per `(scheme, host, port)`.
type Origin = (String, String, u16);

fn origin_of(url: &Url) -> Origin {
    (url.scheme.clone(), url.host.to_ascii_lowercase(), url.port)
}

/// A pooled connection plus when it went idle (for staleness eviction).
struct Idle {
    conn: Conn,
    since_ms: u64,
}

/// A pool of idle keep-alive connections, keyed by origin, shared across cores. Like
/// [`kernway_redis::Pool`], the `Mutex` is held only to pop/push a connection, never
/// across a request's `.await`, so a slow request never blocks another core at the lock.
struct ConnPool {
    idle: Mutex<HashMap<Origin, Vec<Idle>>>,
    max_per_origin: usize,
}

impl ConnPool {
    fn new(max_per_origin: usize) -> Self {
        Self { idle: Mutex::new(HashMap::new()), max_per_origin }
    }

    /// Take a fresh-enough idle connection for `origin`, discarding any that have sat
    /// too long (a server would have closed them).
    fn take(&self, origin: &Origin, now_ms: u64) -> Option<Conn> {
        let mut map = self.idle.lock().unwrap();
        let bucket = map.get_mut(origin)?;
        while let Some(entry) = bucket.pop() {
            if now_ms.saturating_sub(entry.since_ms) < POOL_IDLE_TIMEOUT_MS {
                return Some(entry.conn);
            }
            // otherwise the connection is likely dead — drop it and try the next
        }
        None
    }

    /// Return a reusable connection to the pool (dropped if the bucket is full).
    fn put(&self, origin: Origin, conn: Conn, now_ms: u64) {
        if self.max_per_origin == 0 {
            return;
        }
        let mut map = self.idle.lock().unwrap();
        let bucket = map.entry(origin).or_default();
        if bucket.len() < self.max_per_origin {
            bucket.push(Idle { conn, since_ms: now_ms });
        }
    }
}

/// Unix milliseconds now (for pool idle timestamps).
fn now_ms() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// An HTTP method. `as_str` is the wire token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `PATCH`.
    Patch,
    /// `DELETE`.
    Delete,
    /// `HEAD`.
    Head,
}

impl Method {
    /// The uppercase wire token (`"GET"`, `"POST"`, …).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
            Method::Head => "HEAD",
        }
    }
}

/// A parsed absolute URL — enough of one to open a connection and write a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    /// `http` or `https`.
    pub scheme: String,
    /// The host (no port).
    pub host: String,
    /// The port (defaulted from the scheme if absent).
    pub port: u16,
    /// The path plus query, always starting with `/` (`"/"` if the URL had none).
    pub path_and_query: String,
}

impl Url {
    /// Parse an absolute `http`/`https` URL. Rejects anything without a scheme + host.
    pub fn parse(url: &str) -> Result<Url, HttpError> {
        let (scheme, rest) = url.split_once("://").ok_or_else(|| HttpError::Url(format!("no scheme in {url:?}")))?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(HttpError::Url(format!("unsupported scheme {scheme:?}")));
        }
        // Split the authority from the path at the first '/'.
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        if authority.is_empty() {
            return Err(HttpError::Url(format!("no host in {url:?}")));
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => {
                let port = p.parse::<u16>().map_err(|_| HttpError::Url(format!("bad port in {url:?}")))?;
                (h.to_string(), port)
            }
            None => (authority.to_string(), if scheme == "https" { 443 } else { 80 }),
        };
        Ok(Url { scheme, host, port, path_and_query: if path.is_empty() { "/".into() } else { path.to_string() } })
    }

    /// Whether this URL uses TLS.
    #[must_use]
    pub fn is_tls(&self) -> bool {
        self.scheme == "https"
    }
}

/// An outbound request.
pub struct Request {
    /// The method.
    pub method: Method,
    /// The target URL.
    pub url: Url,
    /// Extra headers (Host/Content-Length/Connection are added automatically).
    pub headers: Vec<(String, String)>,
    /// The request body (empty for a GET).
    pub body: Vec<u8>,
}

impl Request {
    /// A request with no extra headers or body.
    pub fn new(method: Method, url: Url) -> Self {
        Self { method, url, headers: Vec::new(), body: Vec::new() }
    }

    /// Add a header.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Set the body and its `Content-Type`.
    #[must_use]
    pub fn body(mut self, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self.header("content-type", content_type)
    }
}

/// A response from a remote server.
///
/// Headers are stored **without per-header allocation**: the raw header block is owned
/// once, and each header is a pair of byte ranges into it, decoded to `&str` only when
/// read via [`header`](Self::header)/[`headers`](Self::headers). Names keep their
/// on-wire case; lookup is case-insensitive.
#[derive(Clone)]
pub struct Response {
    /// The HTTP status code.
    pub status: u16,
    /// The response body.
    pub body: Vec<u8>,
    /// The raw header block (status line + headers), owned once; `spans` index into it.
    head: Vec<u8>,
    /// `(name, value)` byte ranges into `head`, one per header, in received order.
    spans: Vec<(Range<usize>, Range<usize>)>,
}

impl Response {
    /// The first value of header `name` (case-insensitive), if present.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.spans
            .iter()
            .find(|(n, _)| self.head[n.clone()].eq_ignore_ascii_case(name.as_bytes()))
            .map(|(_, v)| std::str::from_utf8(&self.head[v.clone()]).unwrap_or(""))
    }

    /// Every header as `(name, value)`, in received order.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.spans.iter().map(move |(n, v)| {
            let str_of = |r: &Range<usize>| std::str::from_utf8(&self.head[r.clone()]).unwrap_or("");
            (str_of(n), str_of(v))
        })
    }

    /// The body as UTF-8 (lossy).
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Whether the status is 2xx.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("headers", &self.headers().collect::<Vec<_>>())
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// Something went wrong making a request.
#[derive(Debug)]
pub enum HttpError {
    /// The URL could not be parsed.
    Url(String),
    /// The host could not be resolved.
    Dns(String),
    /// A socket error.
    Io(std::io::Error),
    /// The response was not valid HTTP.
    Protocol(String),
    /// The TLS handshake failed.
    Tls(String),
    /// The connect or the request exceeded its timeout — a hung/slow server.
    Timeout,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::Url(m) => write!(f, "bad url: {m}"),
            HttpError::Dns(m) => write!(f, "dns: {m}"),
            HttpError::Io(e) => write!(f, "io: {e}"),
            HttpError::Protocol(m) => write!(f, "protocol: {m}"),
            HttpError::Tls(m) => write!(f, "tls: {m}"),
            HttpError::Timeout => write!(f, "timed out"),
        }
    }
}

impl std::error::Error for HttpError {}

impl From<std::io::Error> for HttpError {
    fn from(e: std::io::Error) -> Self {
        HttpError::Io(e)
    }
}

/// An HTTP/1.1 client (plain + TLS). Cheap to clone (shares one TLS config); one
/// request opens and closes one connection — a pool can come later, but OAuth-style
/// call volumes do not need one.
#[derive(Clone)]
pub struct HttpClient {
    tls: Arc<rustls::ClientConfig>,
    pool: Arc<ConnPool>,
    cookies: Option<Arc<cookie::CookieJar>>,
    connect_timeout: Option<Duration>,
    timeout: Option<Duration>,
    max_redirects: usize,
    max_idle_per_host: usize,
}

impl HttpClient {
    /// A new client with the default TLS config (verifies against Mozilla's roots), the
    /// default timeouts ([`DEFAULT_CONNECT_TIMEOUT`], [`DEFAULT_TIMEOUT`]) — so a hung
    /// server can never hang a request forever — redirect following
    /// ([`DEFAULT_MAX_REDIRECTS`]), and connection pooling
    /// ([`DEFAULT_MAX_IDLE_PER_HOST`]). Override them per client. Clone to share the
    /// same pool across cores.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tls: tls::default_config(),
            pool: Arc::new(ConnPool::new(DEFAULT_MAX_IDLE_PER_HOST)),
            cookies: None,
            connect_timeout: Some(DEFAULT_CONNECT_TIMEOUT),
            timeout: Some(DEFAULT_TIMEOUT),
            max_redirects: DEFAULT_MAX_REDIRECTS,
            max_idle_per_host: DEFAULT_MAX_IDLE_PER_HOST,
        }
    }

    /// Enable a cookie jar: store `Set-Cookie` from responses and send matching
    /// `Cookie` headers on later requests (off by default — bearer-token APIs rarely
    /// want implicit cookies). The jar is shared across clones of the client.
    #[must_use]
    pub fn cookie_store(mut self, enabled: bool) -> Self {
        self.cookies = enabled.then(|| Arc::new(cookie::CookieJar::new()));
        self
    }

    /// How many idle keep-alive connections to keep per origin (`0` disables pooling —
    /// every request opens and closes its own connection). Set before first use.
    #[must_use]
    pub fn max_idle_per_host(mut self, max: usize) -> Self {
        self.max_idle_per_host = max;
        self.pool = Arc::new(ConnPool::new(max));
        self
    }

    /// How many redirects to follow (`0` returns the 3xx response without following).
    #[must_use]
    pub fn max_redirects(mut self, max: usize) -> Self {
        self.max_redirects = max;
        self
    }

    /// Cap the whole request (connect through reading the full response). `None`
    /// removes the cap (not recommended — a slow server can then block indefinitely).
    #[must_use]
    pub fn timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.timeout = timeout.into();
        self
    }

    /// Cap establishing the connection (TCP connect + TLS handshake).
    #[must_use]
    pub fn connect_timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.connect_timeout = timeout.into();
        self
    }

    /// `GET url`.
    pub async fn get(&self, url: &str) -> Result<Response, HttpError> {
        self.send(Request::new(Method::Get, Url::parse(url)?)).await
    }

    /// `POST url` with a `application/x-www-form-urlencoded` body (the OAuth2 token
    /// request shape).
    pub async fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<Response, HttpError> {
        let body = encode_form(form);
        let req = Request::new(Method::Post, Url::parse(url)?)
            .body("application/x-www-form-urlencoded", body);
        self.send(req).await
    }

    /// Send a request and read the full response — over TLS for an `https` URL, plain
    /// otherwise. Bounded by the configured timeouts: [`HttpError::Timeout`] if the
    /// connect or the overall request runs long (a hung/slow server).
    pub async fn send(&self, req: Request) -> Result<Response, HttpError> {
        match self.timeout {
            Some(limit) => match rt_core::timeout(limit, self.send_inner(req)).await {
                Ok(result) => result,
                Err(_elapsed) => Err(HttpError::Timeout),
            },
            None => self.send_inner(req).await,
        }
    }

    async fn send_inner(&self, mut req: Request) -> Result<Response, HttpError> {
        let mut redirects = 0;
        loop {
            let response = self.roundtrip(&req).await?;

            // Follow a redirect if we have budget and the response points somewhere.
            if redirects < self.max_redirects {
                if let Some(next) = redirect_target(&req, &response)? {
                    redirects += 1;
                    req = next;
                    continue;
                }
            }
            return Ok(response);
        }
    }

    /// One request→response: reuse a pooled connection when possible, retry once on a
    /// fresh connection if the pooled one turned out to be stale, and return the
    /// connection to the pool when it can be kept alive.
    async fn roundtrip(&self, req: &Request) -> Result<Response, HttpError> {
        let origin = origin_of(&req.url);
        let keep_alive = self.max_idle_per_host > 0;
        let now = now_ms();

        // The Cookie header for this URL, if a jar is enabled and has matches.
        let cookie = self
            .cookies
            .as_ref()
            .map(|jar| jar.header_for(&req.url, now / 1000))
            .filter(|c| !c.is_empty());
        let cookie = cookie.as_deref();

        // Reuse a pooled connection if there is one; on a stale one, fall to a dial.
        let (conn, response, keepable) = match self.pool.take(&origin, now) {
            Some(mut conn) => match send_on(&mut conn, req, keep_alive, cookie).await {
                Ok((response, keepable)) => (conn, response, keepable),
                Err(e) if is_stale(&e) => self.dial_and_send(req, keep_alive, cookie).await?,
                Err(e) => return Err(e),
            },
            None => self.dial_and_send(req, keep_alive, cookie).await?,
        };

        // Record any Set-Cookie the response carried.
        if let Some(jar) = &self.cookies {
            let set: Vec<&str> = response
                .headers()
                .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
                .map(|(_, value)| value)
                .collect();
            if !set.is_empty() {
                jar.store(set.into_iter(), &req.url, now / 1000);
            }
        }

        self.maybe_return(origin, conn, keepable, &response);
        Ok(response)
    }

    /// Open a fresh connection and send the request on it.
    async fn dial_and_send(
        &self,
        req: &Request,
        keep_alive: bool,
        cookie: Option<&str>,
    ) -> Result<(Conn, Response, bool), HttpError> {
        let addr = resolve(&req.url.host, req.url.port)?;
        let mut conn = self.connect(addr, &req.url).await?;
        let (response, keepable) = send_on(&mut conn, req, keep_alive, cookie).await?;
        Ok((conn, response, keepable))
    }

    /// Return the connection to the pool if pooling is on and the exchange left it in a
    /// reusable state (framed body, no `Connection: close`).
    fn maybe_return(&self, origin: Origin, conn: Conn, keepable: bool, response: &Response) {
        let close = response.header("connection").is_some_and(|c| c.eq_ignore_ascii_case("close"));
        if self.max_idle_per_host > 0 && keepable && !close {
            self.pool.put(origin, conn, now_ms());
        }
        // Otherwise `conn` is dropped here (closed).
    }

    /// Establish the connection (TCP + TLS handshake for https), bounded by the
    /// connect timeout.
    async fn connect(&self, addr: std::net::SocketAddr, url: &Url) -> Result<Conn, HttpError> {
        let establish = async {
            let tcp = AsyncTcpStream::connect(addr).await?;
            tcp.set_nodelay(true).ok();
            if url.is_tls() {
                let tls = tls::AsyncTlsStream::connect(tcp, self.tls.clone(), &url.host).await?;
                Ok::<Conn, HttpError>(Conn::Tls(Box::new(tls)))
            } else {
                Ok(Conn::Plain(tcp))
            }
        };
        match self.connect_timeout {
            Some(limit) => match rt_core::timeout(limit, establish).await {
                Ok(result) => result,
                Err(_elapsed) => Err(HttpError::Timeout),
            },
            None => establish.await,
        }
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Resolve `host:port` to a socket address. NOTE: `to_socket_addrs` is a *blocking*
/// DNS lookup; acceptable at the low call volumes this client targets (OAuth, the odd
/// API call), but a caller on a hot path should cache the address.
fn resolve(host: &str, port: u16) -> Result<std::net::SocketAddr, HttpError> {
    (host, port)
        .to_socket_addrs()
        .map_err(|e| HttpError::Dns(format!("{host}:{port}: {e}")))?
        .next()
        .ok_or_else(|| HttpError::Dns(format!("no address for {host}:{port}")))
}

/// Write a request and read its full response. Returns the response and whether the
/// connection is at a clean boundary for reuse (a framed body, not read-to-close).
async fn send_on(conn: &mut Conn, req: &Request, keep_alive: bool, cookie: Option<&str>) -> Result<(Response, bool), HttpError> {
    conn.write_all(&encode_request(req, keep_alive, cookie)).await?;
    read_response(conn).await
}

/// Read the whole response, following `Content-Length`, chunked, or read-to-close. The
/// returned bool is whether the body was framed (so the connection can be kept alive);
/// a read-to-close body consumes the connection.
async fn read_response(stream: &mut Conn) -> Result<(Response, bool), HttpError> {
    let mut buf = Vec::with_capacity(4096);

    // Read until the header terminator (CRLFCRLF) is in `buf`.
    let head_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if !read_more(stream, &mut buf).await? {
            return Err(HttpError::Protocol("connection closed before headers completed".into()));
        }
    };

    // Split without copying the head: `body` takes the tail, `head_block` reuses buf.
    let mut body = buf.split_off(head_end);
    let head_block = buf;
    let (status, spans) = parse_head_spans(&head_block)?;

    let keepable = match body_plan(&head_block, &spans) {
        BodyPlan::Length(n) => {
            while body.len() < n {
                if !read_more_into(stream, &mut body).await? {
                    return Err(HttpError::Protocol("connection closed mid-body".into()));
                }
            }
            body.truncate(n);
            true
        }
        BodyPlan::Chunked => {
            loop {
                if let Some(decoded) = decode_chunked(&body) {
                    body = decoded;
                    break;
                }
                if !read_more_into(stream, &mut body).await? {
                    return Err(HttpError::Protocol("connection closed mid-chunk".into()));
                }
            }
            true
        }
        BodyPlan::UntilClose => {
            while read_more_into(stream, &mut body).await? {}
            false // the connection was consumed to EOF — not reusable
        }
    };

    let mut response = Response { status, body, head: head_block, spans };
    compress::decompress(&mut response)?; // inflate gzip/deflate/br transparently
    Ok((response, keepable))
}

/// Whether an error means the connection was closed under us (so a retry on a fresh
/// connection is worthwhile) — the classic stale-pooled-keep-alive case.
fn is_stale(error: &HttpError) -> bool {
    match error {
        HttpError::Io(e) => matches!(
            e.kind(),
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof
        ),
        HttpError::Protocol(m) => m.contains("closed before headers"),
        _ => false,
    }
}

/// Read one chunk from the connection into `buf`; `false` at EOF.
async fn read_more(stream: &mut Conn, buf: &mut Vec<u8>) -> Result<bool, HttpError> {
    read_more_into(stream, buf).await
}

async fn read_more_into(stream: &mut Conn, buf: &mut Vec<u8>) -> Result<bool, HttpError> {
    let mut chunk = [0u8; 4096];
    let n = stream.read(&mut chunk).await?;
    if n == 0 {
        return Ok(false);
    }
    buf.extend_from_slice(&chunk[..n]);
    Ok(true)
}

// ── pure helpers (no I/O) — the reusable core, shared by the plain + TLS paths ──

/// How to read the body.
enum BodyPlan {
    /// Exactly `n` bytes.
    Length(usize),
    /// `Transfer-Encoding: chunked`.
    Chunked,
    /// Until the connection closes.
    UntilClose,
}

/// Parse the status code and the header `(name, value)` byte ranges — **no allocation
/// per header** (the earlier version allocated two lowercased `String`s each) and no
/// whole-block UTF-8 scan (the status code is read from bytes; header bytes are
/// validated as UTF-8 only when accessed). This is the parser's hot path.
fn parse_head_spans(block: &[u8]) -> Result<(u16, Vec<(Range<usize>, Range<usize>)>), HttpError> {
    let line_end = find(block, b"\r\n").ok_or_else(|| HttpError::Protocol("no status line".into()))?;
    let status = parse_status(&block[..line_end])?;

    let mut spans = Vec::new();
    let mut pos = line_end + 2;
    while pos < block.len() {
        let end = match find(&block[pos..], b"\r\n") {
            Some(i) => pos + i,
            None => break, // no terminator — stop at the last complete line
        };
        if end == pos {
            break; // the blank line before the body
        }
        if let Some(colon) = block[pos..end].iter().position(|&b| b == b':') {
            let name = trim_range(block, pos, pos + colon);
            let value = trim_range(block, pos + colon + 1, end);
            spans.push((name, value));
        }
        pos = end + 2;
    }
    Ok((status, spans))
}

/// The code from a status line like `HTTP/1.1 200 OK` (the token after the version).
fn parse_status(line: &[u8]) -> Result<u16, HttpError> {
    let bad = || HttpError::Protocol("bad status line".into());
    let space = line.iter().position(|&b| b == b' ').ok_or_else(bad)?;
    let rest = &line[space + 1..];
    let end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
    std::str::from_utf8(&rest[..end]).ok().and_then(|s| s.parse::<u16>().ok()).ok_or_else(bad)
}

/// `start..end` with surrounding spaces/tabs trimmed — no allocation, just narrowed.
fn trim_range(block: &[u8], mut start: usize, mut end: usize) -> Range<usize> {
    while start < end && (block[start] == b' ' || block[start] == b'\t') {
        start += 1;
    }
    while end > start && (block[end - 1] == b' ' || block[end - 1] == b'\t') {
        end -= 1;
    }
    start..end
}

/// Look up a header value (bytes) by lowercase name, over the ranges.
fn header_bytes<'a>(block: &'a [u8], spans: &[(Range<usize>, Range<usize>)], name: &[u8]) -> Option<&'a [u8]> {
    spans
        .iter()
        .find(|(n, _)| block[n.clone()].eq_ignore_ascii_case(name))
        .map(|(_, v)| &block[v.clone()])
}

/// Decide how to read the body from the parsed header ranges.
fn body_plan(block: &[u8], spans: &[(Range<usize>, Range<usize>)]) -> BodyPlan {
    if header_bytes(block, spans, b"transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case(b"chunked")) {
        return BodyPlan::Chunked;
    }
    if let Some(len) = header_bytes(block, spans, b"content-length")
        .and_then(|v| std::str::from_utf8(v).ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
    {
        return BodyPlan::Length(len);
    }
    BodyPlan::UntilClose
}

/// Decode a chunked body, or `None` if it is not yet complete.
fn decode_chunked(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0;
    loop {
        // Read the chunk-size line.
        let line_end = find(&bytes[pos..], b"\r\n")? + pos;
        let size_str = std::str::from_utf8(&bytes[pos..line_end]).ok()?;
        // Ignore any chunk extensions after ';'.
        let size_hex = size_str.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16).ok()?;
        pos = line_end + 2;

        if size == 0 {
            return Some(out); // the terminating zero-length chunk
        }
        let end = pos.checked_add(size)?;
        if end + 2 > bytes.len() {
            return None; // chunk data (or its trailing CRLF) not fully arrived
        }
        out.extend_from_slice(&bytes[pos..end]);
        pos = end + 2; // skip the chunk's trailing CRLF
    }
}

/// Serialise a request to the wire, adding Host/Content-Length/Connection/User-Agent
/// (and `Cookie` when the jar supplied one). `keep_alive` picks `Connection:
/// keep-alive` (pooled) vs `close`.
fn encode_request(req: &Request, keep_alive: bool, cookie: Option<&str>) -> Vec<u8> {
    let mut out = format!("{} {} HTTP/1.1\r\n", req.method.as_str(), req.url.path_and_query);

    let has = |name: &str| req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name));
    if !has("host") {
        // Include the port unless it is the scheme default.
        let default_port = if req.url.is_tls() { 443 } else { 80 };
        if req.url.port == default_port {
            out.push_str(&format!("Host: {}\r\n", req.url.host));
        } else {
            out.push_str(&format!("Host: {}:{}\r\n", req.url.host, req.url.port));
        }
    }
    if !has("user-agent") {
        out.push_str("User-Agent: kernway-http-client/0.1\r\n");
    }
    // Advertise the encodings we can transparently inflate on the response.
    if !has("accept-encoding") {
        out.push_str("Accept-Encoding: gzip, deflate, br\r\n");
    }
    for (name, value) in &req.headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if !req.body.is_empty() && !has("content-length") {
        out.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    }
    if !has("connection") {
        out.push_str(if keep_alive { "Connection: keep-alive\r\n" } else { "Connection: close\r\n" });
    }
    if let Some(cookie) = cookie {
        if !cookie.is_empty() && !has("cookie") {
            out.push_str(&format!("Cookie: {cookie}\r\n"));
        }
    }
    out.push_str("\r\n");

    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&req.body);
    bytes
}

/// Build the follow-up request for a redirect response, or `None` if it is not a
/// followable redirect. Applies the standard method rules and, crucially, drops
/// credentials when the target is a different origin (never leak `Authorization`/
/// `Cookie` to another host).
fn redirect_target(req: &Request, response: &Response) -> Result<Option<Request>, HttpError> {
    if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
        return Ok(None);
    }
    let Some(location) = response.header("location") else {
        return Ok(None);
    };
    let url = resolve_location(&req.url, location)?;

    // 307/308 preserve the method + body; 301/302/303 become GET (HEAD stays HEAD) and
    // drop the body.
    let (method, body): (Method, Vec<u8>) = match response.status {
        307 | 308 => (req.method, req.body.clone()),
        _ if req.method == Method::Head => (Method::Head, Vec::new()),
        _ => (Method::Get, Vec::new()),
    };

    let same_origin =
        url.scheme == req.url.scheme && url.host.eq_ignore_ascii_case(&req.url.host) && url.port == req.url.port;
    let dropped_body = body.is_empty() && !req.body.is_empty();

    let headers = req
        .headers
        .iter()
        .filter(|(name, _)| {
            let n = name.to_ascii_lowercase();
            // Never carry credentials across origins.
            if !same_origin && (n == "authorization" || n == "cookie") {
                return false;
            }
            // Body headers are stale once the body is dropped (they are recomputed).
            if dropped_body && (n == "content-type" || n == "content-length") {
                return false;
            }
            true
        })
        .cloned()
        .collect();

    Ok(Some(Request { method, url, headers, body }))
}

/// Resolve a `Location` value against the request URL — absolute, absolute-path, or a
/// path relative to the current directory.
fn resolve_location(base: &Url, location: &str) -> Result<Url, HttpError> {
    let loc = location.trim();
    if loc.contains("://") {
        Url::parse(loc)
    } else if loc.starts_with('/') {
        Ok(Url { path_and_query: loc.to_string(), ..base.clone() })
    } else {
        // Relative to the current path's directory.
        let dir = match base.path_and_query.rfind('/') {
            Some(i) => &base.path_and_query[..=i],
            None => "/",
        };
        Ok(Url { path_and_query: format!("{dir}{loc}"), ..base.clone() })
    }
}

/// `application/x-www-form-urlencoded` from key/value pairs.
fn encode_form(form: &[(&str, &str)]) -> Vec<u8> {
    let mut out = String::new();
    for (i, (k, v)) in form.iter().enumerate() {
        if i > 0 {
            out.push('&');
        }
        out.push_str(&percent_encode(k));
        out.push('=');
        out.push_str(&percent_encode(v));
    }
    out.into_bytes()
}

/// Percent-encode per `application/x-www-form-urlencoded` (unreserved kept, space→%20).
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// Benchmark hooks: the per-request CPU work (encode + parse), exposed for `benches/`
// without widening the public API. Hidden from docs; not part of the stable surface.

/// Benchmark hook — encode a request to the wire (see `benches/client.rs`).
#[doc(hidden)]
pub fn bench_encode_request(req: &Request) -> Vec<u8> {
    encode_request(req, true, None)
}

/// Benchmark hook — parse a response head, returning the header count.
#[doc(hidden)]
pub fn bench_parse_head(head_bytes: &[u8]) -> usize {
    parse_head_spans(head_bytes).map(|(_, spans)| spans.len()).unwrap_or(0)
}

/// Benchmark hook — decode a chunked body, returning its length.
#[doc(hidden)]
pub fn bench_decode_chunked(bytes: &[u8]) -> usize {
    decode_chunked(bytes).map(|v| v.len()).unwrap_or(0)
}

/// Index of the first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let u = Url::parse("https://accounts.google.com/o/oauth2/v2/auth?x=1").unwrap();
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "accounts.google.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.path_and_query, "/o/oauth2/v2/auth?x=1");
        assert!(u.is_tls());

        let plain = Url::parse("http://localhost:8080/status").unwrap();
        assert_eq!(plain.port, 8080);
        assert_eq!(plain.path_and_query, "/status");
        assert!(!plain.is_tls());

        assert_eq!(Url::parse("http://example.com").unwrap().path_and_query, "/", "no path → /");
        assert!(Url::parse("ftp://x.com").is_err(), "unsupported scheme");
        assert!(Url::parse("not-a-url").is_err(), "no scheme");
    }

    #[test]
    fn encodes_a_request_with_default_headers() {
        let req = Request::new(Method::Post, Url::parse("http://api.example.com/v1/things").unwrap())
            .body("application/json", b"{}".to_vec());
        let wire = String::from_utf8(encode_request(&req, false, None)).unwrap();
        assert!(wire.starts_with("POST /v1/things HTTP/1.1\r\n"));
        assert!(wire.contains("Host: api.example.com\r\n"));
        // Custom headers are emitted as stored (lowercased); wire header names are
        // case-insensitive, so this is valid.
        assert!(wire.contains("content-type: application/json\r\n"));
        assert!(wire.contains("Content-Length: 2\r\n"));
        assert!(wire.contains("Connection: close\r\n"));
        assert!(wire.ends_with("\r\n\r\n{}"));
        // With pooling on, the connection is kept alive instead.
        let ka = String::from_utf8(encode_request(&req, true, None)).unwrap();
        assert!(ka.contains("Connection: keep-alive\r\n"));
        // A supplied cookie is included.
        let ck = String::from_utf8(encode_request(&req, true, Some("sid=abc"))).unwrap();
        assert!(ck.contains("Cookie: sid=abc\r\n"));
    }

    #[test]
    fn parses_a_response_head_into_ranges() {
        let block = b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 3\r\n\r\n";
        let (status, spans) = parse_head_spans(block).unwrap();
        assert_eq!(status, 404);
        assert_eq!(spans.len(), 2);
        // The name keeps its on-wire case and matches case-insensitively; the value is
        // the exact byte slice, no allocation.
        let (name, value) = &spans[0];
        assert!(block[name.clone()].eq_ignore_ascii_case(b"content-type"));
        assert_eq!(&block[value.clone()], b"text/plain");
        assert_eq!(header_bytes(block, &spans, b"content-length"), Some(b"3".as_slice()));
    }

    #[test]
    fn decodes_a_chunked_body() {
        // "Wiki" + "pedia" + "" (end), with a chunk extension on the first.
        let raw = b"4;ext=1\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(raw).unwrap(), b"Wikipedia");
        // Incomplete → None.
        assert!(decode_chunked(b"5\r\nped").is_none());
    }

    #[test]
    fn form_is_percent_encoded() {
        let body = String::from_utf8(encode_form(&[("code", "a b/c"), ("grant_type", "authorization_code")])).unwrap();
        assert_eq!(body, "code=a%20b%2Fc&grant_type=authorization_code");
    }

    // A real request/response over a loopback socket, driven on the Kernway runtime —
    // proves the async transport, not just the pure parsing.
    #[test]
    fn fetches_over_a_loopback_socket() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            // Read the request head.
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).unwrap();
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello").unwrap();
        });

        let client = HttpClient::new();
        let resp = rt_core::Executor::new()
            .unwrap()
            .block_on(client.get(&format!("http://127.0.0.1:{port}/status")))
            .unwrap() // executor result
            .unwrap(); // the request result
        assert_eq!(resp.status, 200);
        assert_eq!(resp.text(), "hello");
        assert_eq!(resp.header("content-length"), Some("5"));
        server.join().unwrap();
    }

    /// Build a Response with a status + headers (empty body), via the real parser.
    fn mk_response(status: u16, headers: &[(&str, &str)]) -> Response {
        let mut head = format!("HTTP/1.1 {status} X\r\n");
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        let head = head.into_bytes();
        let (status, spans) = parse_head_spans(&head).unwrap();
        Response { status, body: Vec::new(), head, spans }
    }

    #[test]
    fn redirect_applies_method_rules_and_drops_credentials_across_origins() {
        let req = Request::new(Method::Post, Url::parse("https://a.example/login").unwrap())
            .header("authorization", "Bearer secret")
            .body("application/json", b"{}".to_vec());

        // 302 to a DIFFERENT origin → GET, body + credentials dropped.
        let cross = redirect_target(&req, &mk_response(302, &[("location", "https://b.evil/cb")])).unwrap().unwrap();
        assert_eq!(cross.url.host, "b.evil");
        assert_eq!(cross.method, Method::Get, "302 on POST becomes GET");
        assert!(cross.body.is_empty(), "body dropped");
        assert!(
            !cross.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization")),
            "Authorization must not leak to another origin"
        );

        // 307 to the SAME origin → method + body + credentials preserved.
        let same = redirect_target(&req, &mk_response(307, &[("location", "/next")])).unwrap().unwrap();
        assert_eq!(same.url.host, "a.example");
        assert_eq!(same.url.path_and_query, "/next");
        assert_eq!(same.method, Method::Post, "307 preserves the method");
        assert_eq!(same.body, b"{}", "307 preserves the body");
        assert!(same.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization")), "auth kept same-origin");

        // A non-redirect status → no follow.
        assert!(redirect_target(&req, &mk_response(200, &[])).unwrap().is_none());
    }

    #[test]
    fn follows_a_redirect_over_sockets() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            // One keep-alive connection serving both the redirect and the followed
            // request (the client reuses the pooled connection for the redirect).
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf).unwrap(); // /start
            sock.write_all(b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\n\r\n").unwrap();
            let n = sock.read(&mut buf).unwrap(); // /final, same connection
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndone").unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });

        let resp = rt_core::Executor::new()
            .unwrap()
            .block_on(HttpClient::new().get(&format!("http://127.0.0.1:{port}/start")))
            .unwrap()
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.text(), "done");
        let followed = server.join().unwrap();
        assert!(followed.starts_with("GET /final "), "followed to /final: {followed}");
    }

    #[test]
    fn a_cookie_jar_carries_cookies_to_the_next_request() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // One keep-alive connection: reply 1 sets a cookie; capture request 2 to check
        // it echoes the cookie back.
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf).unwrap();
            sock.write_all(b"HTTP/1.1 200 OK\r\nSet-Cookie: sid=abc123; Path=/\r\nContent-Length: 0\r\n\r\n").unwrap();
            let n = sock.read(&mut buf).unwrap();
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });

        let client = HttpClient::new().cookie_store(true);
        let url = format!("http://127.0.0.1:{port}/");
        rt_core::Executor::new()
            .unwrap()
            .block_on(async {
                let _ = client.get(&url).await; // receives Set-Cookie
                let _ = client.get(&url).await; // should send Cookie: sid=abc123
            })
            .unwrap();
        let second = server.join().unwrap();
        assert!(
            second.to_lowercase().contains("cookie: sid=abc123"),
            "the jar sent the cookie on the next request: {second}"
        );
    }

    #[test]
    fn decompresses_a_gzip_response() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        // gzip a payload the way a server would (via the shared compression crate).
        let payload = b"hello gzip world - this body is served compressed and must arrive plain";
        let gz = kernway_compress::encode(payload, kernway_compress::Encoding::Gzip);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 512];
            let n = sock.read(&mut buf).unwrap();
            let head = format!("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n", gz.len());
            sock.write_all(head.as_bytes()).unwrap();
            sock.write_all(&gz).unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });

        let resp = rt_core::Executor::new()
            .unwrap()
            .block_on(HttpClient::new().get(&format!("http://127.0.0.1:{port}/")))
            .unwrap()
            .unwrap();
        assert_eq!(resp.body, payload, "the body arrives decompressed");
        assert_eq!(resp.header("content-encoding"), None, "the stale encoding header is stripped");
        let request = server.join().unwrap();
        assert!(request.to_lowercase().contains("accept-encoding: gzip"), "we advertised gzip: {request}");
    }

    #[test]
    fn reuses_a_pooled_connection() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // The server accepts ONE connection and serves two requests on it. If the
        // client failed to reuse the connection it would open a second one, which this
        // server never accepts — so the second request completing proves reuse.
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 512];
            for _ in 0..2 {
                let _ = sock.read(&mut buf).unwrap();
                sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").unwrap();
            }
        });

        let client = HttpClient::new();
        let url = format!("http://127.0.0.1:{port}/");
        let (r1, r2) = rt_core::Executor::new()
            .unwrap()
            .block_on(async {
                let a = client.get(&url).await; // opens a connection, pools it
                let b = client.get(&url).await; // reuses the pooled connection
                (a, b)
            })
            .unwrap();
        assert_eq!(r1.unwrap().status, 200, "first request");
        assert_eq!(r2.unwrap().status, 200, "second request reused the pooled connection");
        server.join().unwrap();
    }

    #[test]
    fn a_hung_server_hits_the_timeout() {
        use std::io::Read;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf); // consume the request, then never respond
            std::thread::sleep(Duration::from_millis(300)); // hold the socket open
        });

        // A short timeout must fire even though the server is still holding the socket.
        let client = HttpClient::new().timeout(Duration::from_millis(80));
        let err = rt_core::Executor::new()
            .unwrap()
            .block_on(client.get(&format!("http://127.0.0.1:{port}/hang")))
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, HttpError::Timeout), "a hung server must time out, got {err:?}");
        server.join().unwrap();
    }

    // A real HTTPS request — proves the rustls handshake + cert verification over the
    // async socket. Ignored (needs network); run with:
    //   cargo test -p kernway-http-client -- --ignored
    #[test]
    #[ignore = "needs network (real HTTPS to example.com)"]
    fn fetches_over_real_https() {
        let client = HttpClient::new();
        let resp = rt_core::Executor::new()
            .unwrap()
            .block_on(client.get("https://example.com/"))
            .unwrap()
            .unwrap();
        assert_eq!(resp.status, 200, "TLS handshake + verified cert + 200 from example.com");
        assert!(resp.text().contains("Example Domain"), "got the page body over TLS");
    }
}
