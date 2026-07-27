//! # kernway-orm-postgres
//!
//! PostgreSQL backend for `kernway-orm-core`.
//!
//! ## Quick start
//!
//! ```toml
//! [dependencies]
//! kernway-orm-postgres = { version = "0.1", features = ["postgres"] }
//! ```
//!
//! ```rust,ignore
//! use kernway_orm_core::{Driver, Repository};
//! use kernway_orm_postgres::PostgresDriver;
//!
//! let driver = PostgresDriver::connect("postgres://user:pass@localhost/mydb").await?;
//! let users: Box<dyn Repository<User>> = driver.repository();
//! ```
//!
//! ## Without the `postgres` feature
//!
//! If you only need `PostgresDialect` (to share SQL generation with another
//! backend), enable this crate without the feature flag.

pub mod dialect;
pub use dialect::PostgresDialect;

#[cfg(feature = "postgres")]
pub mod driver;
#[cfg(feature = "postgres")]
pub use driver::PostgresDriver;

#[cfg(feature = "postgres")]
pub mod repository;
#[cfg(feature = "postgres")]
pub use repository::PostgresRepository;
