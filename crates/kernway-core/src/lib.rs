//! # kernway-core
//!
//! The spec crate: trait definitions and the plain data types they pass around.
//! No implementations, no `serde`, no `diesel`, no `rustls`. It compiles in
//! under a second, and it is meant to stay that way.
//!
//! ## The idea
//!
//! Every other crate in the workspace depends on this one; this one depends on
//! almost nothing. That direction is the whole design. A framework that couples
//! its abstractions to its implementations forces you to fork it to swap a
//! piece — swapping the template engine means patching the core.
//!
//! Here, "swap a piece" means writing an impl of a trait defined in this crate.
//! `kernleaf` implements [`TemplateEngine`]; a Tera adapter would implement the
//! same trait and drop in beside it. A community crate never needs to be merged
//! upstream to participate, because there is nothing upstream to change.
//!
//! [`TemplateEngine`]: template::TemplateEngine
//!
//! ## Request lifecycle — where each trait sits
//!
//! ```text
//!  bytes on a socket
//!         │
//!         ▼
//!   ┌───────────┐   parsed by kernway-http into…
//!   │  Request  │   method · path · version · headers · query · body
//!   └───────────┘
//!         │
//!         ▼
//!   Layer::handle ──► Layer::handle ──► … ──► handler        (layer)
//!    (logging)          (auth)                   │
//!         ▲                                      │  arguments extracted via
//!         │                                      │  FromRequest              (request)
//!         │                                      ▼
//!         │                                  your code
//!         │                                      │  returns anything that is
//!         │                                      │  IntoResponse             (response)
//!         │                                      ▼
//!         └──────────── Response ◄───────────────┘
//! ```
//!
//! A `Layer` wraps the rest of the chain rather than sitting in a list: work
//! before `next.call(req)` runs on the way in, work after it runs on the way
//! out, and *not* calling it rejects the request early. That is how auth and
//! rate limiting short-circuit without a special case in the dispatcher. The
//! `Layer`/`Next` types themselves live in `kernway-server` now (KEP-0006); this
//! crate keeps only the shared [`BoxFuture`](layer::BoxFuture) alias.
//!
//! ## What lives here
//!
//! | Module | Defines | Spring analogue |
//! |---|---|---|
//! | [`request`] | [`Request`], [`HttpVersion`] | `HttpServletRequest`, argument resolvers |
//! | [`response`] | [`Response`], [`IntoResponse`] | `HttpServletResponse`, `HttpMessageConverter` |
//! | [`fields`] | [`Headers`], [`QueryParams`] | `HttpHeaders` |
//! | [`layer`] | [`BoxFuture`](layer::BoxFuture) | `OncePerRequestFilter`, `FilterChain` |
//! | [`error`] | [`KernwayError`], [`StatusCode`] | `HttpStatus` |
//! | [`db`] | [`DbPool`], [`Connection`] | `javax.sql.DataSource` |
//! | [`template`] | [`TemplateEngine`], [`Value`], [`ToValue`] | `ViewResolver`, `Model` |
//! | [`plugin`] | [`KernwayPlugin`] | `ApplicationContextInitializer` |
//!
//! [`Request`]: request::Request
//! [`HttpVersion`]: request::HttpVersion
//! [`Response`]: response::Response
//! [`IntoResponse`]: response::IntoResponse
//! [`Headers`]: fields::Headers
//! [`QueryParams`]: fields::QueryParams
//! [`KernwayError`]: error::KernwayError
//! [`StatusCode`]: error::StatusCode
//! [`DbPool`]: db::DbPool
//! [`Connection`]: db::Connection
//! [`Value`]: template::Value
//! [`ToValue`]: template::ToValue
//! [`KernwayPlugin`]: plugin::KernwayPlugin
//!
//! ## Example
//!
//! ```
//! use kernway_core::error::StatusCode;
//! use kernway_core::request::{HttpVersion, Request};
//!
//! let mut req = Request::new("GET", "/users/42");
//! req.headers.insert("accept", "application/json");
//! req.query.insert("verbose", "true");
//!
//! // Header names are case-insensitive; query names are not.
//! assert_eq!(req.headers.get("Accept"), Some("application/json"));
//! assert_eq!(req.query.get("verbose"), Some("true"));
//! assert_eq!(req.query.get("Verbose"), None);
//!
//! // HTTP/1.1 keeps the connection alive unless told otherwise.
//! assert_eq!(req.version, HttpVersion::Http11);
//! assert!(StatusCode::OK.is_success());
//! ```
//!
//! ## A note on the field types
//!
//! [`Headers`] and [`QueryParams`] are not `HashMap`s. Both store their entries
//! in a single backing buffer of offsets, because a request carries a handful of
//! short fields and one allocation beats a dozen. See `benches/` for the
//! measurements behind that choice.

#![forbid(unsafe_code)]

pub mod error;
pub mod fields;
pub mod request;
pub mod response;
pub mod layer;
pub mod db;
pub mod template;
pub mod plugin;
pub mod security;

/// Re-export all traits for use with: `use kernway_core::prelude::*`
pub mod prelude {
    pub use crate::error::KernwayError;
    pub use crate::layer::BoxFuture;
    pub use crate::response::IntoResponse;
    pub use crate::db::DbPool;
    pub use crate::template::{TemplateEngine, ToValue, Value};
    pub use crate::plugin::KernwayPlugin;
}
