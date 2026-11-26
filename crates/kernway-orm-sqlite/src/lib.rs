//! SQLite for the kernway ORM.
//!
//! The shared SQL layer ([`kernway_orm_sql`]) — repositories, the query
//! builder, joins and aggregates, transactions, cursors, hand-written SQL,
//! `execute_many` — over an embedded SQLite (compiled in: the same engine on
//! Linux, macOS and Windows, with FTS5 and the JSON functions).
//!
//! ```rust,no_run
//! # async fn demo() -> Result<(), kernway_orm_core::error::OrmError> {
//! use kernway_orm_sqlite::SqliteDriver;
//! let db = SqliteDriver::open("app.db").await?;            // WAL, foreign keys on
//! db.execute("CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, name TEXT)").await?;
//! db.execute_with("INSERT INTO t (name) VALUES (?)", &["Ann".into()]).await?;
//! let rows = db.query("SELECT id, name FROM t", &[]).await?;
//! # let _ = rows; Ok(()) }
//! ```
//!
//! # Async without blocking the reactor
//!
//! SQLite is a library, not a server: every call reads or writes the file
//! and may wait for another connection's lock. Each pooled connection
//! therefore lives on its own thread; the async side sends it work and
//! awaits the reply, so a slow disk or a busy lock never stalls the shard.
//! Cursors are statements kept open on that thread and stepped a batch at a
//! time.
//!
//! # Concurrency
//!
//! One writer at a time (SQLite locks the database, not rows). Transactions
//! begin `IMMEDIATE`, taking the write lock up front, so concurrent writers
//! queue for up to [`SqliteConfig::busy_timeout`] instead of failing; in WAL
//! mode (the default for files) readers never wait for the writer.
//!
//! # Not available
//!
//! Stored procedures do not exist in SQLite: [`SqlDb::call`](kernway_orm_sql::SqlDb::call)
//! is `Unsupported`. Register Rust functions instead ([`SqliteConfig::function`])
//! and call them from SQL or with `call_function`.

mod config;
mod conn;
mod dialect;
mod driver;
mod oneshot;
mod worker;

pub use config::{Change, ChangeKind, FunctionBody, JournalMode, Location, OpenMode, ScalarFunction, SqliteConfig, Synchronous};
pub use conn::SqliteConn;
pub use dialect::SqliteDialect;
pub use driver::{SqliteConnect, SqliteDriver};

/// A transaction on a SQLite database.
pub type SqliteTx = kernway_orm_sql::SqlTx<SqliteConnect>;
