//! # kernway
//!
//! Rust Web Framework — Spring-inspired.
//!
//! ```toml
//! [dependencies]
//! kernway = "0.1"
//! ```
//!
//! ```rust,ignore
//! use kernway::prelude::*;
//!
//! #[component]
//! struct UserService;
//!
//! #[component]
//! struct UserController {
//!     #[inject]
//!     service: std::sync::Arc<UserService>,
//! }
//! ```

// Re-export core traits
pub use kernway_core::prelude::*;
pub use kernway_core::{request, response, error as http_error};

// Re-export DI
pub use di_core::{AppContext, BeanEntry, DiError};
pub use di_core::{KernwayComponent, KernwayController};

// Re-export macros
pub use di_macro::{Component, component, inject, controller, route, require_role, validated, transactional};

/// `use kernway::prelude::*` — import everything required.
pub mod prelude {
    pub use crate::{
        // Core traits
        IntoResponse, FromRequest, Layer, Next, DbPool, TemplateEngine, KernwayPlugin,
        // DI
        AppContext, BeanEntry, DiError, KernwayComponent, KernwayController,
        // Macros
        component, inject, controller, route, require_role, validated, transactional,
        Component,
    };
    pub use std::sync::Arc;
    pub use kernway_core::error::StatusCode;
}
