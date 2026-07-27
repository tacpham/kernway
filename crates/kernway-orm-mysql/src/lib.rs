//! # kernway-orm-mysql
//!
//! MySQL backend for `kernway-orm-core`.
//!
//! Enable the `mysql` feature to compile the driver skeleton; without it this
//! crate still exports `MySqlDialect` for shared SQL generation.

pub mod dialect;
pub use dialect::MySqlDialect;

#[cfg(feature = "mysql")]
pub mod driver;
#[cfg(feature = "mysql")]
pub use driver::MySqlDriver;

#[cfg(feature = "mysql")]
pub mod repository;
#[cfg(feature = "mysql")]
pub use repository::MySqlRepository;
