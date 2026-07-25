//! # kernway-server
//!
//! The piece that turns a pile of components into a running HTTP server:
//! address binding, routing, middleware, and the shard/drain lifecycle.
//!
//! ## The flow
//!
//! ```text
//!   AppBuilder                    KernwayApp::run
//!   ──────────                    ───────────────
//!   .route(...)   ──┐             bind + N shards (one per core, via rt-net)
//!   .middleware() ──┤                        │
//!   .register(...)──┤  build()               ▼  per connection, per shard
//!   .shards(n)    ──┘   ───►  App    parse (kernway-http) ─► Router::find
//!                                                                │
//!                                     middleware chain ◄─────────┘
//!                                            │
//!                                            ▼
//!                                        handler ─► Response ─► write
//! ```
//!
//! A shard owns its connections start to finish — no task ever moves between
//! cores, so nothing on this path needs a lock.
//!
//! ## Routing
//!
//! [`Router`] splits routes in two at registration time. A pattern with no
//! placeholder can only ever match one path, so it goes into a hash map and
//! costs one lookup. Only patterns containing `{...}` are walked segment by
//! segment. Static routes therefore do not get slower as the app grows — see
//! `benches/` for the numbers that motivated the split.
//!
//! ## Shutdown
//!
//! `run` consumes the app, so a shutdown trigger has to be taken *before* it.
//! Firing one stops accepts, lets in-flight requests finish within the drain
//! timeout, then force-closes whatever is left.

#![forbid(unsafe_code)]

pub mod router;
pub mod app;
/// Synchronous middleware chain and the built-in layers.
pub mod middleware;

pub use app::{AppBuilder, KeepAliveConfig, KernwayApp};
pub use middleware::Middleware;
pub use router::Router;
// The per-request DI scope handlers receive (KEP-0005), re-exported so a handler
// signature `|req, scope| …` needs only kernway-server in scope.
pub use di_core::RequestScope;
