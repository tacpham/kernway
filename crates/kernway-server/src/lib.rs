//! kernway-server — HTTP server, Router, KernwayApp builder.

#![forbid(unsafe_code)]

pub mod router;
pub mod app;
pub mod middleware;

pub use app::{AppBuilder, KeepAliveConfig, KernwayApp};
pub use router::Router;
