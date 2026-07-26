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

mod tls;

use std::fmt;
use std::net::ToSocketAddrs;
use std::sync::Arc;

use rt_net::AsyncTcpStream;

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
#[derive(Debug, Clone)]
pub struct Response {
    /// The HTTP status code.
    pub status: u16,
    /// The response headers, in order (names lowercased).
    pub headers: Vec<(String, String)>,
    /// The response body.
    pub body: Vec<u8>,
}

impl Response {
    /// The first value of header `name` (case-insensitive), if present.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
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
    /// TLS is required (an `https` URL) but not available in this build/cut.
    Tls(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::Url(m) => write!(f, "bad url: {m}"),
            HttpError::Dns(m) => write!(f, "dns: {m}"),
            HttpError::Io(e) => write!(f, "io: {e}"),
            HttpError::Protocol(m) => write!(f, "protocol: {m}"),
            HttpError::Tls(m) => write!(f, "tls: {m}"),
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
}

impl HttpClient {
    /// A new client with the default TLS config (verifies against Mozilla's roots).
    #[must_use]
    pub fn new() -> Self {
        Self { tls: tls::default_config() }
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
    /// otherwise.
    pub async fn send(&self, req: Request) -> Result<Response, HttpError> {
        let addr = resolve(&req.url.host, req.url.port)?;
        let tcp = AsyncTcpStream::connect(addr).await?;
        tcp.set_nodelay(true).ok();

        let mut conn = if req.url.is_tls() {
            let tls = tls::AsyncTlsStream::connect(tcp, self.tls.clone(), &req.url.host).await?;
            Conn::Tls(Box::new(tls))
        } else {
            Conn::Plain(tcp)
        };

        let raw = encode_request(&req);
        conn.write_all(&raw).await?;
        read_response(&mut conn).await
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

/// Read the whole response from a connection, following `Content-Length`, chunked, or
/// read-to-close.
async fn read_response(stream: &mut Conn) -> Result<Response, HttpError> {
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

    let head = parse_head(&buf[..head_end])?;
    let mut body = buf[head_end..].to_vec(); // any bytes already read past the headers

    match body_plan(&head.headers) {
        BodyPlan::Length(n) => {
            while body.len() < n {
                if !read_more_into(stream, &mut body).await? {
                    return Err(HttpError::Protocol("connection closed mid-body".into()));
                }
            }
            body.truncate(n);
        }
        BodyPlan::Chunked => {
            // Read until the chunked stream can be fully decoded.
            loop {
                if let Some(decoded) = decode_chunked(&body) {
                    body = decoded;
                    break;
                }
                if !read_more_into(stream, &mut body).await? {
                    return Err(HttpError::Protocol("connection closed mid-chunk".into()));
                }
            }
        }
        BodyPlan::UntilClose => {
            while read_more_into(stream, &mut body).await? {}
        }
    }

    Ok(Response { status: head.status, headers: head.headers, body })
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

// ── pure helpers (no I/O) — the reusable core, shared by the future TLS path ──

/// The parsed status line + headers.
struct Head {
    status: u16,
    headers: Vec<(String, String)>,
}

/// How to read the body.
enum BodyPlan {
    /// Exactly `n` bytes.
    Length(usize),
    /// `Transfer-Encoding: chunked`.
    Chunked,
    /// Until the connection closes.
    UntilClose,
}

/// Parse the status line and headers (given the bytes up to and including CRLFCRLF).
fn parse_head(head_bytes: &[u8]) -> Result<Head, HttpError> {
    let text = std::str::from_utf8(head_bytes).map_err(|_| HttpError::Protocol("non-UTF8 headers".into()))?;
    let mut lines = text.split("\r\n");

    let status_line = lines.next().ok_or_else(|| HttpError::Protocol("empty response".into()))?;
    // "HTTP/1.1 200 OK" → the middle token is the code.
    let mut parts = status_line.splitn(3, ' ');
    let _version = parts.next();
    let status = parts
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| HttpError::Protocol(format!("bad status line {status_line:?}")))?;

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break; // the blank line before the body
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    Ok(Head { status, headers })
}

/// Decide how to read the body from the response headers.
fn body_plan(headers: &[(String, String)]) -> BodyPlan {
    let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
    if header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        return BodyPlan::Chunked;
    }
    if let Some(len) = header("content-length").and_then(|v| v.trim().parse::<usize>().ok()) {
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

/// Serialise a request to the wire, adding Host/Content-Length/Connection/User-Agent.
fn encode_request(req: &Request) -> Vec<u8> {
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
    for (name, value) in &req.headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if !req.body.is_empty() && !has("content-length") {
        out.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    }
    // One request per connection for now — simplest, and fine for OAuth-style volumes.
    if !has("connection") {
        out.push_str("Connection: close\r\n");
    }
    out.push_str("\r\n");

    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&req.body);
    bytes
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
    encode_request(req)
}

/// Benchmark hook — parse a response head, returning the header count.
#[doc(hidden)]
pub fn bench_parse_head(head_bytes: &[u8]) -> usize {
    parse_head(head_bytes).map(|h| h.headers.len()).unwrap_or(0)
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
        let wire = String::from_utf8(encode_request(&req)).unwrap();
        assert!(wire.starts_with("POST /v1/things HTTP/1.1\r\n"));
        assert!(wire.contains("Host: api.example.com\r\n"));
        // Custom headers are emitted as stored (lowercased); wire header names are
        // case-insensitive, so this is valid.
        assert!(wire.contains("content-type: application/json\r\n"));
        assert!(wire.contains("Content-Length: 2\r\n"));
        assert!(wire.contains("Connection: close\r\n"));
        assert!(wire.ends_with("\r\n\r\n{}"));
    }

    #[test]
    fn parses_a_response_head() {
        let head = parse_head(b"HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 3\r\n\r\n").unwrap();
        assert_eq!(head.status, 404);
        assert_eq!(head.headers.len(), 2);
        assert_eq!(head.headers[0], ("content-type".to_string(), "text/plain".to_string()));
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
