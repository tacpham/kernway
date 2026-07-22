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
    pub const OK:                    Self = Self(200);
    pub const CREATED:               Self = Self(201);
    pub const NO_CONTENT:            Self = Self(204);
    pub const BAD_REQUEST:           Self = Self(400);
    pub const UNAUTHORIZED:          Self = Self(401);
    pub const FORBIDDEN:             Self = Self(403);
    pub const NOT_FOUND:             Self = Self(404);
    pub const METHOD_NOT_ALLOWED:    Self = Self(405);
    pub const UNPROCESSABLE_ENTITY:  Self = Self(422);
    pub const TOO_MANY_REQUESTS:     Self = Self(429);
    pub const INTERNAL_SERVER_ERROR: Self = Self(500);
    pub const SERVICE_UNAVAILABLE:   Self = Self(503);

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
