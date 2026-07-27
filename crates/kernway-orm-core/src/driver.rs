//! The `Driver` trait — the entry point for any database backend.
//!
//! A `Driver` is a configured, shareable handle to a specific database instance.
//! It acts as a factory for [`Repository`] instances and exposes lifecycle
//! operations (health check, migrations) that are common to every backend.
//!
//! # Implementing a new backend
//!
//! ```text
//! 1. Create a crate (e.g. `kernway-orm-postgres`).
//! 2. Define a config struct: `PostgresConfig { url: String, pool_size: usize }`.
//! 3. Implement `Driver` for a `PostgresDriver` struct.
//! 4. Implement `Repository<T>` for `PostgresRepository<T>` where T: Entity.
//! 5. Add an optional `SqlDialect` impl for `PostgresDialect` to share SQL
//!    generation with other SQL backends.
//! ```
//!
//! # Example
//! ```rust,ignore
//! let driver = SqliteDriver::open("app.db")?;
//! let users: Box<dyn Repository<User>> = driver.repository();
//! let admin = users.query().filter_eq("role", "ADMIN").fetch_one().await?;
//! ```

use crate::{entity::Entity, error::OrmError, repository::Repository, BoxFuture};
use serde::{de::DeserializeOwned, Serialize};

/// Declares which optional ORM features a driver supports.
///
/// Drivers return this from [`Driver::capabilities`] so that callers can
/// branch or fail fast instead of discovering unsupported operations at
/// runtime. The [`OrmError::Unsupported`] variant covers operations attempted
/// on a driver that returned `false` here.
#[derive(Debug, Clone, Default)]
pub struct DriverCapabilities {
    /// Driver can open a transaction and roll it back on failure.
    pub transactions: bool,
    /// Driver accepts raw, backend-native queries (SQL string or DSL JSON).
    pub raw_query: bool,
    /// Driver supports full-text or fuzzy-text search natively.
    pub full_text_search: bool,
    /// Driver has a native JSON column type (`ColumnType::Json` persisted as
    /// a structured document, not serialised text).
    pub json_columns: bool,
    /// Driver can apply schema migrations declaratively.
    pub migrations: bool,
}

/// A configured handle to a specific database instance.
///
/// `Driver` is the single point of entry for a backend crate:
/// it holds the connection / pool / config and vends `Repository` objects.
///
/// # Object-safety note
/// The `repository` method is generic over `T: Entity`, which means
/// `Driver` is not object-safe by default. Use it as a concrete type or
/// wrap it behind an application-specific factory when dynamic dispatch
/// is needed.
pub trait Driver: Send + Sync + 'static {
    /// Create a repository for the given entity type.
    ///
    /// For SQL backends this usually opens (or reuses) a connection, ensures
    /// the table exists, and returns a `Box<dyn Repository<T>>`.
    fn repository<T>(&self) -> Box<dyn Repository<T>>
    where
        T: Entity + Serialize + DeserializeOwned + 'static;

    /// Verify the connection is alive.
    ///
    /// SQL backends run `SELECT 1`; REST backends perform a HEAD request.
    /// Returns `Ok(())` on success.
    fn ping(&self) -> BoxFuture<'_, Result<(), OrmError>>;

    /// Declare what optional features this driver supports.
    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities::default()
    }
}
