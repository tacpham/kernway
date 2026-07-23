//! Database pool abstraction.

use crate::layer::BoxFuture;
use std::fmt;

/// Database connection error.
#[derive(Debug)]
pub struct DbError(pub String);

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "db error: {}", self.0)
    }
}

impl std::error::Error for DbError {}

/// Async database connection — implementation-agnostic.
pub trait Connection: Send {
    /// Execute raw SQL (direct use is discouraged — use Repository instead).
    fn execute<'a>(&'a mut self, sql: &'a str) -> BoxFuture<'a, Result<u64, DbError>>;
}

/// Connection pool.
///
/// Equivalent to `javax.sql.DataSource` in Java.
/// `PostgresPool`, `MySqlPool`, and `SqlitePool` all implement this trait.
pub trait DbPool: Send + Sync + 'static {
    /// Take a connection from the pool, waiting if none is free.
    ///
    /// The connection returns to the pool when the `Box` is dropped, so hold it
    /// for as short a span as the work allows.
    fn acquire(&self) -> BoxFuture<'_, Result<Box<dyn Connection>, DbError>>;
}
