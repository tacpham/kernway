//! The boxed-future type alias shared across the async surface.
//!
//! The middleware trait itself lives in `kernway-server` (`Middleware`), where the
//! per-request scope (KEP-0005) and the handler chain are. An earlier `Layer`/`Next`
//! pair lived here as a pre-KEP-0006 spec; it was superseded by `Middleware` and
//! removed, leaving only this shared type.

use std::future::Future;
use std::pin::Pin;

/// A boxed, `Send` future — the return type of async handlers and middleware.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
