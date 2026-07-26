//! HTTP Request abstraction + FromRequest trait.

use crate::error::KernwayError;
use crate::fields::{Headers, QueryParams};
use std::collections::HashMap;

/// HTTP protocol version of a request.
///
/// Only the two versions the framework speaks are modelled. The distinction is
/// not cosmetic: it decides the default connection behaviour — HTTP/1.1 keeps
/// the connection alive unless asked to close, HTTP/1.0 closes unless asked to
/// keep it. Anything that is not exactly `HTTP/1.0` is treated as 1.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HttpVersion {
    /// `HTTP/1.0` — connection closes by default.
    Http10,
    /// `HTTP/1.1` — connection is persistent by default (RFC 9112 §9.3).
    #[default]
    Http11,
}

impl std::fmt::Display for HttpVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            HttpVersion::Http10 => "HTTP/1.0",
            HttpVersion::Http11 => "HTTP/1.1",
        })
    }
}

/// Raw HTTP request — implementation-agnostic.
#[derive(Debug)]
pub struct Request {
    /// HTTP method, uppercase as it arrived on the wire (`GET`, `POST`, ...).
    pub method:      String,
    /// Request path, without the query string (`/users/42`).
    pub path:        String,
    /// Protocol version — decides the default keep-alive behaviour.
    pub version:     HttpVersion,
    /// Request headers. Names compare case-insensitively, per RFC 9110 §5.1.
    pub headers:     Headers,
    /// Parsed query string. Names are case-sensitive, unlike headers.
    pub query:       QueryParams,
    /// Values captured from the route pattern — `/users/{id}` yields `id`.
    ///
    /// Populated by the router after a route matches, so it is empty on a
    /// hand-built `Request`.
    pub path_params: HashMap<String, String>,
    /// Raw request body. Left empty when the request carries none.
    pub body:        Vec<u8>,
}

impl Request {
    /// Create a new request (for testing). Defaults to HTTP/1.1.
    pub fn new(method: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            method:      method.into(),
            path:        path.into(),
            version:     HttpVersion::default(),
            headers:     Headers::new(),
            query:       QueryParams::new(),
            path_params: HashMap::new(),
            body:        Vec::new(),
        }
    }

    /// Whether the connection should stay open after this request.
    ///
    /// Follows RFC 9112 §9.3: the `connection` header wins, and the version
    /// supplies the default when the header is absent.
    pub fn wants_keep_alive(&self) -> bool {
        // Compared in place rather than through `to_ascii_lowercase`: this runs
        // once per request and the old form allocated a whole copy of the value
        // to answer a question about two known tokens.
        let has = |value: &str, token: &str| {
            value
                .split(',')
                .any(|v| v.trim().eq_ignore_ascii_case(token))
        };
        match self.header("connection") {
            Some(value) if has(value, "close") => false,
            Some(value) if has(value, "keep-alive") => true,
            _ => self.version == HttpVersion::Http11,
        }
    }

    /// Get a header value. Matching is ASCII case-insensitive.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)
    }
}

/// Trait for extracting data from an HTTP request into a Rust type.
///
// Argument resolution is `kernway_server::Extract` (which also sees the request
// scope), and the extractors' own `from_request`. An early `FromRequest` trait
// lived here (0 impls) and was removed as dead in favour of those.

// --- Error rejection helper ---

/// Simple rejection that wraps KernwayError.
pub struct Rejection(pub KernwayError);

impl crate::response::IntoResponse for Rejection {
    fn into_response(self) -> crate::response::Response {
        use crate::error::StatusCode;
        crate::response::Response::new(StatusCode::BAD_REQUEST)
            .content_type("text/plain")
            .body(self.0.to_string().into_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_header(version: HttpVersion, value: &str) -> Request {
        let mut req = Request::new("GET", "/");
        req.version = version;
        req.headers.insert("connection", value);
        req
    }

    #[test]
    fn http11_keeps_the_connection_by_default() {
        assert!(Request::new("GET", "/").wants_keep_alive());
    }

    #[test]
    fn http10_closes_by_default() {
        let mut req = Request::new("GET", "/");
        req.version = HttpVersion::Http10;
        assert!(!req.wants_keep_alive());
    }

    #[test]
    fn the_connection_header_overrides_the_version_default() {
        assert!(!with_header(HttpVersion::Http11, "close").wants_keep_alive());
        assert!(with_header(HttpVersion::Http10, "keep-alive").wants_keep_alive());
    }

    #[test]
    fn the_connection_header_is_case_insensitive() {
        assert!(!with_header(HttpVersion::Http11, "Close").wants_keep_alive());
        assert!(with_header(HttpVersion::Http10, "Keep-Alive").wants_keep_alive());
    }

    #[test]
    fn close_wins_inside_a_comma_separated_list() {
        // Proxies routinely send `connection: keep-alive, close` on the last hop.
        assert!(!with_header(HttpVersion::Http11, "keep-alive, close").wants_keep_alive());
    }

    #[test]
    fn version_renders_back_to_the_wire_form() {
        assert_eq!(HttpVersion::Http10.to_string(), "HTTP/1.0");
        assert_eq!(HttpVersion::Http11.to_string(), "HTTP/1.1");
    }
}
