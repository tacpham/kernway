//! HTTP Response abstraction.

use crate::error::StatusCode;
use std::collections::HashMap;

/// Raw HTTP response — implementation-agnostic.
#[derive(Debug)]
pub struct Response {
    pub status:  StatusCode,
    pub headers: HashMap<String, String>,
    pub body:    Vec<u8>,
}

impl Response {
    /// Create an empty response with a status code.
    pub fn new(status: StatusCode) -> Self {
        Self {
            status,
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    /// Set body bytes.
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
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
        assert_eq!(r.body, b"hello");
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
        assert_eq!(r.body, b"hello");
        assert!(r.headers["content-type"].contains("text/plain"));
    }

    #[test]
    fn into_response_string() {
        let r = String::from("world").into_response();
        assert_eq!(r.body, b"world");
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
