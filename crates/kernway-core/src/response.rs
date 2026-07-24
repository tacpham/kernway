//! HTTP Response abstraction.

use crate::error::StatusCode;
use std::collections::HashMap;
use std::path::PathBuf;

/// A response body: bytes in memory, or a file the connection task streams.
///
/// Per [KEP-0002]. The common case is `Bytes` — what every `IntoResponse`
/// produces. `File` lets a handler name a file instead of carrying its contents,
/// so a large download is not read whole into memory: the async connection task
/// reads it in bounded chunks off the blocking pool.
///
/// [KEP-0002]: https://github.com/tacpham/kernway/blob/main/docs/kep/0002-response-body.md
#[derive(Debug)]
pub enum Body {
    /// No body — a `HEAD` response, a `204`, a `304`.
    Empty,
    /// Bytes already in memory. What handlers and `IntoResponse` produce.
    Bytes(Vec<u8>),
    /// A file to stream. `len` is the full file size; `range`, when set, is the
    /// half-open byte interval `[start, end)` to send for a `206`.
    File {
        /// Path to the file, resolved and safety-checked by the caller.
        path: PathBuf,
        /// Full file length, for `Content-Length` and range arithmetic.
        len: u64,
        /// The byte interval to send, for a partial response; `None` sends all.
        range: Option<(u64, u64)>,
    },
}

impl Body {
    /// The number of bytes this body will write — the `Content-Length`.
    ///
    /// For a `File` with a range, that is the range width, not the file size.
    #[must_use]
    pub fn len(&self) -> u64 {
        match self {
            Body::Empty => 0,
            Body::Bytes(b) => b.len() as u64,
            Body::File { len, range, .. } => range.map_or(*len, |(s, e)| e - s),
        }
    }

    /// Whether the body writes no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Raw HTTP response — implementation-agnostic.
#[derive(Debug)]
pub struct Response {
    /// Status line code.
    pub status:  StatusCode,
    /// Response headers, written out verbatim.
    pub headers: HashMap<String, String>,
    /// Response body — bytes, a file, or empty.
    pub body:    Body,
}

impl Response {
    /// Create an empty response with a status code.
    pub fn new(status: StatusCode) -> Self {
        Self {
            status,
            headers: HashMap::new(),
            body: Body::Empty,
        }
    }

    /// Set the body to bytes in memory.
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = Body::Bytes(body.into());
        self
    }

    /// Set the body to a file the connection task will stream. `len` is the full
    /// file size; the read happens later, off the request path.
    pub fn file(mut self, path: impl Into<PathBuf>, len: u64) -> Self {
        self.body = Body::File { path: path.into(), len, range: None };
        self
    }

    /// The in-memory body bytes, or an empty slice for `Empty`/`File`.
    ///
    /// A convenience for reading a `Bytes` body (tests, middleware). A `File`
    /// body has no bytes in memory, so this returns `&[]` for it — use the
    /// `Body::File` fields to stream.
    #[must_use]
    pub fn body_bytes(&self) -> &[u8] {
        match &self.body {
            Body::Bytes(b) => b,
            Body::Empty | Body::File { .. } => &[],
        }
    }

    /// Set Content-Type header.
    pub fn content_type(mut self, ct: impl Into<String>) -> Self {
        self.headers.insert("content-type".to_owned(), ct.into());
        self
    }
}

/// Trait for converting any type into an HTTP Response.
///
/// Equivalent to `HttpMessageConverter` in Spring.
/// `Json<T>`, `Html<T>`, `StatusCode`, and `(StatusCode, Json<T>)` all implement this trait.
pub trait IntoResponse: Send {
    /// Convert self into a [`Response`].
    fn into_response(self) -> Response;
}

// --- Blanket implementations ---

impl IntoResponse for Response {
    fn into_response(self) -> Response {
        self
    }
}

impl IntoResponse for StatusCode {
    fn into_response(self) -> Response {
        Response::new(self)
    }
}

impl IntoResponse for &'static str {
    fn into_response(self) -> Response {
        Response::new(StatusCode::OK)
            .content_type("text/plain; charset=utf-8")
            .body(self.as_bytes().to_vec())
    }
}

impl IntoResponse for String {
    fn into_response(self) -> Response {
        Response::new(StatusCode::OK)
            .content_type("text/plain; charset=utf-8")
            .body(self.into_bytes())
    }
}

impl<T: IntoResponse, E: IntoResponse> IntoResponse for Result<T, E> {
    fn into_response(self) -> Response {
        match self {
            Ok(v)  => v.into_response(),
            Err(e) => e.into_response(),
        }
    }
}

/// Tuple (StatusCode, T) → response with a custom status.
impl<T: IntoResponse> IntoResponse for (StatusCode, T) {
    fn into_response(self) -> Response {
        let mut resp = self.1.into_response();
        resp.status = self.0;
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_new_has_empty_body_and_headers() {
        let r = Response::new(StatusCode::OK);
        assert_eq!(r.status, StatusCode::OK);
        assert!(r.body.is_empty());
        assert!(r.headers.is_empty());
    }

    #[test]
    fn response_body_builder() {
        let r = Response::new(StatusCode::OK).body(b"hello".to_vec());
        assert_eq!(r.body_bytes(), b"hello");
    }

    #[test]
    fn response_content_type_builder() {
        let r = Response::new(StatusCode::OK).content_type("text/html");
        assert_eq!(r.headers.get("content-type").unwrap(), "text/html");
    }

    #[test]
    fn into_response_static_str() {
        let r = "hello".into_response();
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.body_bytes(), b"hello");
        assert!(r.headers["content-type"].contains("text/plain"));
    }

    #[test]
    fn into_response_string() {
        let r = String::from("world").into_response();
        assert_eq!(r.body_bytes(), b"world");
    }

    #[test]
    fn into_response_status_code_only() {
        let r = StatusCode::NO_CONTENT.into_response();
        assert_eq!(r.status, StatusCode::NO_CONTENT);
        assert!(r.body.is_empty());
    }

    #[test]
    fn into_response_result_ok() {
        let r: Result<&'static str, StatusCode> = Ok("ok");
        let resp = r.into_response();
        assert_eq!(resp.status, StatusCode::OK);
    }

    #[test]
    fn into_response_result_err() {
        let r: Result<&'static str, StatusCode> = Err(StatusCode::NOT_FOUND);
        let resp = r.into_response();
        assert_eq!(resp.status, StatusCode::NOT_FOUND);
    }

    #[test]
    fn into_response_tuple_overrides_status() {
        let r = (StatusCode::CREATED, "resource created").into_response();
        assert_eq!(r.status, StatusCode::CREATED);
    }
}
