//! The boxed-future type alias shared across the async surface.
//!
//! The middleware trait itself lives in `kernway-server` (`Middleware`), where the
//! per-request scope (KEP-0005) and the handler chain are. An earlier `Layer`/`Next`
//! pair lived here as a pre-KEP-0006 spec; it was superseded by `Middleware` and
//! removed, leaving only this shared type.

use std::future::Future;
use std::pin::Pin;

/// A boxed future — the return type of async handlers and middleware.
///
/// **Not `Send`** — by design. Kernway is thread-per-core: each shard runs its
/// own reactor and a request's future is created and polled on one shard, never
/// moved across threads (see `rt_net::run_shards`, whose per-connection `Fut`
/// has no `Send` bound). Dropping `Send` lets a handler await a `!Send`
/// resource — a native per-core database/redis connection — directly, with no
/// `spawn_blocking` detour. The handler *function* is still shared across shards
/// (`Handler` keeps `Fn: Send + Sync`); only the future it returns is local.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;
