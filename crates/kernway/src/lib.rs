//! # kernway
//!
//! Rust web framework — Spring-inspired. One dependency; a fresh `kernway` is a
//! working web server.
//!
//! ```toml
//! [dependencies]
//! kernway = "0.1"
//! ```
//!
//! ```no_run
//! use kernway::prelude::*;
//!
//! KernwayApp::builder()
//!     .static_files("public")
//!     .get("/health", |_req, _ctx| Response::new(StatusCode::OK))
//!     .build()
//!     .run()
//!     .unwrap();
//! ```
//!
//! `use kernway::prelude::*` brings in what a handler needs. The individual
//! crates (`kernway_server`, `kernway_web`, `di_core`, …) are re-exported here
//! too, so a caller never depends on them by name.

// --- HTTP vocabulary (kernway-core) ---
pub use kernway_core::prelude::*;
pub use kernway_core::{error, error as http_error, request, response};

// --- server: the builder, router, middleware, static files ---
pub use kernway_server::{AppBuilder, KernwayApp, Router};

// --- web: extractors and response types ---
pub use kernway_web::{Json, Path, ProblemDetail, Query};

// --- DI ---
pub use di_core::{AppContext, BeanEntry, DiError};
pub use di_core::{KernwayComponent, KernwayController};

// --- macros ---
pub use di_macro::{Component, component, inject, controller, route, require_role, validated, transactional};

/// `use kernway::prelude::*` — everything a handler usually needs.
pub mod prelude {
    // Server
    pub use crate::{AppBuilder, KernwayApp, Router};
    // HTTP types
    pub use crate::response::Response;
    pub use kernway_core::error::StatusCode;
    // Traits
    pub use crate::{IntoResponse, FromRequest, Layer, Next, DbPool, TemplateEngine, KernwayPlugin};
    // Extractors and response types
    pub use crate::{Json, Path, ProblemDetail, Query};
    // DI
    pub use crate::{AppContext, BeanEntry, DiError, KernwayComponent, KernwayController};
    // Macros
    pub use crate::{component, inject, controller, route, require_role, validated, transactional, Component};

    pub use std::sync::Arc;
}
