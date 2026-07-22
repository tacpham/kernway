//! # kernway-core
//!
//! Spec-only crate — contains trait definitions only.
//! No implementations. Compile time < 1s.
//!
//! All other crates in the Kernway workspace implement these traits.
//! Community crates can also implement these traits — no need to fork Kernway.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // v0.1 — complete documentation planned for v1.0

pub mod error;
pub mod request;
pub mod response;
pub mod layer;
pub mod db;
pub mod template;
pub mod plugin;

/// Re-export all traits for use with: `use kernway_core::prelude::*`
pub mod prelude {
    pub use crate::error::KernwayError;
    pub use crate::request::FromRequest;
    pub use crate::response::IntoResponse;
    pub use crate::layer::{Layer, Next};
    pub use crate::db::DbPool;
    pub use crate::template::TemplateEngine;
    pub use crate::plugin::KernwayPlugin;
}
