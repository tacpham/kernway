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

pub mod app;
/// JWT bearer authentication middleware (`BearerAuth`, feature = `jwt`).
#[cfg(feature = "jwt")]
pub mod auth;
/// Config-driven backend selection (memory / file / redis) for the security stores.
pub mod backends;
/// Dynamic response compression middleware (feature = `compression`).
#[cfg(feature = "compression")]
pub mod compression;
/// CORS — cross-origin resource sharing middleware (Spring's `cors()`).
pub mod cors;
/// Typed argument extraction for `#[controller]` methods.
pub mod extract;
/// Synchronous middleware chain and the built-in layers.
pub mod middleware;
/// `Multipart` — a `multipart/form-data` request body (RFC 7578).
pub mod multipart;
/// Rate limiting — a per-client token bucket returning `429`.
pub mod rate_limit;
pub mod router;
/// Web security — central, path-based access rules (Spring's `HttpSecurity`).
pub mod security;
/// Visitor tracking + ban middleware (`VisitorTracking`, `BanFilter`).
pub mod tracking;
/// `UploadFile` — a large request body streamed to a temp file.
pub mod upload;

pub use app::{AppBuilder, Controller, KeepAliveConfig, KernwayApp};
pub use middleware::{Middleware, Next};
pub use router::Router;
// The async handler/middleware future type (KEP-0006).
pub use kernway_core::layer::BoxFuture;
// The per-request DI scope handlers receive (KEP-0005), re-exported so a handler
// signature `|req, scope| …` needs only kernway-server in scope.
pub use di_core::RequestScope;
// Re-exported so `#[controller]`/`#[route]`-generated code references only
// `::kernway_server::…` — a controller crate needs just this one dependency.
pub use kernway_core::error::StatusCode;
pub use kernway_core::request::Request;
pub use kernway_core::response::Response;
// The role check `#[require_role]` compiles to (reads the SecurityContext the auth
// middleware put in the scope, KEP-0005) and the 401/403 responses.
pub use app::{forbidden, role_allowed, unauthorized};

#[doc(inline)]
pub use app::serve_file;
// Central, path-based access rules (Spring's HttpSecurity), and the SecurityHeaders
// middleware (kernway-server implements `Middleware` for it — see security.rs).
#[cfg(feature = "compression")]
pub use compression::Compression;
pub use cors::Cors;
pub use kernway_security::SecurityHeaders;
pub use rate_limit::RateLimit;
pub use security::{Access, HttpSecurity, SecurityLayer};
// Typed argument extraction + the extractors, re-exported so `#[controller]` method
// params (`id: Path<u64>`, `body: Validated<T>`) reference only `::kernway_server::`.
pub use app::UploadConfig;
pub use extract::Extract;
pub use kernway_web::{Json, Path, ProblemDetail, Query, Validated};
pub use multipart::{Multipart, Part};
pub use upload::UploadFile;
// Visitor tracking + ban middleware and its types.
pub use kernway_security::{BanList, Bans, RequestMeta};
pub use tracking::{BanFilter, VisitorTracking};
// Config-driven backend selection.
pub use backends::{session_store_from_config, BackendError, BanBackend};
// JWT bearer authentication (feature = `jwt`).
#[cfg(feature = "jwt")]
pub use auth::BearerAuth;
#[cfg(feature = "jwt")]
pub use kernway_security::{Claims, Jwt, JwtError, Validation};
// Live activity — the "who's on the site and where" middleware + store (feature =
// `presence`, forwarded to kernway-security).
#[cfg(feature = "presence")]
pub use kernway_security::{ActiveVisitor, Activity, InMemoryActivity};
#[cfg(feature = "presence")]
pub use tracking::ActivityTracking;
