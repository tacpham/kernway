//! # di-core
//!
//! Dependency Injection runtime for Kernway.
//! Equivalent to `ApplicationContext` in Spring.

#![forbid(unsafe_code)]
#![allow(missing_docs)] // v0.1

pub mod context;
pub mod container;
pub mod bean;
pub mod error;
pub mod marker;
pub mod buildable;

pub use context::AppContext;
pub use container::Container;
pub use bean::{BeanEntry, BeanOrigin};
pub use error::DiError;
pub use marker::{KernwayComponent, KernwayController};
pub use buildable::{Buildable, RegistersComponent};
