//! HTTP Request abstraction + FromRequest trait.

use crate::error::KernwayError;
use std::collections::HashMap;

/// Raw HTTP request — implementation-agnostic.
#[derive(Debug)]
pub struct Request {
    pub method:      String,
    pub path:        String,
    pub headers:     HashMap<String, String>,
    pub query:       HashMap<String, String>,
    pub path_params: HashMap<String, String>,
    pub body:        Vec<u8>,
}

impl Request {
    /// Create a new request (for testing).
    pub fn new(method: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            method:      method.into(),
            path:        path.into(),
            headers:     HashMap::new(),
            query:       HashMap::new(),
            path_params: HashMap::new(),
            body:        Vec::new(),
        }
    }

    /// Get a header value.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_lowercase()).map(|s| s.as_str())
    }
}

/// Trait for extracting data from an HTTP request into a Rust type.
///
/// Equivalent to `HandlerMethodArgumentResolver` in Spring MVC.
/// `Path<T>`, `Query<T>`, `Json<T>`, and `Header<T>` all implement this trait.
pub trait FromRequest: Sized + Send {
    /// Error returned when extraction fails — must implement IntoResponse.
    type Rejection: crate::response::IntoResponse;

    /// Extract self from the request.
    fn from_request(req: &Request) -> Result<Self, Self::Rejection>;
}

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
