//! Error types for kernway-core.

use thiserror::Error;

/// Common framework error.
#[derive(Debug, Error)]
pub enum KernwayError {
    /// Error parsing or extracting a request.
    #[error("bad request: {0}")]
    BadRequest(String),

    /// Internal framework error.
    #[error("internal error: {0}")]
    Internal(String),

    /// Error from a layer (middleware).
    #[error("layer error: {0}")]
    Layer(String),
}

/// HTTP status code — a subset sufficient for the framework.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusCode(pub u16);

impl StatusCode {
    /// `200 OK` — the request succeeded.
    pub const OK: Self = Self(200);
    /// `201 Created` — a new resource was created; set `Location` alongside it.
    pub const CREATED: Self = Self(201);
    /// `204 No Content` — success, and deliberately no body.
    pub const NO_CONTENT: Self = Self(204);
    /// `206 Partial Content` — a byte range of the resource; `Content-Range` set.
    pub const PARTIAL_CONTENT: Self = Self(206);
    /// `304 Not Modified` — the client's cached copy is current; no body sent.
    /// Answered to a conditional request whose `If-None-Match` matches.
    pub const NOT_MODIFIED: Self = Self(304);
    /// `400 Bad Request` — the request was malformed.
    pub const BAD_REQUEST: Self = Self(400);
    /// `401 Unauthorized` — no or invalid credentials (authentication).
    pub const UNAUTHORIZED: Self = Self(401);
    /// `403 Forbidden` — authenticated, but not allowed (authorisation).
    pub const FORBIDDEN: Self = Self(403);
    /// `404 Not Found` — no route matched, or the resource does not exist.
    pub const NOT_FOUND: Self = Self(404);
    /// `405 Method Not Allowed` — the path exists, the method does not.
    pub const METHOD_NOT_ALLOWED: Self = Self(405);
    /// `413 Payload Too Large` — the request body exceeds the configured maximum
    /// (`max_upload_size`); the server refuses rather than spool it to disk.
    pub const PAYLOAD_TOO_LARGE: Self = Self(413);
    /// `422 Unprocessable Entity` — well-formed but semantically invalid;
    /// what validation failures return.
    pub const UNPROCESSABLE_ENTITY: Self = Self(422);
    /// `416 Range Not Satisfiable` — the requested byte range lies outside the
    /// resource; `Content-Range: bytes */len` states the actual length.
    pub const RANGE_NOT_SATISFIABLE: Self = Self(416);
    /// `429 Too Many Requests` — rate limit exceeded.
    pub const TOO_MANY_REQUESTS: Self = Self(429);
    /// `500 Internal Server Error` — an unhandled failure on the server.
    pub const INTERNAL_SERVER_ERROR: Self = Self(500);
    /// `503 Service Unavailable` — temporarily unable to serve; used while
    /// draining during shutdown.
    pub const SERVICE_UNAVAILABLE: Self = Self(503);

    /// Is this a 2xx status?
    pub fn is_success(self) -> bool {
        self.0 >= 200 && self.0 < 300
    }
}

impl std::fmt::Display for StatusCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_2xx_is_success() {
        assert!(StatusCode::OK.is_success());
        assert!(StatusCode::CREATED.is_success());
        assert!(StatusCode::NO_CONTENT.is_success());
    }

    #[test]
    fn status_4xx_5xx_not_success() {
        assert!(!StatusCode::BAD_REQUEST.is_success());
        assert!(!StatusCode::NOT_FOUND.is_success());
        assert!(!StatusCode::INTERNAL_SERVER_ERROR.is_success());
    }

    #[test]
    fn status_display() {
        assert_eq!(StatusCode::OK.to_string(), "200");
        assert_eq!(StatusCode::NOT_FOUND.to_string(), "404");
    }

    #[test]
    fn status_equality() {
        assert_eq!(StatusCode::OK, StatusCode(200));
        assert_ne!(StatusCode::OK, StatusCode::NOT_FOUND);
    }
}
