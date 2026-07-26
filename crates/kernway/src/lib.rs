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
//!     .get("/health", |_req: Request, _ctx: &RequestScope| async { Response::new(StatusCode::OK) })
//!     .build()
//!     .run()
//!     .unwrap();
//! ```
//!
//! `use kernway::prelude::*` brings in what a handler needs. The individual
//! crates (`kernway_server`, `kernway_web`, `di_core`, …) are re-exported here
//! too, so a caller never depends on them by name.
//!
//! ## Capabilities
//!
//! The baseline is a working web server on its own — DI, routing, HTTP, JSON,
//! HTML, and static files. Extra capabilities are opt-in features that pull in
//! nothing when off:
//!
//! ```toml
//! kernway = { version = "0.1", features = ["htmx"] }
//! ```
//!
//! | Feature | Brings in | Adds to the prelude |
//! |---|---|---|
//! | `htmx` | typed `HX-*` request extraction and response headers (htmx 2.0.x) | `Htmx`, `HtmxResponse`, `Swap` |
//! | `security` | CSRF tokens, security headers, sessions (KEP-0004) | `SecurityContext`, `SecurityHeaders` |
//! | `presence` | heartbeat / who is online now | `Presence`, `InMemoryPresence` |
//! | `redis` | Redis-backed session store, and presence when `presence` is on too (implies `security`) | + `RedisSessionStore` (`+ RedisPresence`) |

// --- HTTP vocabulary (kernway-core) ---
pub use kernway_core::prelude::*;
pub use kernway_core::{error, error as http_error, request, response};

// --- server: the builder, router, middleware, static files, controllers ---
pub use kernway_server::{AppBuilder, BoxFuture, Controller, KernwayApp, Router};
// --- web security: central path-based access rules (Spring's HttpSecurity) ---
pub use kernway_server::{Access, HttpSecurity};

// --- the per-request DI scope a handler receives (KEP-0005) ---
pub use di_core::RequestScope;

// --- web: extractors and response types ---
pub use kernway_web::{Html, Json, Path, ProblemDetail, Query, Validated};

// --- validation: constraints + RFC 7807 (KEP-0000 §1) ---
pub use kernway_validation::{self, rules as validation_rules, Validate, ValidationErrors};

// --- htmx: typed HX-* extraction and response headers (feature = "htmx") ---
#[cfg(feature = "htmx")]
pub use kernway_htmx::{Htmx, HtmxResponse, Swap};

// --- security: CSRF, headers, sessions (feature = "security") ---
#[cfg(feature = "security")]
pub use kernway_security::{csrf, session, SecurityContext, SecurityHeaders};
// The Redis-backed session store (feature = "redis", which implies "security").
#[cfg(feature = "redis")]
pub use kernway_security::RedisSessionStore;

// --- presence: heartbeat / who is online (feature = "presence") ---
#[cfg(feature = "presence")]
pub use kernway_security::{presence, InMemoryPresence, Presence};
// The Redis-backed presence tracker needs both features.
#[cfg(all(feature = "presence", feature = "redis"))]
pub use kernway_security::RedisPresence;

// --- config: layered .properties, profiles, env (KEP-0007) ---
pub use kernway_config::{self, Config, ConfigBuilder, FromConfig};

// --- logging: levels, per-module filters, Pretty/JSON ---
pub use kernway_log::{self, Filter as LogFilter, Format as LogFormat, Level as LogLevel, Logger};

// --- DI ---
pub use di_core::{AppContext, BeanEntry, DiError};
pub use di_core::{KernwayComponent, KernwayController};

// --- macros ---
// `Validate` is both the derive (di-macro, macro namespace) and the trait
// (kernway-validation, type namespace) — coexisting like serde's `Serialize`.
pub use di_macro::{Component, Validate, component, configuration, inject, controller, route, require_role, validated, transactional};

/// `use kernway::prelude::*` — everything a handler usually needs.
pub mod prelude {
    // Server
    pub use crate::{AppBuilder, KernwayApp, Router};
    // HTTP types
    pub use crate::request::Request;
    pub use crate::response::Response;
    pub use kernway_core::error::StatusCode;
    // The per-request DI scope and the async handler future type (KEP-0005/0006)
    pub use crate::{BoxFuture, RequestScope};
    // Traits
    pub use crate::{IntoResponse, DbPool, TemplateEngine, KernwayPlugin};
    // Extractors and response types
    pub use crate::{Html, Json, Path, ProblemDetail, Query, Validated};
    // Validation — the trait + derive share the name `Validate`
    pub use crate::{Validate, ValidationErrors};
    // htmx (feature = "htmx")
    #[cfg(feature = "htmx")]
    pub use crate::{Htmx, HtmxResponse, Swap};
    // security (feature = "security")
    #[cfg(feature = "security")]
    pub use crate::{SecurityContext, SecurityHeaders};
    // presence (feature = "presence")
    #[cfg(feature = "presence")]
    pub use crate::{InMemoryPresence, Presence};
    // Config (KEP-0007)
    pub use crate::{Config, FromConfig};
    // Logging macros + init
    pub use kernway_log::{debug, error, info, trace, warn};
    // DI
    pub use crate::{AppContext, BeanEntry, DiError, KernwayComponent, KernwayController};
    // Macros
    pub use crate::{component, configuration, inject, controller, route, require_role, validated, transactional, Component};

    pub use std::sync::Arc;
}
