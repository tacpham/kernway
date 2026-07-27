//! SQLite backend for kernway-orm-core.
//!
//! Row marshaling uses `serde_json` as an intermediary layer:
//! - entity → JSON → SQLite params on write
//! - SQLite row → JSON → entity on read
//!
//! This keeps the backend generic over any `Serialize + DeserializeOwned`
//! entity without requiring a custom row-mapping derive.

mod dialect;
mod driver;
mod query;
mod repository;

pub use dialect::SqliteDialect;
pub use driver::SqliteDriver;
pub use repository::SqliteRepository;
