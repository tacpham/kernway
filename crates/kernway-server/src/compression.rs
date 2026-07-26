//! Dynamic response compression (feature = `compression`) — the server half of the
//! shared [`kernway_compress`] layer (the client half decompresses).
//!
//! Selective, never blind. A response is compressed only when *all* hold, so CPU is
//! never wasted on payloads that will not shrink:
//! - the client's `Accept-Encoding` offers an encoding we support ([`negotiate`]),
//! - the `Content-Type` is worth compressing ([`is_compressible`] — text/JSON/…, not
//!   images/video/archives),
//! - the body is an in-memory `Body::Bytes` (a streamed `Body::File` is left to
//!   `kernway-static`'s precompressed `.br`/`.gz` variants), and
//! - it is at least [`min_size`](Compression::min_size) bytes (tiny bodies cost more
//!   in framing overhead than they save).
//!
//! A final guard drops the result if compression did not actually shrink it.
//!
//! [`negotiate`]: kernway_compress::negotiate
//! [`is_compressible`]: kernway_compress::is_compressible

use di_core::RequestScope;
use kernway_compress::{encode, is_compressible, negotiate, Encoding};
use kernway_core::layer::BoxFuture;
use kernway_core::request::Request;
use kernway_core::response::{Body, Response};

use crate::middleware::{Middleware, Next};

/// Below this body size, compression's framing overhead outweighs the saving.
pub const DEFAULT_MIN_SIZE: usize = 1024;

/// Compresses eligible responses, choosing `br`/`gzip`/`deflate` per the request's
/// `Accept-Encoding`. Place it near the top of the chain so it sees the final body.
pub struct Compression {
    min_size: usize,
}

impl Compression {
    /// A compressor with the default [`DEFAULT_MIN_SIZE`] threshold.
    #[must_use]
    pub fn new() -> Self {
        Self { min_size: DEFAULT_MIN_SIZE }
    }

    /// Only compress bodies at least `bytes` long.
    #[must_use]
    pub fn min_size(mut self, bytes: usize) -> Self {
        self.min_size = bytes;
        self
    }
}

impl Default for Compression {
    fn default() -> Self {
        Self::new()
    }
}

impl Middleware for Compression {
    fn name(&self) -> &'static str {
        "Compression"
    }

    fn handle<'a>(&'a self, req: Request, scope: &'a RequestScope, next: Next<'a>) -> BoxFuture<'a, Response> {
        let accept = req.header("accept-encoding").unwrap_or("").to_string();
        let min_size = self.min_size;
        Box::pin(async move {
            let mut response = next.run(req, scope).await;
            compress(&mut response, &accept, min_size);
            response
        })
    }
}

/// Compress `response` in place if it is eligible (see the module docs).
fn compress(response: &mut Response, accept: &str, min_size: usize) {
    // Only in-memory bodies; a streamed file is the static layer's concern.
    let Body::Bytes(bytes) = &response.body else {
        return;
    };
    if bytes.len() < min_size {
        return;
    }
    // Do not double-encode.
    if response.headers.get("content-encoding").is_some() {
        return;
    }
    if !is_compressible(response.headers.get("content-type").unwrap_or("")) {
        return;
    }
    let encoding = negotiate(accept);
    if encoding == Encoding::Identity {
        return;
    }

    let encoded = encode(bytes, encoding);
    if encoded.len() >= bytes.len() {
        return; // it did not actually help — keep the plain body
    }

    let length = encoded.len();
    response.body = Body::Bytes(encoded);
    response.headers.insert("content-encoding", encoding.as_str());
    response.headers.insert("content-length", &length.to_string());
    add_vary(response);
}

/// Ensure `Vary: Accept-Encoding` is present so shared caches key on the encoding.
fn add_vary(response: &mut Response) {
    match response.headers.get("vary").map(str::to_string) {
        Some(existing) if existing.to_ascii_lowercase().contains("accept-encoding") => {}
        Some(existing) => response.headers.insert("vary", &format!("{existing}, Accept-Encoding")),
        None => response.headers.insert("vary", "Accept-Encoding"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::Terminal;
    use di_core::AppContext;
    use kernway_core::error::StatusCode;
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    fn block_on<F: Future>(fut: F) -> F::Output {
        let mut fut = Box::pin(fut);
        let waker = Waker::noop();
        match fut.as_mut().poll(&mut Context::from_waker(waker)) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("resolves synchronously"),
        }
    }

    /// Run a request with the given Accept-Encoding through `Compression` to a handler
    /// that returns `body` with `content_type`.
    fn run(accept: &str, content_type: &'static str, body: Vec<u8>, min_size: usize) -> Response {
        let mut req = Request::new("GET", "/");
        if !accept.is_empty() {
            req.headers.insert("accept-encoding", accept);
        }
        let app = AppContext::new();
        let scope = RequestScope::new(&app);
        let terminal: &Terminal = &move |_req, _scope| {
            let body = body.clone();
            Box::pin(async move {
                Response::new(StatusCode::OK).content_type(content_type).body(body)
            }) as BoxFuture<'static, Response>
        };
        block_on(Compression::new().min_size(min_size).handle(req, &scope, Next { rest: &[], terminal }))
    }

    #[test]
    fn compresses_a_large_compressible_body() {
        let body = "hello world, this JSON is very repetitive. ".repeat(50).into_bytes();
        let resp = run("gzip, deflate, br", "application/json", body.clone(), 1024);
        assert_eq!(resp.headers.get("content-encoding"), Some("br"), "picks the best offered (br)");
        assert_eq!(resp.headers.get("vary"), Some("Accept-Encoding"));
        let encoded = resp.body_bytes().to_vec();
        assert!(encoded.len() < body.len(), "actually smaller");
        assert_eq!(resp.headers.get("content-length"), Some(encoded.len().to_string().as_str()));
        // And it decodes back to the original.
        assert_eq!(kernway_compress::decode(&encoded, Encoding::Br).unwrap(), body);
    }

    #[test]
    fn skips_small_bodies() {
        let resp = run("gzip", "text/plain", b"tiny".to_vec(), 1024);
        assert_eq!(resp.headers.get("content-encoding"), None, "below the threshold");
    }

    #[test]
    fn skips_incompressible_types() {
        let body = vec![0u8; 4096];
        let resp = run("gzip", "image/png", body, 1024);
        assert_eq!(resp.headers.get("content-encoding"), None, "images are already compressed");
    }

    #[test]
    fn skips_when_the_client_does_not_accept_it() {
        let body = "compressible text ".repeat(100).into_bytes();
        let resp = run("", "text/plain", body, 1024);
        assert_eq!(resp.headers.get("content-encoding"), None, "no Accept-Encoding → no compression");
    }

    #[test]
    fn honours_the_clients_offered_encodings() {
        let body = "compressible text ".repeat(100).into_bytes();
        // Only gzip offered → gzip, not br.
        let resp = run("gzip", "text/plain", body, 1024);
        assert_eq!(resp.headers.get("content-encoding"), Some("gzip"));
    }
}
