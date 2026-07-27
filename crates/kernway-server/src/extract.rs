//! Typed argument extraction for `#[controller]` methods.
//!
//! A controller method can declare its inputs by type instead of parsing the raw
//! request: `async fn get(&self, id: Path<u64>, body: Validated<CreateUser>)`. The
//! `#[controller]` macro extracts each parameter through [`Extract`] before calling
//! the method; a failed extraction (a bad path segment, an invalid or unvalidated
//! body) short-circuits to the extractor's error response, so the method body only
//! runs on well-formed input.
//!
//! `Request` itself is not an [`Extract`] — the macro recognises it and passes the
//! owned request (the raw escape hatch, still supported).

use di_core::RequestScope;
use kernway_core::request::Request;
use kernway_core::response::Response;
use kernway_security::SecurityContext;
use kernway_web::{Json, Path, ProblemDetail, Query, Validated};

use crate::multipart::Multipart;
use crate::upload::UploadFile;

/// Extract a typed value from the request (and the request scope). The `param` is
/// the method parameter's name — a `Path<T>` uses it as the path-variable name, the
/// rest ignore it. A failure is a ready error [`Response`] the handler returns.
pub trait Extract: Sized {
    /// Extract `Self`, or a ready error response (a 400/401) on failure.
    ///
    /// # Errors
    /// Returns the error response when the value cannot be extracted.
    fn extract(req: &Request, scope: &RequestScope, param: &str) -> Result<Self, Response>;
}

impl<T> Extract for Path<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    fn extract(req: &Request, _scope: &RequestScope, param: &str) -> Result<Self, Response> {
        Path::<T>::from_request(req, param).map_err(ProblemDetail::bad_request)
    }
}

impl<T: serde::de::DeserializeOwned> Extract for Json<T> {
    fn extract(req: &Request, _scope: &RequestScope, _param: &str) -> Result<Self, Response> {
        Json::<T>::from_request(req).map_err(ProblemDetail::bad_request)
    }
}

impl<T: serde::de::DeserializeOwned> Extract for Query<T> {
    fn extract(req: &Request, _scope: &RequestScope, _param: &str) -> Result<Self, Response> {
        Query::<T>::from_request(req).map_err(ProblemDetail::bad_request)
    }
}

impl<T> Extract for Validated<T>
where
    T: serde::de::DeserializeOwned + kernway_validation::Validate,
{
    fn extract(req: &Request, _scope: &RequestScope, _param: &str) -> Result<Self, Response> {
        // Validated already renders RFC 7807 on failure.
        Validated::<T>::from_request(req)
    }
}

impl Extract for SecurityContext {
    fn extract(_req: &Request, scope: &RequestScope, _param: &str) -> Result<Self, Response> {
        // The context the auth middleware set (KEP-0005); anonymous if none.
        Ok(scope
            .get::<SecurityContext>()
            .map(|ctx| (*ctx).clone())
            .unwrap_or_default())
    }
}

impl Extract for UploadFile {
    fn extract(req: &Request, _scope: &RequestScope, _param: &str) -> Result<Self, Response> {
        UploadFile::from_request(req).map_err(ProblemDetail::bad_request)
    }
}

impl Extract for Multipart {
    fn extract(req: &Request, _scope: &RequestScope, _param: &str) -> Result<Self, Response> {
        Multipart::from_request(req).map_err(|e| ProblemDetail::bad_request(e.to_string()))
    }
}
