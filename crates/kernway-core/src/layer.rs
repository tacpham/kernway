//! Middleware Layer trait.

use crate::request::Request;
use crate::response::Response;
use std::future::Future;
use std::pin::Pin;

/// Async handler function signature — boxed for object safety.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Continue the middleware chain — call the handler or next layer.
///
/// Equivalent to `FilterChain` in Spring Security.
pub trait Next: Send + Sync {
    /// Hand the request to the next layer, or to the handler if this is the last one.
    ///
    /// A layer that never calls this short-circuits the chain — which is exactly
    /// how auth and rate limiting reject a request without reaching the handler.
    fn call<'a>(&'a self, req: Request) -> BoxFuture<'a, Response>;
}

/// Middleware layer — intercept request/response.
///
/// Equivalent to `OncePerRequestFilter` or `HandlerInterceptor` in Spring.
/// Implement to provide logging, auth, rate limiting, CORS, tracing, ...
pub trait Layer: Send + Sync {
    /// Intercept one request/response round trip.
    ///
    /// Work before `next.call(req)` sees the request on the way in; work after it
    /// sees the response on the way out. Skip the call entirely to reject early.
    fn handle<'a>(&'a self, req: Request, next: &'a dyn Next) -> BoxFuture<'a, Response>;
}
