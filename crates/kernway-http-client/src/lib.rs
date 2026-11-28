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
//! Speaks HTTP/1.1 (`Content-Length`, chunked with trailers, read-to-close, interim
//! `1xx`) over plain TCP or **TLS**: an `https://` URL is dialled through `rustls`,
//! verifying against Mozilla's roots, a private CA ([`HttpClient::trust_ca_pem`]), and
//! presenting a client certificate for mutual TLS ([`HttpClient::client_identity_pem`]).
//!
//! What it guards against, by default:
//! - a request split on the wire — header names and values with CR / LF / NUL, and
//!   URLs with control characters, are refused before anything is sent;
//! - memory exhaustion — a head over [`MAX_HEAD_SIZE`], a buffered body or an inflated
//!   one over [`HttpClient::max_response_size`] fail with [`HttpError::TooLarge`];
//! - credentials leaking — `Authorization`, `Cookie`, `Proxy-Authorization` and URL
//!   credentials are dropped on a redirect to another origin;
//! - a payment sent twice — a POST that may have reached the server is never resent on
//!   a fresh connection; idempotent requests are, once, and a pooled keep-alive the
//!   server already closed is noticed before a request is written to it.

#![forbid(unsafe_code)]

mod compress;
mod cookie;
// Public: kernway-ws reuses the TLS transport (AsyncTlsStream + default_config) for wss://.
/// Client TLS — re-exported from [`kernway_tls`], where it now lives so the MySQL
/// driver shares it.
pub use kernway_tls as tls;

use std::collections::HashMap;
use std::fmt;
use std::io::ErrorKind;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant, SystemTime};

use rt_net::AsyncTcpStream;

/// Default cap on establishing a connection (TCP connect + TLS handshake).
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default cap on the whole request (connect through reading the full response).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default maximum number of redirects to follow before returning the 3xx response.
pub const DEFAULT_MAX_REDIRECTS: usize = 10;
/// Default number of idle keep-alive connections kept per origin.
pub const DEFAULT_MAX_IDLE_PER_HOST: usize = 8;
/// Default cap on a buffered response body, after decompression (64 MiB). Larger
/// downloads stream ([`HttpClient::send_streaming`]) or raise the cap.
pub const DEFAULT_MAX_RESPONSE_SIZE: usize = 64 << 20;
/// The largest response head (status line + headers, or chunked trailers) read
/// before giving up — a server that never ends its headers cannot exhaust memory.
pub const MAX_HEAD_SIZE: usize = 64 << 10;
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

    /// Whether an idle pooled connection can carry another request: the server has
    /// not closed it (nor sent anything unasked). Checked without waiting, so a
    /// keep-alive the server timed out is dropped *before* a request is written to
    /// it — which is what makes it safe not to resend a POST on a dead one.
    fn is_idle_open(&mut self) -> bool {
        match self {
            Conn::Plain(s) => {
                let mut probe = [0u8; 1];
                matches!(s.try_read(&mut probe), Err(e) if e.kind() == ErrorKind::WouldBlock)
            }
            Conn::Tls(s) => s.is_idle_open(),
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

/// A pool of idle keep-alive connections. The `Mutex` is held only to pop/push a
/// connection, never across a request's `.await`, so a slow request never blocks
/// another core at the lock.
///
/// **Per-shard, not shared across cores.** A socket is registered on the reactor of the
/// shard (thread) that created it; its readiness events are delivered there and nowhere
/// else. Reusing a pooled connection on a *different* shard would park the waiter on a
/// reactor that never owns the socket, so the read is never woken (it dies at the request
/// timeout) — the source of intermittent, shard-dependent failures. Keying the bucket by
/// the owning `ThreadId` keeps each connection on its home shard.
struct ConnPool {
    idle: Mutex<HashMap<(ThreadId, Origin), Vec<Idle>>>,
    max_per_origin: usize,
}

impl ConnPool {
    fn new(max_per_origin: usize) -> Self {
        Self {
            idle: Mutex::new(HashMap::new()),
            max_per_origin,
        }
    }

    /// Take a fresh-enough idle connection for `origin` **on the current shard**,
    /// discarding any that have sat too long (a server would have closed them).
    fn take(&self, origin: &Origin, now_ms: u64) -> Option<Conn> {
        let key = (thread::current().id(), origin.clone());
        let mut map = self.idle.lock().unwrap();
        let bucket = map.get_mut(&key)?;
        while let Some(mut entry) = bucket.pop() {
            if now_ms.saturating_sub(entry.since_ms) < POOL_IDLE_TIMEOUT_MS && entry.conn.is_idle_open() {
                return Some(entry.conn);
            }
            // otherwise the connection is dead or likely so — drop it and try the next
        }
        None
    }

    /// Return a reusable connection to the pool, tagged to the current shard (dropped if
    /// the bucket is full).
    fn put(&self, origin: Origin, conn: Conn, now_ms: u64) {
        if self.max_per_origin == 0 {
            return;
        }
        let key = (thread::current().id(), origin);
        let mut map = self.idle.lock().unwrap();
        let bucket = map.entry(key).or_default();
        if bucket.len() < self.max_per_origin {
            bucket.push(Idle {
                conn,
                since_ms: now_ms,
            });
        }
    }
}

/// Unix milliseconds now (for pool idle timestamps).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    /// `OPTIONS`.
    Options,
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
            Method::Options => "OPTIONS",
        }
    }

    /// Whether sending the request twice has the effect of sending it once
    /// (RFC 9110 §9.2.2) — so a connection that died before answering may be
    /// retried. POST and PATCH are not.
    #[must_use]
    pub fn is_idempotent(self) -> bool {
        !matches!(self, Method::Post | Method::Patch)
    }
}

/// A parsed absolute URL — enough of one to open a connection and write a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    /// `http` or `https`.
    pub scheme: String,
    /// The host, lowercased: a name, an IPv4 address, or an IPv6 address
    /// **without** its brackets (`::1`).
    pub host: String,
    /// The port (defaulted from the scheme if absent).
    pub port: u16,
    /// The path plus query, always starting with `/` (`"/"` if the URL had none).
    /// Spaces and non-ASCII are percent-encoded; the `#fragment` is not kept (it
    /// never goes on the wire).
    pub path_and_query: String,
    /// `user:password@` from the URL, percent-decoded — sent as HTTP Basic
    /// authentication unless the request sets its own `Authorization`.
    pub userinfo: Option<(String, String)>,
}

impl Url {
    /// Parse an absolute `http`/`https` URL. Rejects anything without a scheme +
    /// host, control characters anywhere (they could split the request on the
    /// wire), and host names that are not ASCII (internationalized names must be
    /// given in their `xn--` form).
    pub fn parse(url: &str) -> Result<Url, HttpError> {
        let bad = |what: &str| HttpError::Url(format!("{what} in {url:?}"));
        if url.chars().any(|c| c.is_control()) {
            return Err(bad("a control character"));
        }
        let (scheme, rest) = url.split_once("://").ok_or_else(|| bad("no scheme"))?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(HttpError::Url(format!("unsupported scheme {scheme:?}")));
        }
        // The fragment is the client's alone.
        let rest = rest.split('#').next().unwrap_or("");
        // The authority ends at the first '/' or '?'.
        let end = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, authority),
        };
        let (host, port, bracketed) = if let Some(inner) = hostport.strip_prefix('[') {
            let (ip, after) = inner.split_once(']').ok_or_else(|| bad("an unclosed '[' in the host"))?;
            ip.parse::<Ipv6Addr>().map_err(|_| bad("a bad IPv6 address"))?;
            let port = match after {
                "" => None,
                p => Some(p.strip_prefix(':').ok_or_else(|| bad("junk after the IPv6 address"))?),
            };
            (ip, port, true)
        } else {
            match hostport.rsplit_once(':') {
                Some((h, p)) => (h, Some(p), false),
                None => (hostport, None, false),
            }
        };
        if host.is_empty() {
            return Err(bad("no host"));
        }
        // A name or IPv4 address: letters, digits, '-', '.', '_' only.
        if !bracketed && !host.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_')) {
            return Err(bad("a host that is not an ASCII name or IP address"));
        }
        let port = match port {
            None | Some("") => if scheme == "https" { 443 } else { 80 },
            Some(p) => p.parse::<u16>().map_err(|_| bad("a bad port"))?,
        };
        let userinfo = userinfo.map(|u| {
            let (user, pass) = u.split_once(':').unwrap_or((u, ""));
            (percent_decode(user), percent_decode(pass))
        });
        let path_and_query = match tail {
            "" => "/".to_owned(),
            t if t.starts_with('?') => format!("/{}", encode_target(t)),
            t => encode_target(t),
        };
        Ok(Url {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
            path_and_query,
            userinfo,
        })
    }

    /// Whether this URL uses TLS.
    #[must_use]
    pub fn is_tls(&self) -> bool {
        self.scheme == "https"
    }

    /// `host[:port]` as it goes in the `Host` header: the port only when it is
    /// not the scheme's default, an IPv6 address in brackets.
    #[must_use]
    pub fn authority(&self) -> String {
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let default_port = if self.is_tls() { 443 } else { 80 };
        if self.port == default_port {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

/// Percent-encode what may not appear raw in a request target: spaces, `"`, `<`,
/// `>`, `` ` ``, and every non-ASCII byte (WHATWG's path set). Existing `%XX`
/// escapes are kept as they are.
fn encode_target(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b > 0x7E || matches!(b, b' ' | b'"' | b'<' | b'>' | b'`') {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push(b as char);
        }
    }
    out
}

/// Decode `%XX` escapes (invalid ones are kept literally); the bytes are read as
/// UTF-8, lossily.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' {
            if let (Some(h), Some(l)) = (b.get(i + 1).and_then(|&c| hex(c)), b.get(i + 2).and_then(|&c| hex(c))) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
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
        Self {
            method,
            url,
            headers: Vec::new(),
            body: Vec::new(),
        }
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

    /// HTTP Basic authentication (RFC 7617): `Authorization: Basic base64(user:password)`.
    #[must_use]
    pub fn basic_auth(self, user: &str, password: &str) -> Self {
        let token = base64(format!("{user}:{password}").as_bytes());
        self.header("authorization", format!("Basic {token}"))
    }

    /// A bearer token (RFC 6750): `Authorization: Bearer <token>`.
    #[must_use]
    pub fn bearer_auth(self, token: &str) -> Self {
        self.header("authorization", format!("Bearer {token}"))
    }

    /// Refuse what would not survive the trip as one request: a header name that is
    /// not an RFC 9110 token, or a CR, LF or NUL in a header value — either would
    /// let the bytes after it be read as another header or another request.
    fn check(&self) -> Result<(), HttpError> {
        for (name, value) in &self.headers {
            let token = |b: u8| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b);
            if name.is_empty() || !name.bytes().all(token) {
                return Err(HttpError::InvalidRequest(format!("header name {name:?} is not a token")));
            }
            if value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
                return Err(HttpError::InvalidRequest(format!("header {name:?}: a CR, LF or NUL in the value")));
            }
        }
        Ok(())
    }
}

/// Standard base64 with padding (RFC 4648 §4) — the shared one.
fn base64(data: &[u8]) -> String {
    kernway_wire::base64::encode(data)
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
    /// The trailer fields after a chunked body (raw lines), and their ranges.
    trailer: Vec<u8>,
    trailer_spans: Vec<(Range<usize>, Range<usize>)>,
}

impl Response {
    /// A trailer field sent after a chunked body (RFC 9110 §6.5), if present —
    /// a checksum or status computed while the body streamed.
    #[must_use]
    pub fn trailer(&self, name: &str) -> Option<&str> {
        self.trailer_spans
            .iter()
            .find(|(n, _)| self.trailer[n.clone()].eq_ignore_ascii_case(name.as_bytes()))
            .map(|(_, v)| std::str::from_utf8(&self.trailer[v.clone()]).unwrap_or(""))
    }

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
            let str_of =
                |r: &Range<usize>| std::str::from_utf8(&self.head[r.clone()]).unwrap_or("");
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
    /// The TLS handshake failed (an untrusted or mismatched certificate, no
    /// common protocol version, a client certificate the server refused).
    Tls(String),
    /// The connect or the request exceeded its timeout — a hung/slow server.
    Timeout,
    /// The request cannot be sent as it is (a header that would split it).
    InvalidRequest(String),
    /// The response is larger than the client accepts ([`MAX_HEAD_SIZE`] for the
    /// head, [`HttpClient::max_response_size`] for a buffered body).
    TooLarge(String),
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
            HttpError::InvalidRequest(m) => write!(f, "invalid request: {m}"),
            HttpError::TooLarge(m) => write!(f, "response too large: {m}"),
        }
    }
}

impl std::error::Error for HttpError {}

impl From<std::io::Error> for HttpError {
    fn from(e: std::io::Error) -> Self {
        // A TLS alert read after the handshake (TLS 1.3 servers refuse a client
        // certificate there) is still a TLS failure, not a socket one.
        if e.get_ref().is_some_and(|inner| inner.is::<rustls::Error>()) {
            return HttpError::Tls(e.to_string());
        }
        HttpError::Io(e)
    }
}

/// An HTTP/1.1 client (plain + TLS) with a keep-alive pool. Cheap to clone: clones
/// share the TLS config, the pool and the cookie jar.
#[derive(Clone)]
pub struct HttpClient {
    tls: Arc<rustls::ClientConfig>,
    pool: Arc<ConnPool>,
    cookies: Option<Arc<cookie::CookieJar>>,
    connect_timeout: Option<Duration>,
    timeout: Option<Duration>,
    max_redirects: usize,
    max_idle_per_host: usize,
    max_response_size: usize,
}

impl HttpClient {
    /// A new client with the default TLS config (verifies against Mozilla's roots), the
    /// default timeouts ([`DEFAULT_CONNECT_TIMEOUT`], [`DEFAULT_TIMEOUT`]) — so a hung
    /// server can never hang a request forever — redirect following
    /// ([`DEFAULT_MAX_REDIRECTS`]), connection pooling ([`DEFAULT_MAX_IDLE_PER_HOST`])
    /// and a [`DEFAULT_MAX_RESPONSE_SIZE`] cap on buffered bodies. Override them per
    /// client. Clone to share the same pool across cores.
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
            max_response_size: DEFAULT_MAX_RESPONSE_SIZE,
        }
    }

    /// Use this TLS configuration for `https://` — a private CA, a client
    /// certificate for mutual TLS, pinned protocol versions. [`tls::client_config`]
    /// and [`tls::client_config_with_identity`] build the usual ones.
    #[must_use]
    pub fn tls_config(mut self, config: Arc<rustls::ClientConfig>) -> Self {
        self.tls = config;
        self.pool = Arc::new(ConnPool::new(self.max_idle_per_host));
        self
    }

    /// Trust the CA certificate(s) in `ca_pem` (instead of Mozilla's roots) — for
    /// services behind a private CA. Fails if the PEM holds no certificate.
    pub fn trust_ca_pem(self, ca_pem: &[u8]) -> Result<Self, HttpError> {
        let config = tls::client_config(Some(ca_pem), tls::CertCheck::Full).map_err(|e| HttpError::Tls(e.to_string()))?;
        Ok(self.tls_config(config))
    }

    /// Present a client certificate (mutual TLS), trusting `ca_pem` — or Mozilla's
    /// roots when `None` — for the server. Fails on an unreadable or mismatched
    /// certificate / key pair.
    pub fn client_identity_pem(self, ca_pem: Option<&[u8]>, cert_pem: &[u8], key_pem: &[u8]) -> Result<Self, HttpError> {
        let id = tls::ClientIdentity { cert_pem, key_pem };
        let config = tls::client_config_with_identity(ca_pem, tls::CertCheck::Full, Some(id)).map_err(|e| HttpError::Tls(e.to_string()))?;
        Ok(self.tls_config(config))
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

    /// The largest body [`send`](Self::send) (and [`ResponseStream::read_to_end`])
    /// will hold in memory, after decompression: past it the request fails with
    /// [`HttpError::TooLarge`] instead of exhausting memory (a huge download, or a
    /// small gzip "bomb"). Streaming with [`ResponseStream::chunk`] is not capped.
    #[must_use]
    pub fn max_response_size(mut self, max: usize) -> Self {
        self.max_response_size = max;
        self
    }

    /// Cap the whole request (connect through reading the full response). `None`
    /// removes the cap (not recommended — a slow server can then block indefinitely).
    #[must_use]
    pub fn timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.timeout = timeout.into();
        self
    }

    /// Cap establishing the connection (TCP connect + TLS handshake), across every
    /// address the host resolves to.
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
        req.check()?;
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

    /// `GET url`, streaming the response body — see [`send_streaming`](Self::send_streaming).
    pub async fn get_streaming(&self, url: &str) -> Result<ResponseStream, HttpError> {
        self.send_streaming(Request::new(Method::Get, Url::parse(url)?))
            .await
    }

    /// Send a request and return a [`ResponseStream`]: the head is ready immediately and
    /// the body is pulled a chunk at a time, so memory stays O(chunk) for large downloads,
    /// proxying, or SSE. Redirects are followed (their small bodies drained); the final
    /// hop is the one you stream.
    ///
    /// Requests `Accept-Encoding: identity` unless you set the header yourself — the
    /// streamed bytes are raw and are **not** transparently inflated (unlike [`send`](Self::send)).
    pub async fn send_streaming(&self, req: Request) -> Result<ResponseStream, HttpError> {
        req.check()?;
        let mut req = req;
        // No transparent decompression mid-stream: ask the server not to encode.
        if !req
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("accept-encoding"))
        {
            req = req.header("accept-encoding", "identity");
        }

        let mut redirects = 0;
        loop {
            let mut stream = self.dial_and_stream(&req).await?;

            // Follow a redirect if we have budget and the head points somewhere. The head
            // alone decides; the (usually empty) redirect body is drained to reuse the
            // connection before following.
            if redirects < self.max_redirects {
                let head_only = Response {
                    status: stream.status,
                    body: Vec::new(),
                    head: stream.head.clone(),
                    spans: stream.spans.clone(),
                    trailer: Vec::new(),
                    trailer_spans: Vec::new(),
                };
                if let Some(next) = redirect_target(&req, &head_only)? {
                    while stream.chunk().await?.is_some() {}
                    redirects += 1;
                    req = next;
                    continue;
                }
            }
            return Ok(stream);
        }
    }

    /// The `Cookie` header for `url`, if a jar is enabled and has matches.
    fn cookie_for(&self, url: &Url, now_ms: u64) -> Option<String> {
        self.cookies
            .as_ref()
            .map(|jar| jar.header_for(url, now_ms / 1000))
            .filter(|c| !c.is_empty())
    }

    /// One streaming request→head: reuse a pooled connection when possible (retry on a
    /// fresh one when that is safe), read only the head, and hand the live connection to
    /// a `ResponseStream`.
    async fn dial_and_stream(&self, req: &Request) -> Result<ResponseStream, HttpError> {
        let origin = origin_of(&req.url);
        let keep_alive = self.max_idle_per_host > 0;
        let now = now_ms();
        let cookie = self.cookie_for(&req.url, now);
        let cookie = cookie.as_deref();

        let (conn, head) = match self.pool.take(&origin, now) {
            Some(mut conn) => match self.head_exchange(&mut conn, req, keep_alive, cookie).await {
                Ok(head) => (conn, head),
                Err(e) if e.retryable(req) => self.dial_head(req, keep_alive, cookie).await?,
                Err(e) => return Err(e.error),
            },
            None => self.dial_head(req, keep_alive, cookie).await?,
        };

        // Record any Set-Cookie the head carried (same as the buffered path).
        if let Some(jar) = &self.cookies {
            let set = set_cookies(&head.head, &head.spans);
            if !set.is_empty() {
                jar.store(set.into_iter(), &req.url, now / 1000);
            }
        }

        Ok(ResponseStream {
            status: head.status,
            head: head.head,
            spans: head.spans,
            conn: Some(conn),
            buf: head.buf,
            plan: head.plan,
            pool: Arc::clone(&self.pool),
            origin,
            max_idle_per_host: self.max_idle_per_host,
            close: head.close,
            idle_timeout: self.timeout,
            max_response_size: self.max_response_size,
        })
    }

    /// Open a fresh connection and read the response head on it.
    async fn dial_head(
        &self,
        req: &Request,
        keep_alive: bool,
        cookie: Option<&str>,
    ) -> Result<(Conn, HeadRead), HttpError> {
        let mut conn = self.dial(&req.url).await?;
        let head = self
            .head_exchange(&mut conn, req, keep_alive, cookie)
            .await
            .map_err(|e| e.error)?;
        Ok((conn, head))
    }

    /// Write the request and read only the head, bounded by the overall timeout (the body
    /// then streams under the per-chunk idle timeout).
    async fn head_exchange(
        &self,
        conn: &mut Conn,
        req: &Request,
        keep_alive: bool,
        cookie: Option<&str>,
    ) -> Result<HeadRead, Failed> {
        let exchange = async {
            conn.write_all(&encode_request(req, keep_alive, cookie))
                .await
                .map_err(Failed::writing)?;
            read_stream_head(conn, req.method == Method::Head).await.map_err(Failed::reading)
        };
        match self.timeout {
            Some(limit) => match rt_core::timeout(limit, exchange).await {
                Ok(result) => result,
                Err(_elapsed) => Err(Failed::reading(HttpError::Timeout)),
            },
            None => exchange.await,
        }
    }

    /// One request→response: reuse a pooled connection when possible, retry on a fresh
    /// connection if the pooled one turned out to be dead and resending is safe, and
    /// return the connection to the pool when it can be kept alive.
    async fn roundtrip(&self, req: &Request) -> Result<Response, HttpError> {
        let origin = origin_of(&req.url);
        let keep_alive = self.max_idle_per_host > 0;
        let now = now_ms();
        let cookie = self.cookie_for(&req.url, now);
        let cookie = cookie.as_deref();
        let max = self.max_response_size;

        // Reuse a pooled connection if there is one; on a dead one, fall to a dial.
        let (conn, response, keepable) = match self.pool.take(&origin, now) {
            Some(mut conn) => match send_on(&mut conn, req, keep_alive, cookie, max).await {
                Ok((response, keepable)) => (conn, response, keepable),
                Err(e) if e.retryable(req) => self.dial_and_send(req, keep_alive, cookie).await?,
                Err(e) => return Err(e.error),
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

        self.maybe_return(origin, conn, keepable);
        Ok(response)
    }

    /// Open a fresh connection and send the request on it.
    async fn dial_and_send(
        &self,
        req: &Request,
        keep_alive: bool,
        cookie: Option<&str>,
    ) -> Result<(Conn, Response, bool), HttpError> {
        let mut conn = self.dial(&req.url).await?;
        let (response, keepable) = send_on(&mut conn, req, keep_alive, cookie, self.max_response_size)
            .await
            .map_err(|e| e.error)?;
        Ok((conn, response, keepable))
    }

    /// Return the connection to the pool if pooling is on and the exchange left it in a
    /// reusable state (framed body, persistent connection).
    fn maybe_return(&self, origin: Origin, conn: Conn, keepable: bool) {
        if self.max_idle_per_host > 0 && keepable {
            self.pool.put(origin, conn, now_ms());
        }
        // Otherwise `conn` is dropped here (closed).
    }

    /// Resolve `url`'s host and connect to the first of its addresses that answers,
    /// in the order the resolver gave them — a `localhost` whose `::1` refuses still
    /// reaches `127.0.0.1`. The connect timeout covers every attempt: each address
    /// gets an equal share of what is left, so one that never answers cannot use it
    /// all up.
    async fn dial(&self, url: &Url) -> Result<Conn, HttpError> {
        let addrs = resolve_all(&url.host, url.port).await?;
        let started = Instant::now();
        let mut last = None;
        for (i, &addr) in addrs.iter().enumerate() {
            let share = self.connect_timeout.map(|limit| {
                let left = limit.saturating_sub(started.elapsed());
                left / u32::try_from(addrs.len() - i).unwrap_or(u32::MAX)
            });
            match self.connect(addr, url, share).await {
                Ok(conn) => return Ok(conn),
                // A TLS failure is the server's answer, not an unreachable address.
                Err(e @ HttpError::Tls(_)) => return Err(e),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| HttpError::Dns(format!("no address for {}", url.host))))
    }

    /// Establish the connection (TCP + TLS handshake for https), bounded by `limit`.
    async fn connect(&self, addr: SocketAddr, url: &Url, limit: Option<Duration>) -> Result<Conn, HttpError> {
        let establish = async {
            let tcp = AsyncTcpStream::connect(addr).await?;
            tcp.set_nodelay(true).ok();
            if url.is_tls() {
                let tls = tls::AsyncTlsStream::connect(tcp, self.tls.clone(), &url.host)
                    .await
                    .map_err(|e| HttpError::Tls(e.to_string()))?;
                Ok::<Conn, HttpError>(Conn::Tls(Box::new(tls)))
            } else {
                Ok(Conn::Plain(tcp))
            }
        };
        match limit {
            Some(limit) => match rt_core::timeout(limit, establish).await {
                Ok(result) => result,
                Err(_elapsed) => Err(HttpError::Timeout),
            },
            None => establish.await,
        }
    }
}

/// A failed exchange, and whether the request may already have reached the server.
struct Failed {
    error: HttpError,
    /// The request was fully written before it failed (so the server may have acted on it).
    written: bool,
}

impl Failed {
    fn writing(e: std::io::Error) -> Self {
        Self { error: e.into(), written: false }
    }

    fn reading(error: HttpError) -> Self {
        Self { error, written: true }
    }

    /// Whether to resend `req` on a fresh connection after its pooled one failed: the
    /// connection died under it (closed, reset — not a timeout or a bad response), and
    /// either the request never got out or sending it twice is harmless (an idempotent
    /// method, or an `Idempotency-Key` the server deduplicates on). A POST that went
    /// out is never resent: the server may have acted on it before closing.
    fn retryable(&self, req: &Request) -> bool {
        let dead = match &self.error {
            HttpError::Io(e) => matches!(
                e.kind(),
                ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe | ErrorKind::UnexpectedEof
            ),
            HttpError::Protocol(m) => m.contains("closed before headers"),
            _ => false,
        };
        let replayable = req.method.is_idempotent()
            || req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("idempotency-key"));
        dead && (!self.written || replayable)
    }
}

impl Default for HttpClient {
    fn default() -> Self {
        Self::new()
    }
}

/// The system's resolver configuration and hosts file, read once — on the blocking
/// pool, since reading a file blocks and a request may be the first thing a shard does.
struct SystemTables {
    resolver: kernway_dns::Resolver,
    hosts: HashMap<String, Vec<IpAddr>>,
}

async fn system_tables() -> &'static SystemTables {
    static T: OnceLock<SystemTables> = OnceLock::new();
    if let Some(t) = T.get() {
        return t;
    }
    let load = || SystemTables { resolver: kernway_dns::Resolver::from_system(), hosts: kernway_dns::system::load_hosts() };
    let loaded = rt_core::spawn_blocking(load).await;
    T.get_or_init(|| loaded.unwrap_or_else(|| SystemTables { resolver: kernway_dns::Resolver::from_conf(Default::default()), hosts: HashMap::new() }))
}

/// Resolve `host:port` to a socket address, **without ever blocking the executor
/// on `getaddrinfo`** for the common cases.
///
/// Resolution order:
/// 1. **IP literal** (`127.0.0.1`, `::1`) — parsed directly, no lookup.
/// 2. **`/etc/hosts`** (and a `localhost` safety net) — a cached file read.
/// 3. **Async UDP DNS** ([`kernway_dns`]) against the system nameservers — bounded
///    by the resolver's own per-query timeout, so a slow DNS server cannot hang
///    the request the way the old unbounded `getaddrinfo` call could.
/// 4. **`getaddrinfo` fallback** (feature `getaddrinfo-fallback`, default on) — on
///    the blocking pool, for what the stub deliberately skips (search domains,
///    mDNS, split-DNS). Bounded by a 5 s timeout so a wedged lookup can't hang the
///    caller (the pool thread may linger, but the request proceeds).
async fn resolve_all(host: &str, port: u16) -> Result<Vec<SocketAddr>, HttpError> {
    let host = host.strip_suffix('.').unwrap_or(host);
    let with_port = |ips: &[IpAddr]| ips.iter().map(|&ip| SocketAddr::new(ip, port)).collect::<Vec<_>>();

    // 1. IP literal — no lookup at all.
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }

    // 2. The hosts file. `localhost` is always both loopbacks (RFC 6761 §6.3),
    //    whatever the file lists: a server on only one of them is still reached.
    let tables = system_tables().await;
    let key = host.to_ascii_lowercase();
    if key == "localhost" {
        let mut ips = tables.hosts.get(&key).cloned().unwrap_or_default();
        ips.retain(IpAddr::is_loopback);
        for lo in [IpAddr::V4(Ipv4Addr::LOCALHOST), IpAddr::V6(Ipv6Addr::LOCALHOST)] {
            if !ips.contains(&lo) {
                ips.push(lo);
            }
        }
        return Ok(with_port(&ips));
    }
    if let Some(ips) = tables.hosts.get(&key).filter(|ips| !ips.is_empty()) {
        return Ok(with_port(ips));
    }

    // 3. Async UDP DNS (A preferred, AAAA fallback; honours search domains).
    let dns_err = match tables.resolver.lookup(host).await {
        Ok(addrs) if !addrs.is_empty() => return Ok(with_port(&addrs)),
        Ok(_) => "no A/AAAA records".to_owned(),
        Err(e) => e.to_string(),
    };

    // 4. Last resort.
    resolve_fallback(host, port, &dns_err).await
}

/// getaddrinfo on the blocking pool, bounded so an uncancellable lookup can't
/// hang the caller. Present when `getaddrinfo-fallback` is enabled.
#[cfg(feature = "getaddrinfo-fallback")]
async fn resolve_fallback(host: &str, port: u16, _dns_err: &str) -> Result<Vec<SocketAddr>, HttpError> {
    use std::net::ToSocketAddrs;
    let owned = host.to_owned();
    let lookup = rt_core::spawn_blocking(move || {
        let addrs: Vec<SocketAddr> = (owned.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| HttpError::Dns(format!("{owned}:{port}: {e}")))?
            .collect();
        if addrs.is_empty() {
            return Err(HttpError::Dns(format!("no address for {owned}:{port}")));
        }
        Ok(addrs)
    });
    match rt_core::timeout(Duration::from_secs(5), lookup).await {
        Ok(done) => done.unwrap_or_else(|| Err(HttpError::Dns("DNS thread panicked".into()))),
        Err(_elapsed) => Err(HttpError::Dns(format!("getaddrinfo timed out for {host}"))),
    }
}

/// With the fallback disabled, an async-DNS miss is terminal.
#[cfg(not(feature = "getaddrinfo-fallback"))]
async fn resolve_fallback(host: &str, _port: u16, dns_err: &str) -> Result<Vec<SocketAddr>, HttpError> {
    Err(HttpError::Dns(format!("could not resolve {host}: {dns_err}")))
}

/// Write a request and read its full response (at most `max` body bytes). Returns the
/// response and whether the connection is at a clean boundary for reuse.
async fn send_on(
    conn: &mut Conn,
    req: &Request,
    keep_alive: bool,
    cookie: Option<&str>,
    max: usize,
) -> Result<(Response, bool), Failed> {
    conn.write_all(&encode_request(req, keep_alive, cookie))
        .await
        .map_err(Failed::writing)?;
    read_response(conn, req.method == Method::Head, max).await.map_err(Failed::reading)
}

/// How the streaming reader delivers the body, and where it currently sits.
enum StreamPlan {
    /// `Content-Length`: this many body bytes remain to deliver.
    Length(usize),
    /// `Transfer-Encoding: chunked`, decoded incrementally.
    Chunked(ChunkDecoder),
    /// No framing — deliver until the connection closes (consumes it).
    UntilClose,
}

/// The head plus streaming state, produced by [`read_stream_head`] and moved into a
/// [`ResponseStream`].
struct HeadRead {
    status: u16,
    head: Vec<u8>,
    spans: Vec<(Range<usize>, Range<usize>)>,
    /// Body bytes already read alongside the head (not yet delivered).
    buf: Vec<u8>,
    plan: StreamPlan,
    /// The connection does not persist after this response (`Connection: close`,
    /// or HTTP/1.0 without `keep-alive`).
    close: bool,
}

/// Read the head and set up the streaming body plan, without reading any body.
async fn read_stream_head(stream: &mut Conn, head_request: bool) -> Result<HeadRead, HttpError> {
    let (status, head, spans, buf) = read_head(stream).await?;
    let plan = match body_plan(&head, &spans, head_request, status)? {
        BodyPlan::Length(n) => StreamPlan::Length(n),
        BodyPlan::Chunked => StreamPlan::Chunked(ChunkDecoder::new()),
        BodyPlan::UntilClose => StreamPlan::UntilClose,
    };
    let close = !persists(&head, &spans);
    Ok(HeadRead {
        status,
        head,
        spans,
        buf,
        plan,
        close,
    })
}

/// Whether the connection stays open after this response (RFC 9112 §9.3): HTTP/1.1
/// unless it says `Connection: close`; HTTP/1.0 only if it says `keep-alive`.
fn persists(head: &[u8], spans: &[(Range<usize>, Range<usize>)]) -> bool {
    let tokens = |v: &[u8], t: &str| {
        std::str::from_utf8(v).is_ok_and(|v| v.split(',').any(|x| x.trim().eq_ignore_ascii_case(t)))
    };
    let connection: Vec<&[u8]> = spans
        .iter()
        .filter(|(n, _)| head[n.clone()].eq_ignore_ascii_case(b"connection"))
        .map(|(_, v)| &head[v.clone()])
        .collect();
    if connection.iter().any(|v| tokens(v, "close")) {
        return false;
    }
    head.starts_with(b"HTTP/1.1") || connection.iter().any(|v| tokens(v, "keep-alive"))
}

/// The `Set-Cookie` header values in a head block, in received order.
fn set_cookies<'a>(head: &'a [u8], spans: &[(Range<usize>, Range<usize>)]) -> Vec<&'a str> {
    spans
        .iter()
        .filter(|(n, _)| head[n.clone()].eq_ignore_ascii_case(b"set-cookie"))
        .map(|(_, v)| std::str::from_utf8(&head[v.clone()]).unwrap_or(""))
        .collect()
}

/// A streaming response: the head is available immediately, and the body is pulled a
/// chunk at a time with [`chunk`](Self::chunk) so memory stays O(chunk), not O(body) —
/// for large downloads, proxying, or Server-Sent Events. Get one from
/// [`HttpClient::send_streaming`]/[`get_streaming`](HttpClient::get_streaming).
///
/// The streaming path requests `Accept-Encoding: identity`, so the delivered bytes are
/// the raw body — it does **not** transparently inflate gzip/deflate/br (that decoder is
/// whole-buffer only). Use the buffered [`HttpClient::send`] if you want that.
///
/// When the body is fully drained the connection returns to the pool (if keep-alive);
/// dropping the stream before it is drained simply closes the connection — a half-read
/// keep-alive connection cannot be reused.
pub struct ResponseStream {
    status: u16,
    head: Vec<u8>,
    spans: Vec<(Range<usize>, Range<usize>)>,
    /// The live connection while the body is being read; `None` once drained/returned.
    conn: Option<Conn>,
    /// Raw bytes read from the socket but not yet delivered/decoded.
    buf: Vec<u8>,
    plan: StreamPlan,
    pool: Arc<ConnPool>,
    origin: Origin,
    max_idle_per_host: usize,
    close: bool,
    /// Per-read idle bound: no bytes within this long → [`HttpError::Timeout`].
    idle_timeout: Option<Duration>,
    /// The cap [`read_to_end`](Self::read_to_end) applies.
    max_response_size: usize,
}

impl ResponseStream {
    /// The HTTP status code.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Whether the status is 2xx.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

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
            let str_of =
                |r: &Range<usize>| std::str::from_utf8(&self.head[r.clone()]).unwrap_or("");
            (str_of(n), str_of(v))
        })
    }

    /// The raw trailer block of a chunked body, once [`chunk`](Self::chunk) has
    /// returned `None` (empty otherwise).
    fn trailer_block(&self) -> &[u8] {
        match &self.plan {
            StreamPlan::Chunked(dec) => &dec.trailer,
            _ => &[],
        }
    }

    /// Read the next slice of the body, or `None` at the end. Memory stays O(chunk): only
    /// the bytes needed to produce the next slice are held. On the final slice the
    /// connection is returned to the pool (framed body, keep-alive) or closed.
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>, HttpError> {
        // What a single pass over the buffered bytes produced.
        enum Step {
            Yield(Vec<u8>),
            Done(bool), // keepable: the body was framed, connection reusable
            NeedMore(&'static str),
        }

        loop {
            if self.conn.is_none() {
                return Ok(None);
            }

            let step = match self.plan {
                StreamPlan::Length(ref mut rem) => {
                    if *rem == 0 {
                        // Bytes past the declared length: the connection is out of step.
                        Step::Done(self.buf.is_empty())
                    } else if !self.buf.is_empty() {
                        let take = (*rem).min(self.buf.len());
                        *rem -= take;
                        Step::Yield(self.buf.drain(..take).collect())
                    } else {
                        Step::NeedMore("connection closed mid-body")
                    }
                }
                StreamPlan::Chunked(ref mut dec) => {
                    let mut out = Vec::new();
                    dec.pull(&mut self.buf, &mut out)?;
                    if !out.is_empty() {
                        Step::Yield(out)
                    } else if dec.is_done() {
                        Step::Done(self.buf.is_empty())
                    } else {
                        Step::NeedMore("connection closed mid-chunk")
                    }
                }
                StreamPlan::UntilClose => {
                    if !self.buf.is_empty() {
                        Step::Yield(std::mem::take(&mut self.buf))
                    } else {
                        Step::NeedMore("") // an EOF here is a clean end, not an error
                    }
                }
            };

            match step {
                Step::Yield(out) => return Ok(Some(out)),
                Step::Done(keepable) => {
                    self.finish(keepable);
                    return Ok(None);
                }
                Step::NeedMore(mid_err) => {
                    if !self.fill().await? {
                        if matches!(self.plan, StreamPlan::UntilClose) {
                            self.finish(false); // read-to-close: EOF is the end
                            return Ok(None);
                        }
                        return Err(HttpError::Protocol(mid_err.into()));
                    }
                }
            }
        }
    }

    /// Drain the rest of the body into a buffered [`Response`] (inflating any content
    /// encoding, for callers who set one). Convenience for "stream, then collect".
    pub async fn read_to_end(mut self) -> Result<Response, HttpError> {
        let max = self.max_response_size;
        let mut body = Vec::new();
        while let Some(part) = self.chunk().await? {
            if body.len() + part.len() > max {
                return Err(too_large(max));
            }
            body.extend_from_slice(&part);
        }
        let trailer = self.trailer_block().to_vec();
        let mut response = Response {
            status: self.status,
            body,
            head: std::mem::take(&mut self.head),
            spans: std::mem::take(&mut self.spans),
            trailer_spans: field_spans(&trailer),
            trailer,
        };
        compress::decompress(&mut response, max)?;
        Ok(response)
    }

    /// Read more bytes from the socket into `self.buf`, bounded by the idle timeout.
    /// `false` at EOF. Splits the field borrows so `conn` and `buf` can be used together.
    async fn fill(&mut self) -> Result<bool, HttpError> {
        let Self {
            conn,
            buf,
            idle_timeout,
            ..
        } = self;
        let conn = conn
            .as_mut()
            .ok_or_else(|| HttpError::Protocol("stream already finished".into()))?;
        let read = read_more_into(conn, buf);
        match idle_timeout {
            Some(limit) => match rt_core::timeout(*limit, read).await {
                Ok(result) => result,
                Err(_elapsed) => Err(HttpError::Timeout),
            },
            None => read.await,
        }
    }

    /// End the stream: return the connection to the pool when the body was framed and
    /// keep-alive is on, otherwise drop (close) it.
    fn finish(&mut self, keepable: bool) {
        if let Some(conn) = self.conn.take() {
            if self.max_idle_per_host > 0 && keepable && !self.close {
                self.pool
                    .put(std::mem::take(&mut self.origin), conn, now_ms());
            }
            // Otherwise `conn` drops here (closed). A partially-read connection is never
            // pooled — the leftover bytes would corrupt the next response on it.
        }
    }
}

/// Read the response head (status line + headers) up to and including the CRLFCRLF
/// terminator. Returns the status, the raw head block, the header spans, and any body
/// bytes that were read alongside the head (the leftover after the terminator).
///
/// Interim `1xx` responses (`100 Continue`, `103 Early Hints`, RFC 9110 §15.2) are read
/// past: the final response follows them on the same connection. `101 Switching
/// Protocols` is final. A head larger than [`MAX_HEAD_SIZE`] fails.
async fn read_head(
    stream: &mut Conn,
) -> Result<(u16, Vec<u8>, Vec<(Range<usize>, Range<usize>)>, Vec<u8>), HttpError> {
    let mut buf = Vec::with_capacity(4096);
    loop {
        // Read until the header terminator (CRLFCRLF) is in `buf`.
        let mut scanned: usize = 0;
        let head_end = loop {
            if let Some(pos) = find(&buf[scanned.saturating_sub(3)..], b"\r\n\r\n") {
                break scanned.saturating_sub(3) + pos + 4;
            }
            if buf.len() > MAX_HEAD_SIZE {
                return Err(HttpError::TooLarge(format!("response head over {MAX_HEAD_SIZE} bytes")));
            }
            scanned = buf.len();
            if !read_more(stream, &mut buf).await? {
                return Err(HttpError::Protocol(
                    "connection closed before headers completed".into(),
                ));
            }
        };
        if head_end > MAX_HEAD_SIZE {
            return Err(HttpError::TooLarge(format!("response head over {MAX_HEAD_SIZE} bytes")));
        }

        // Split without copying the head: `leftover` takes the tail (already-read body
        // bytes), `head_block` reuses buf.
        let leftover = buf.split_off(head_end);
        let head_block = buf;
        let (status, spans) = parse_head_spans(&head_block)?;
        if (100..200).contains(&status) && status != 101 {
            buf = leftover; // an interim response: the real one follows
            continue;
        }
        return Ok((status, head_block, spans, leftover));
    }
}

/// Read the whole response, following `Content-Length`, chunked, or read-to-close, and
/// holding at most `max` body bytes (before and after decompression). The returned bool
/// is whether the connection can carry another request: a framed body read exactly to
/// its end, on a connection that persists.
async fn read_response(stream: &mut Conn, head_request: bool, max: usize) -> Result<(Response, bool), HttpError> {
    let (status, head_block, spans, mut body) = read_head(stream).await?;
    let mut trailer = Vec::new();

    let framed_cleanly = match body_plan(&head_block, &spans, head_request, status)? {
        BodyPlan::Length(n) => {
            if n > max {
                return Err(too_large(max));
            }
            while body.len() < n {
                if !read_more_into(stream, &mut body).await? {
                    return Err(HttpError::Protocol("connection closed mid-body".into()));
                }
            }
            let extra = body.len() > n;
            body.truncate(n);
            !extra
        }
        BodyPlan::Chunked => {
            let mut dec = ChunkDecoder::new();
            let mut out = Vec::new();
            loop {
                dec.pull(&mut body, &mut out)?;
                if out.len() > max {
                    return Err(too_large(max));
                }
                if dec.is_done() {
                    break;
                }
                if !read_more_into(stream, &mut body).await? {
                    return Err(HttpError::Protocol("connection closed mid-chunk".into()));
                }
            }
            let extra = !body.is_empty();
            body = out;
            trailer = std::mem::take(&mut dec.trailer);
            !extra
        }
        BodyPlan::UntilClose => {
            while read_more_into(stream, &mut body).await? {
                if body.len() > max {
                    return Err(too_large(max));
                }
            }
            false // the connection was consumed to EOF — not reusable
        }
    };
    let keepable = framed_cleanly && persists(&head_block, &spans);

    let mut response = Response {
        status,
        body,
        head: head_block,
        spans,
        trailer_spans: field_spans(&trailer),
        trailer,
    };
    compress::decompress(&mut response, max)?; // inflate gzip/deflate/br transparently
    Ok((response, keepable))
}

fn too_large(max: usize) -> HttpError {
    HttpError::TooLarge(format!("body over the {max}-byte limit"))
}

/// Read one chunk from the connection into `buf`; `false` at EOF.
async fn read_more(stream: &mut Conn, buf: &mut Vec<u8>) -> Result<bool, HttpError> {
    read_more_into(stream, buf).await
}

/// The most one socket read takes (so one streamed slice is at most this).
const READ_SIZE: usize = 8192;

async fn read_more_into(stream: &mut Conn, buf: &mut Vec<u8>) -> Result<bool, HttpError> {
    let mut chunk = [0u8; READ_SIZE];
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

/// The header `(name-range, value-range)` pairs a parsed head yields.
type HeaderSpans = Vec<(Range<usize>, Range<usize>)>;

/// Parse the status code and the header `(name, value)` byte ranges — **no allocation
/// per header** and no whole-block UTF-8 scan (the status code is read from bytes; header
/// bytes are validated as UTF-8 only when accessed). This is the parser's hot path.
fn parse_head_spans(block: &[u8]) -> Result<(u16, HeaderSpans), HttpError> {
    let line_end =
        find(block, b"\r\n").ok_or_else(|| HttpError::Protocol("no status line".into()))?;
    if !block.starts_with(b"HTTP/1.") {
        return Err(HttpError::Protocol("not an HTTP/1.x response".into()));
    }
    let status = parse_status(&block[..line_end])?;
    Ok((status, field_spans(&block[line_end + 2..]).into_iter().map(|(n, v)| (shift(n, line_end + 2), shift(v, line_end + 2))).collect()))
}

fn shift(r: Range<usize>, by: usize) -> Range<usize> {
    r.start + by..r.end + by
}

/// The `(name, value)` ranges of the `Name: value` lines in `block`, up to the first
/// blank line. Lines without a colon are skipped.
fn field_spans(block: &[u8]) -> HeaderSpans {
    let mut spans = Vec::new();
    let mut pos = 0;
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
    spans
}

/// The code from a status line like `HTTP/1.1 200 OK` (the token after the version).
fn parse_status(line: &[u8]) -> Result<u16, HttpError> {
    let bad = || HttpError::Protocol("bad status line".into());
    let space = line.iter().position(|&b| b == b' ').ok_or_else(bad)?;
    let rest = &line[space + 1..];
    let end = rest.iter().position(|&b| b == b' ').unwrap_or(rest.len());
    let code = &rest[..end];
    if code.len() != 3 || !code.iter().all(u8::is_ascii_digit) {
        return Err(bad());
    }
    std::str::from_utf8(code)
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|c| (100..600).contains(c))
        .ok_or_else(bad)
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
#[cfg(test)]
fn header_bytes<'a>(
    block: &'a [u8],
    spans: &[(Range<usize>, Range<usize>)],
    name: &[u8],
) -> Option<&'a [u8]> {
    spans
        .iter()
        .find(|(n, _)| block[n.clone()].eq_ignore_ascii_case(name))
        .map(|(_, v)| &block[v.clone()])
}

/// Decide how to read the body (RFC 9112 §6.3). A response to HEAD, and a 1xx / 204 /
/// 304, has no body whatever its headers say — a HEAD answer's Content-Length is the
/// GET's. `Transfer-Encoding` wins over `Content-Length`: chunked when chunked is the
/// last coding, otherwise the body runs to the close. Several `Content-Length` values
/// must agree, and a malformed one is an error, not a guess — a client that framed the
/// body differently from the server would read the next response out of this one.
fn body_plan(block: &[u8], spans: &[(Range<usize>, Range<usize>)], head_request: bool, status: u16) -> Result<BodyPlan, HttpError> {
    if head_request || status / 100 == 1 || status == 204 || status == 304 {
        return Ok(BodyPlan::Length(0));
    }
    let values = |name: &'static [u8]| field_values(block, spans, name);
    if let Some(last) = values(b"transfer-encoding").filter(|v| !v.is_empty()).last() {
        return Ok(if last.eq_ignore_ascii_case("chunked") { BodyPlan::Chunked } else { BodyPlan::UntilClose });
    }
    let mut length = None;
    for v in values(b"content-length") {
        if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
            return Err(HttpError::Protocol(format!("bad Content-Length {v:?}")));
        }
        let n = v.parse::<usize>().map_err(|_| HttpError::Protocol(format!("Content-Length {v} too large")))?;
        if length.is_some_and(|l| l != n) {
            return Err(HttpError::Protocol("conflicting Content-Length values".into()));
        }
        length = Some(n);
    }
    Ok(length.map_or(BodyPlan::UntilClose, BodyPlan::Length))
}

/// Every comma-separated value of the header `name`, across all its lines, trimmed.
fn field_values<'a>(block: &'a [u8], spans: &'a [(Range<usize>, Range<usize>)], name: &'a [u8]) -> impl Iterator<Item = &'a str> + 'a {
    spans
        .iter()
        .filter(move |(n, _)| block[n.clone()].eq_ignore_ascii_case(name))
        .flat_map(move |(_, v)| block[v.clone()].split(|&b| b == b','))
        .map(|v| std::str::from_utf8(v).unwrap_or("\u{FFFD}").trim())
}

/// Decode a complete chunked body, or `None` if it is not yet complete (or malformed).
fn decode_chunked(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut dec = ChunkDecoder::new();
    let mut buf = bytes.to_vec();
    let mut out = Vec::new();
    dec.pull(&mut buf, &mut out).ok()?;
    dec.is_done().then_some(out)
}

/// The longest chunk-size line accepted (the size plus any extensions).
const MAX_CHUNK_LINE: usize = 4096;

/// Where an incremental chunked decode currently sits between reads.
enum ChunkState {
    /// Reading the `<hex>\r\n` chunk-size line.
    Size,
    /// Reading `n` more bytes of the current chunk's data.
    Data(usize),
    /// Expecting the CRLF that follows a chunk's data.
    DataCrlf,
    /// Past the terminating `0` chunk: trailer fields, then a blank line.
    Trailer,
    /// The terminating chunk and its trailer have been fully consumed.
    Done,
}

/// A stateful chunked-transfer decoder (RFC 9112 §7.1) that consumes bytes across many
/// reads. [`pull`](Self::pull) decodes as much of `buf` as it can, leaving any partial
/// tail for the next call; it never re-parses from the start. Trailer fields are kept in
/// [`trailer`](Self::trailer); a malformed size line or a missing CRLF after a chunk is an
/// error rather than a misread body.
struct ChunkDecoder {
    state: ChunkState,
    /// The trailer field lines (`Name: value\r\n`…) after the last chunk.
    trailer: Vec<u8>,
}

impl ChunkDecoder {
    fn new() -> Self {
        Self {
            state: ChunkState::Size,
            trailer: Vec::new(),
        }
    }

    /// Whether the terminating zero-length chunk and its trailer have been consumed.
    fn is_done(&self) -> bool {
        matches!(self.state, ChunkState::Done)
    }

    /// Consume as much of `buf` as possible, appending decoded body bytes to `out`.
    /// Unconsumed partial bytes are left at the front of `buf` for the next call.
    fn pull(&mut self, buf: &mut Vec<u8>, out: &mut Vec<u8>) -> Result<(), HttpError> {
        let bad = |what: &str| HttpError::Protocol(format!("malformed chunked body: {what}"));
        let mut pos = 0;
        let result = loop {
            match self.state {
                ChunkState::Done => break Ok(()),
                ChunkState::Size => match find(&buf[pos..], b"\r\n") {
                    Some(i) => {
                        let line = &buf[pos..pos + i];
                        let hex = line.split(|&b| b == b';').next().unwrap_or(b"");
                        let hex = std::str::from_utf8(hex).map(str::trim).unwrap_or("");
                        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                            break Err(bad("a bad chunk size"));
                        }
                        let Ok(size) = usize::from_str_radix(hex, 16) else {
                            break Err(bad("a chunk size too large"));
                        };
                        pos += i + 2;
                        self.state = if size == 0 { ChunkState::Trailer } else { ChunkState::Data(size) };
                    }
                    None if buf.len() - pos > MAX_CHUNK_LINE => break Err(bad("a chunk-size line too long")),
                    None => break Ok(()), // size line not fully arrived
                },
                ChunkState::Data(rem) => {
                    let avail = buf.len() - pos;
                    if avail == 0 {
                        break Ok(());
                    }
                    let take = rem.min(avail);
                    out.extend_from_slice(&buf[pos..pos + take]);
                    pos += take;
                    self.state = if take == rem { ChunkState::DataCrlf } else { ChunkState::Data(rem - take) };
                }
                ChunkState::DataCrlf => {
                    if buf.len() - pos < 2 {
                        break Ok(());
                    }
                    if &buf[pos..pos + 2] != b"\r\n" {
                        break Err(bad("no CRLF after a chunk"));
                    }
                    pos += 2;
                    self.state = ChunkState::Size;
                }
                ChunkState::Trailer => match find(&buf[pos..], b"\r\n") {
                    Some(0) => {
                        pos += 2; // the blank line ending the message
                        self.state = ChunkState::Done;
                    }
                    Some(i) => {
                        self.trailer.extend_from_slice(&buf[pos..pos + i + 2]);
                        pos += i + 2;
                        if self.trailer.len() > MAX_HEAD_SIZE {
                            break Err(HttpError::TooLarge(format!("trailer over {MAX_HEAD_SIZE} bytes")));
                        }
                    }
                    None if buf.len() - pos > MAX_HEAD_SIZE => {
                        break Err(HttpError::TooLarge(format!("trailer over {MAX_HEAD_SIZE} bytes")))
                    }
                    None => break Ok(()),
                },
            }
        };
        buf.drain(..pos);
        result
    }
}

/// Serialise a request to the wire, adding Host/Content-Length/Connection/User-Agent
/// (and `Cookie` when the jar supplied one). `keep_alive` picks `Connection:
/// keep-alive` (pooled) vs `close`.
fn encode_request(req: &Request, keep_alive: bool, cookie: Option<&str>) -> Vec<u8> {
    let mut out = format!(
        "{} {} HTTP/1.1\r\n",
        req.method.as_str(),
        req.url.path_and_query
    );

    let has = |name: &str| {
        req.headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case(name))
    };
    if !has("host") {
        // The port only when it is not the scheme default; IPv6 in brackets.
        out.push_str(&format!("Host: {}\r\n", req.url.authority()));
    }
    if let Some((user, password)) = &req.url.userinfo {
        if !has("authorization") {
            out.push_str(&format!("Authorization: Basic {}\r\n", base64(format!("{user}:{password}").as_bytes())));
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
    // A body, or a method that expects one (a bodiless POST still says so — some
    // servers answer 411 Length Required otherwise).
    let expects_body = matches!(req.method, Method::Post | Method::Put | Method::Patch);
    if (!req.body.is_empty() || expects_body) && !has("content-length") {
        out.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    }
    if !has("connection") {
        out.push_str(if keep_alive {
            "Connection: keep-alive\r\n"
        } else {
            "Connection: close\r\n"
        });
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
/// `Cookie`/`Proxy-Authorization` — or the URL's `user:password` — to another host).
fn redirect_target(req: &Request, response: &Response) -> Result<Option<Request>, HttpError> {
    if !matches!(response.status, 301 | 302 | 303 | 307 | 308) {
        return Ok(None);
    }
    let Some(location) = response.header("location") else {
        return Ok(None);
    };
    let mut url = resolve_location(&req.url, location)?;

    // 307/308 preserve the method + body; 301/302/303 become GET (HEAD stays HEAD) and
    // drop the body.
    let (method, body): (Method, Vec<u8>) = match response.status {
        307 | 308 => (req.method, req.body.clone()),
        _ if req.method == Method::Head => (Method::Head, Vec::new()),
        _ => (Method::Get, Vec::new()),
    };

    let same_origin = url.scheme == req.url.scheme
        && url.host.eq_ignore_ascii_case(&req.url.host)
        && url.port == req.url.port;
    if same_origin && url.userinfo.is_none() {
        url.userinfo.clone_from(&req.url.userinfo);
    }
    let dropped_body = body.is_empty() && !req.body.is_empty();

    let headers = req
        .headers
        .iter()
        .filter(|(name, _)| {
            let n = name.to_ascii_lowercase();
            // Never carry credentials across origins.
            if !same_origin && matches!(n.as_str(), "authorization" | "cookie" | "proxy-authorization") {
                return false;
            }
            // Body headers are stale once the body is dropped (they are recomputed).
            if dropped_body && (n == "content-type" || n == "content-length") {
                return false;
            }
            // The Host is the new URL's.
            n != "host"
        })
        .cloned()
        .collect();

    Ok(Some(Request {
        method,
        url,
        headers,
        body,
    }))
}

/// Resolve a `Location` value against the request URL (RFC 3986 §5.2): absolute,
/// scheme-relative (`//host/path`), absolute-path, query-only (`?q`), or a path relative
/// to the current directory — with `.` and `..` segments removed. The fragment is dropped.
fn resolve_location(base: &Url, location: &str) -> Result<Url, HttpError> {
    let loc = location.trim();
    let loc = loc.split('#').next().unwrap_or("");
    if let Some((scheme, _)) = loc.split_once("://") {
        if !scheme.is_empty() && scheme.bytes().all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b)) {
            return Url::parse(loc);
        }
    }
    if let Some(rest) = loc.strip_prefix("//") {
        return Url::parse(&format!("{}://{rest}", base.scheme));
    }
    let base_path = base.path_and_query.split('?').next().unwrap_or("/");
    let target = if loc.is_empty() {
        base.path_and_query.clone()
    } else if let Some(query) = loc.strip_prefix('?') {
        format!("{base_path}?{query}")
    } else {
        let (path, query) = match loc.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (loc, None),
        };
        let merged = if path.starts_with('/') {
            path.to_owned()
        } else {
            let dir = &base_path[..=base_path.rfind('/').unwrap_or(0)];
            format!("{dir}{path}")
        };
        let mut out = remove_dot_segments(&merged);
        if let Some(q) = query {
            out.push('?');
            out.push_str(q);
        }
        out
    };
    let mut url = Url::parse(&format!("{}://{}{}", base.scheme, base.authority(), target))?;
    url.userinfo.clone_from(&base.userinfo);
    Ok(url)
}

/// RFC 3986 §5.2.4: collapse `.` and `..` segments of an absolute path.
fn remove_dot_segments(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    for (i, seg) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        match *seg {
            "." => {
                if last {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
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
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
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
    parse_head_spans(head_bytes)
        .map(|(_, spans)| spans.len())
        .unwrap_or(0)
}

/// Benchmark hook — decode a chunked body, returning its length.
#[doc(hidden)]
pub fn bench_decode_chunked(bytes: &[u8]) -> usize {
    decode_chunked(bytes).map(|v| v.len()).unwrap_or(0)
}

/// Benchmark hook — decode a chunked body incrementally (the streaming decoder),
/// returning its length. Fed the whole buffer at once here, but O(consumed) either way.
#[doc(hidden)]
pub fn bench_chunk_decoder_incremental(bytes: &[u8]) -> usize {
    let mut dec = ChunkDecoder::new();
    let mut buf = bytes.to_vec();
    let mut out = Vec::new();
    let _ = dec.pull(&mut buf, &mut out);
    out.len()
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

    #[test]
    fn head_204_and_304_responses_have_no_body() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Length: 4096\r\nETag: \"x\"\r\n\r\n";
        let (status, spans) = parse_head_spans(head).unwrap();
        assert!(matches!(body_plan(head, &spans, true, status), Ok(BodyPlan::Length(0))), "HEAD: the GET's length, no body");
        assert!(matches!(body_plan(head, &spans, false, status), Ok(BodyPlan::Length(4096))));
        for (code, block) in [(204u16, &b"HTTP/1.1 204 No Content\r\nContent-Length: 10\r\n\r\n"[..]), (304, b"HTTP/1.1 304 Not Modified\r\nTransfer-Encoding: chunked\r\n\r\n")] {
            let (status, spans) = parse_head_spans(block).unwrap();
            assert_eq!(status, code);
            assert!(matches!(body_plan(block, &spans, false, status), Ok(BodyPlan::Length(0))), "{code}");
        }
    }
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

        assert_eq!(
            Url::parse("http://example.com").unwrap().path_and_query,
            "/",
            "no path → /"
        );
        assert!(Url::parse("ftp://x.com").is_err(), "unsupported scheme");
        assert!(Url::parse("not-a-url").is_err(), "no scheme");
    }

    #[test]
    fn encodes_a_request_with_default_headers() {
        let req = Request::new(
            Method::Post,
            Url::parse("http://api.example.com/v1/things").unwrap(),
        )
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
        let block =
            b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 3\r\n\r\n";
        let (status, spans) = parse_head_spans(block).unwrap();
        assert_eq!(status, 404);
        assert_eq!(spans.len(), 2);
        // The name keeps its on-wire case and matches case-insensitively; the value is
        // the exact byte slice, no allocation.
        let (name, value) = &spans[0];
        assert!(block[name.clone()].eq_ignore_ascii_case(b"content-type"));
        assert_eq!(&block[value.clone()], b"text/plain");
        assert_eq!(
            header_bytes(block, &spans, b"content-length"),
            Some(b"3".as_slice())
        );
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
        let body = String::from_utf8(encode_form(&[
            ("code", "a b/c"),
            ("grant_type", "authorization_code"),
        ]))
        .unwrap();
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
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
                .unwrap();
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

    /// Spawn a one-shot loopback server that replies with `response` bytes to the first
    /// connection. Returns the port and the join handle.
    fn one_shot(response: Vec<u8>) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).unwrap();
            sock.write_all(&response).unwrap();
        });
        (port, handle)
    }

    #[test]
    fn streams_a_content_length_body_in_chunks() {
        // A 2 MiB framed body — far larger than the 4 KiB socket read.
        let payload = vec![b'z'; 2 * 1024 * 1024];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        )
        .into_bytes();
        response.extend_from_slice(&payload);
        let (port, server) = one_shot(response);

        let received = rt_core::Executor::new()
            .unwrap()
            .block_on(async {
                let mut stream = HttpClient::new()
                    .get_streaming(&format!("http://127.0.0.1:{port}/big"))
                    .await?;
                assert_eq!(stream.status(), 200);
                let mut body = Vec::new();
                while let Some(part) = stream.chunk().await? {
                    // Each delivered slice is bounded by the socket read size, NOT the
                    // 2 MiB total — this is the O(chunk) memory guarantee.
                    assert!(
                        part.len() <= READ_SIZE,
                        "slice {} exceeds the read size",
                        part.len()
                    );
                    body.extend_from_slice(&part);
                }
                Ok::<_, HttpError>(body)
            })
            .unwrap()
            .unwrap();

        assert_eq!(received.len(), payload.len());
        assert!(received.iter().all(|&b| b == b'z'));
        server.join().unwrap();
    }

    #[test]
    fn streams_a_chunked_body_incrementally() {
        // Three chunks + terminator, delivered as one write; the incremental decoder must
        // reassemble "HelloWorld!!".
        let body = "5\r\nHello\r\n5\r\nWorld\r\n2\r\n!!\r\n0\r\n\r\n";
        let response =
            format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{body}").into_bytes();
        let (port, server) = one_shot(response);

        let text = rt_core::Executor::new()
            .unwrap()
            .block_on(async {
                let mut stream = HttpClient::new()
                    .get_streaming(&format!("http://127.0.0.1:{port}/c"))
                    .await?;
                let mut out = Vec::new();
                while let Some(part) = stream.chunk().await? {
                    out.extend_from_slice(&part);
                }
                Ok::<_, HttpError>(String::from_utf8(out).unwrap())
            })
            .unwrap()
            .unwrap();

        assert_eq!(text, "HelloWorld!!");
        server.join().unwrap();
    }

    #[test]
    fn streaming_memory_stays_bounded() {
        // Stream 8 MiB and assert the internal buffer never grows toward the body size —
        // it stays within a couple of socket reads, independent of the total.
        let payload = vec![b'x'; 8 * 1024 * 1024];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        )
        .into_bytes();
        response.extend_from_slice(&payload);
        let (port, server) = one_shot(response);

        let (total, peak_buf) = rt_core::Executor::new()
            .unwrap()
            .block_on(async {
                let mut stream = HttpClient::new()
                    .get_streaming(&format!("http://127.0.0.1:{port}/huge"))
                    .await?;
                let mut total = 0usize;
                let mut peak = stream.buf.len();
                while let Some(part) = stream.chunk().await? {
                    total += part.len();
                    peak = peak.max(stream.buf.len());
                }
                Ok::<_, HttpError>((total, peak))
            })
            .unwrap()
            .unwrap();

        assert_eq!(total, payload.len(), "streamed the whole body");
        assert!(
            peak_buf <= 8192,
            "internal buffer stayed O(chunk): peak was {peak_buf} bytes for an 8 MiB body"
        );
        server.join().unwrap();
    }

    #[test]
    fn streaming_returns_connection_to_pool_after_drain() {
        // One server connection serves two requests: the pooled connection is reused only
        // if the first stream returned it after being fully drained.
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap(); // exactly ONE connection
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf).unwrap();
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc")
                .unwrap();
            let _ = sock.read(&mut buf).unwrap(); // second request on the same connection
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\ndef")
                .unwrap();
        });

        let second = rt_core::Executor::new()
            .unwrap()
            .block_on(async {
                let client = HttpClient::new();
                let url = format!("http://127.0.0.1:{port}/x");
                let mut s1 = client.get_streaming(&url).await?;
                let mut b1 = Vec::new();
                while let Some(p) = s1.chunk().await? {
                    b1.extend_from_slice(&p);
                }
                assert_eq!(b1, b"abc");
                // Reuses the pooled connection — the server only ever accepts one.
                let s2 = client.get_streaming(&url).await?;
                s2.read_to_end().await
            })
            .unwrap()
            .unwrap();

        assert_eq!(second.text(), "def");
        server.join().unwrap();
    }

    #[test]
    fn dropping_stream_early_does_not_pool() {
        // A half-read stream must NOT return its connection to the pool.
        let payload = vec![b'q'; 64 * 1024];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            payload.len()
        )
        .into_bytes();
        response.extend_from_slice(&payload);
        let (port, server) = one_shot(response);

        let client = HttpClient::new();
        rt_core::Executor::new()
            .unwrap()
            .block_on(async {
                let mut stream = client
                    .get_streaming(&format!("http://127.0.0.1:{port}/p"))
                    .await?;
                let _first = stream.chunk().await?.unwrap(); // read one slice, then drop
                drop(stream);
                Ok::<_, HttpError>(())
            })
            .unwrap()
            .unwrap();

        let pooled: usize = client
            .pool
            .idle
            .lock()
            .unwrap()
            .values()
            .map(Vec::len)
            .sum();
        assert_eq!(pooled, 0, "a partially-read connection must not be pooled");
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
        Response {
            status,
            body: Vec::new(),
            head,
            spans,
            trailer: Vec::new(),
            trailer_spans: Vec::new(),
        }
    }

    #[test]
    fn redirect_applies_method_rules_and_drops_credentials_across_origins() {
        let req = Request::new(Method::Post, Url::parse("https://a.example/login").unwrap())
            .header("authorization", "Bearer secret")
            .body("application/json", b"{}".to_vec());

        // 302 to a DIFFERENT origin → GET, body + credentials dropped.
        let cross = redirect_target(
            &req,
            &mk_response(302, &[("location", "https://b.evil/cb")]),
        )
        .unwrap()
        .unwrap();
        assert_eq!(cross.url.host, "b.evil");
        assert_eq!(cross.method, Method::Get, "302 on POST becomes GET");
        assert!(cross.body.is_empty(), "body dropped");
        assert!(
            !cross
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("authorization")),
            "Authorization must not leak to another origin"
        );

        // 307 to the SAME origin → method + body + credentials preserved.
        let same = redirect_target(&req, &mk_response(307, &[("location", "/next")]))
            .unwrap()
            .unwrap();
        assert_eq!(same.url.host, "a.example");
        assert_eq!(same.url.path_and_query, "/next");
        assert_eq!(same.method, Method::Post, "307 preserves the method");
        assert_eq!(same.body, b"{}", "307 preserves the body");
        assert!(
            same.headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("authorization")),
            "auth kept same-origin"
        );

        // A non-redirect status → no follow.
        assert!(redirect_target(&req, &mk_response(200, &[]))
            .unwrap()
            .is_none());
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
            sock.write_all(b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            let n = sock.read(&mut buf).unwrap(); // /final, same connection
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\ndone")
                .unwrap();
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
        assert!(
            followed.starts_with("GET /final "),
            "followed to /final: {followed}"
        );
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
            sock.write_all(
                b"HTTP/1.1 200 OK\r\nSet-Cookie: sid=abc123; Path=/\r\nContent-Length: 0\r\n\r\n",
            )
            .unwrap();
            let n = sock.read(&mut buf).unwrap();
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
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
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
                gz.len()
            );
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
        assert_eq!(
            resp.header("content-encoding"),
            None,
            "the stale encoding header is stripped"
        );
        let request = server.join().unwrap();
        assert!(
            request.to_lowercase().contains("accept-encoding: gzip"),
            "we advertised gzip: {request}"
        );
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
                sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .unwrap();
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
        assert_eq!(
            r2.unwrap().status,
            200,
            "second request reused the pooled connection"
        );
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
        assert!(
            matches!(err, HttpError::Timeout),
            "a hung server must time out, got {err:?}"
        );
        server.join().unwrap();
    }

    // ── resolve() routing — no network for these ────────────────────────────
    // Verify the fast paths short-circuit before any DNS/getaddrinfo call.

    fn resolve_now(host: &str, port: u16) -> Result<SocketAddr, HttpError> {
        rt_core::Executor::new()
            .unwrap()
            .block_on(async move { resolve_all(host, port).await })
            .unwrap()
            .map(|addrs| addrs[0])
    }

    #[test]
    fn ipv4_literal_resolves_without_a_lookup() {
        let addr = resolve_now("127.0.0.1", 8080).unwrap();
        assert_eq!(addr, SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080));
    }

    #[test]
    fn ipv6_literal_resolves_without_a_lookup() {
        let addr = resolve_now("::1", 443).unwrap();
        assert!(addr.ip().is_loopback());
        assert_eq!(addr.port(), 443);
    }

    #[test]
    fn localhost_resolves_to_loopback_without_network() {
        let addr = resolve_now("localhost", 80).unwrap();
        assert!(addr.ip().is_loopback(), "localhost must be loopback, got {addr}");
        assert_eq!(addr.port(), 80);
    }

    #[test]
    fn a_trailing_dot_is_stripped_before_resolving() {
        let addr = resolve_now("127.0.0.1.", 1).unwrap();
        assert_eq!(addr, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1));
    }

    // ── URL, request and framing rules ─────────────────────────────────────

    #[test]
    fn urls_with_ipv6_userinfo_queries_and_fragments() {
        let u = Url::parse("http://[::1]:8080/a?b=1#frag").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path_and_query.as_str()), ("::1", 8080, "/a?b=1"));
        assert_eq!(u.authority(), "[::1]:8080");
        assert_eq!(Url::parse("https://[2001:db8::1]/").unwrap().authority(), "[2001:db8::1]");

        let u = Url::parse("https://us%40er:p%3Ass@Example.COM/x").unwrap();
        assert_eq!(u.userinfo, Some(("us@er".into(), "p:ss".into())));
        assert_eq!(u.host, "example.com", "hosts are case-insensitive");

        assert_eq!(Url::parse("http://h?x=1").unwrap().path_and_query, "/?x=1", "a query with no path");
        assert_eq!(Url::parse("http://h:8080").unwrap().port, 8080);
        assert_eq!(Url::parse("http://h:").unwrap().port, 80, "an empty port is the default");
        assert_eq!(
            Url::parse("http://h/tìm kiếm?q=a b").unwrap().path_and_query,
            "/t%C3%ACm%20ki%E1%BA%BFm?q=a%20b",
            "spaces and non-ASCII are percent-encoded, existing escapes kept"
        );
        assert_eq!(Url::parse("http://h/%41").unwrap().path_and_query, "/%41");

        for bad in [
            "http://h/x\r\nX-Injected: 1",
            "http://h/\0",
            "http://[::1/",
            "http://[::zz]/",
            "http://a:b:c/",
            "http://bad host/",
            "http://hôst.vn/",
            "http:///path",
            "http://h:99999/",
        ] {
            assert!(Url::parse(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn header_values_and_names_cannot_split_the_request() {
        let url = Url::parse("http://h/").unwrap();
        for (name, value) in [("x-a", "1\r\nX-Injected: 2"), ("x-a", "1\nGET / HTTP/1.1"), ("x-a", "a\0b"), ("bad name", "v"), ("x:y", "v"), ("", "v")] {
            let req = Request::new(Method::Get, url.clone()).header(name, value);
            assert!(matches!(req.check(), Err(HttpError::InvalidRequest(_))), "{name:?}: {value:?}");
        }
        assert!(Request::new(Method::Get, url).header("x-ok", "tab\tand ünïcode").check().is_ok());
    }

    #[test]
    fn basic_and_bearer_auth_and_url_credentials() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"Aladdin:open sesame"), "QWxhZGRpbjpvcGVuIHNlc2FtZQ==", "RFC 7617 §2");

        let url = Url::parse("http://h/").unwrap();
        let wire = |r: Request| String::from_utf8(encode_request(&r, true, None)).unwrap();
        assert!(wire(Request::new(Method::Get, url.clone()).basic_auth("Aladdin", "open sesame")).contains("authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==\r\n"));
        assert!(wire(Request::new(Method::Get, url).bearer_auth("t0k")).contains("authorization: Bearer t0k\r\n"));
        let creds = Request::new(Method::Get, Url::parse("http://Aladdin:open%20sesame@h/").unwrap());
        assert!(wire(creds).contains("Authorization: Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==\r\n"));
        // An explicit Authorization wins over the URL's.
        let both = Request::new(Method::Get, Url::parse("http://u:p@h/").unwrap()).bearer_auth("x");
        assert_eq!(wire(both).matches("uthorization").count(), 1);
    }

    #[test]
    fn a_bodiless_post_still_sends_its_length() {
        let url = Url::parse("http://h/").unwrap();
        let wire = |m| String::from_utf8(encode_request(&Request::new(m, url.clone()), true, None)).unwrap();
        assert!(wire(Method::Post).contains("Content-Length: 0\r\n"));
        assert!(wire(Method::Put).contains("Content-Length: 0\r\n"));
        assert!(!wire(Method::Get).contains("Content-Length"));
    }

    fn plan(head: &str) -> Result<BodyPlan, HttpError> {
        let block = head.as_bytes();
        let (status, spans) = parse_head_spans(block).unwrap();
        body_plan(block, &spans, false, status)
    }

    #[test]
    fn body_framing_follows_rfc_9112() {
        assert!(matches!(plan("HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n"), Ok(BodyPlan::Chunked)));
        assert!(matches!(plan("HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nTransfer-Encoding: CHUNKED\r\n\r\n"), Ok(BodyPlan::Chunked)));
        assert!(matches!(plan("HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip\r\nContent-Length: 5\r\n\r\n"), Ok(BodyPlan::UntilClose)), "chunked not last: read to the close");
        assert!(matches!(plan("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n"), Ok(BodyPlan::Chunked)), "TE overrides CL");
        assert!(matches!(plan("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Length: 5\r\n\r\n"), Ok(BodyPlan::Length(5))));
        assert!(matches!(plan("HTTP/1.1 200 OK\r\nContent-Length: 5, 5\r\n\r\n"), Ok(BodyPlan::Length(5))));
        for bad in ["Content-Length: 5\r\nContent-Length: 6", "Content-Length: 5, 6", "Content-Length: -1", "Content-Length: 0x10", "Content-Length: 99999999999999999999999"] {
            assert!(matches!(plan(&format!("HTTP/1.1 200 OK\r\n{bad}\r\n\r\n")), Err(HttpError::Protocol(_))), "{bad}");
        }
        assert!(matches!(plan("HTTP/1.1 200 OK\r\n\r\n"), Ok(BodyPlan::UntilClose)));
    }

    #[test]
    fn status_lines_are_checked() {
        assert!(parse_head_spans(b"HTTP/1.1 200\r\n\r\n").is_ok(), "the reason phrase is optional");
        assert!(parse_head_spans(b"HTTP/1.0 404 Not Found\r\n\r\n").is_ok());
        for bad in [&b"ICY 200 OK\r\n\r\n"[..], b"HTTP/1.1 20 OK\r\n\r\n", b"HTTP/1.1 2000 OK\r\n\r\n", b"HTTP/1.1 +20 OK\r\n\r\n", b"HTTP/1.1 099 X\r\n\r\n"] {
            assert!(parse_head_spans(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn connections_persist_per_version_and_connection_header() {
        let p = |h: &str| {
            let (_, spans) = parse_head_spans(h.as_bytes()).unwrap();
            persists(h.as_bytes(), &spans)
        };
        assert!(p("HTTP/1.1 200 OK\r\n\r\n"));
        assert!(!p("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n"));
        assert!(!p("HTTP/1.1 200 OK\r\nConnection: Upgrade, Close\r\n\r\n"));
        assert!(!p("HTTP/1.0 200 OK\r\n\r\n"), "HTTP/1.0 closes by default");
        assert!(p("HTTP/1.0 200 OK\r\nConnection: Keep-Alive\r\n\r\n"));
    }

    #[test]
    fn chunked_bodies_keep_trailers_and_refuse_malformed_framing() {
        let mut dec = ChunkDecoder::new();
        let mut buf = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\nX-Checksum: abc\r\nX-Other: 1\r\n\r\nHTTP/1.1".to_vec();
        let mut out = Vec::new();
        dec.pull(&mut buf, &mut out).unwrap();
        assert!(dec.is_done());
        assert_eq!(out, b"Wikipedia");
        assert_eq!(buf, b"HTTP/1.1", "the next response is left in place, not eaten");
        let spans = field_spans(&dec.trailer);
        assert_eq!(header_bytes(&dec.trailer, &spans, b"x-checksum"), Some(&b"abc"[..]));

        // Byte by byte gives the same result.
        let raw = b"3;ext=\"x\"\r\nabc\r\n0\r\nT: v\r\n\r\n";
        let mut dec = ChunkDecoder::new();
        let (mut buf, mut out) = (Vec::new(), Vec::new());
        for &b in raw {
            buf.push(b);
            dec.pull(&mut buf, &mut out).unwrap();
        }
        assert!(dec.is_done() && out == b"abc" && buf.is_empty());

        // Not done until the blank line after the last chunk.
        assert!(decode_chunked(b"0\r\n").is_none());
        assert_eq!(decode_chunked(b"0\r\n\r\n").unwrap(), b"");

        for bad in [&b"zz\r\nabc\r\n0\r\n\r\n"[..], b"3\r\nabcXX0\r\n\r\n", b"\r\nabc", b"-3\r\nabc\r\n", b"fffffffffffffffffffff\r\n"] {
            let mut dec = ChunkDecoder::new();
            assert!(dec.pull(&mut bad.to_vec(), &mut Vec::new()).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        let mut long = vec![b'1'; MAX_CHUNK_LINE + 10];
        assert!(ChunkDecoder::new().pull(&mut long, &mut Vec::new()).is_err(), "an endless size line");
    }

    #[test]
    fn locations_resolve_per_rfc_3986() {
        let base = Url::parse("https://u:p@a.example:8443/dir/page?x=1").unwrap();
        let r = |loc| {
            let u = resolve_location(&base, loc).unwrap();
            format!("{}://{}{}", u.scheme, u.authority(), u.path_and_query)
        };
        assert_eq!(r("/abs"), "https://a.example:8443/abs");
        assert_eq!(r("next"), "https://a.example:8443/dir/next");
        assert_eq!(r("../up?q=2"), "https://a.example:8443/up?q=2");
        assert_eq!(r("./same/../x"), "https://a.example:8443/dir/x");
        assert_eq!(r("?only=query"), "https://a.example:8443/dir/page?only=query");
        assert_eq!(r("//b.example/p"), "https://b.example/p", "scheme-relative keeps the scheme");
        assert_eq!(r("http://c.example/z#frag"), "http://c.example/z");
        assert_eq!(r("/../../etc"), "https://a.example:8443/etc", "cannot climb above the root");
        assert_eq!(resolve_location(&base, "/x").unwrap().userinfo, base.userinfo, "same origin keeps the URL's credentials");
    }

    #[test]
    fn redirects_drop_every_credential_across_origins() {
        let req = Request::new(Method::Get, Url::parse("https://u:p@a.example/").unwrap())
            .header("Authorization", "Bearer x")
            .header("Cookie", "s=1")
            .header("Proxy-Authorization", "Basic y")
            .header("Host", "a.example")
            .header("X-Keep", "1");
        let elsewhere = redirect_target(&req, &mk_response(302, &[("Location", "https://evil.example/")])).unwrap().unwrap();
        let names: Vec<String> = elsewhere.headers.iter().map(|(k, _)| k.to_ascii_lowercase()).collect();
        assert_eq!(names, ["x-keep"], "only the harmless header follows; the Host is the new URL's");
        assert_eq!(elsewhere.url.userinfo, None);
        let downgrade = redirect_target(&req, &mk_response(301, &[("Location", "http://a.example/")])).unwrap().unwrap();
        assert!(downgrade.headers.iter().all(|(k, _)| !k.eq_ignore_ascii_case("authorization")), "https → http is another origin");
        let same = redirect_target(&req, &mk_response(307, &[("Location", "/next")])).unwrap().unwrap();
        assert!(same.headers.iter().any(|(k, _)| k == "Authorization"));
        assert_eq!(same.url.userinfo, Some(("u".into(), "p".into())));
    }

    #[test]
    fn only_safe_requests_are_resent_on_a_dead_connection() {
        let get = Request::new(Method::Get, Url::parse("http://h/").unwrap());
        let post = Request::new(Method::Post, Url::parse("http://h/").unwrap());
        let keyed = Request::new(Method::Post, Url::parse("http://h/").unwrap()).header("Idempotency-Key", "k1");
        let eof = || HttpError::Protocol("connection closed before headers completed".into());
        let reset = || std::io::Error::from(ErrorKind::ConnectionReset);

        assert!(Failed::reading(eof()).retryable(&get));
        assert!(!Failed::reading(eof()).retryable(&post), "the server may have acted on the POST");
        assert!(Failed::reading(eof()).retryable(&keyed));
        assert!(Failed::writing(reset()).retryable(&post), "the POST never got out");
        assert!(!Failed::reading(HttpError::Timeout).retryable(&get), "a slow server is not a dead connection");
        assert!(!Failed::reading(HttpError::Protocol("bad status line".into())).retryable(&get));
    }
}
