//! # kernway-orm-mssql
//!
//! SQL Server for kernway's ORM — a **self-written TDS driver** (MS-TDS 7.4
//! and 8.0) on the per-core async reactor; no ODBC, no tiberius, no tokio:
//!
//! - TLS in every mode SQL Server speaks: inside PRELOGIN (TDS 7.4),
//!   login-only, none, and TDS 8.0 `strict`; certificates checked against
//!   a CA and the host name, or trusted;
//! - SQL Server logins; `mssql://` URLs and ADO.NET connection strings;
//! - `sp_executesql` with typed parameters, token streams read packet by
//!   packet, every common type both ways, `(max)` values in PLP chunks;
//! - pipelined batches, API cursors, procedures with `OUTPUT` parameters,
//!   `GO` scripts, `PRINT` / `RAISERROR` messages, typed errors;
//! - statement timeouts that cancel with `ATTENTION` and keep the connection.
//!
//! ```rust,ignore
//! let db = MssqlDriver::connect("mssql://app:secret@db.example:1433/shop?encrypt=strict").await?;
//! let items = db.repository::<Item>();
//! ```
//!
//! Enable the `mssql` feature to compile the driver; without it the crate
//! still exports [`SqlServerDialect`].

pub mod dialect;
pub use dialect::SqlServerDialect;

#[cfg(feature = "mssql")]
pub mod backend;
#[cfg(feature = "mssql")]
pub mod config;
#[cfg(feature = "mssql")]
pub mod driver;
#[cfg(feature = "mssql")]
pub mod pool;
#[cfg(feature = "mssql")]
pub mod protocol;
#[cfg(feature = "mssql")]
pub mod types;

#[cfg(feature = "mssql")]
pub use config::{Encrypt, MssqlConfig};
#[cfg(feature = "mssql")]
pub use protocol::{Column, Conn, Response, ServerMessage};
#[cfg(feature = "mssql")]
pub use driver::MssqlDriver;
#[cfg(feature = "mssql")]
pub use pool::MssqlConnect;
/// The shared SQL types used with this driver.
#[cfg(feature = "mssql")]
pub use kernway_orm_sql::{col, params, Agg, CallArg, CallResult, Cmp, Col, DbCursor, OutKind, Outcome, Param, Rows, Select, ServerCursor, SqlDb, Value};
/// A SQL Server transaction handle.
#[cfg(feature = "mssql")]
pub type MssqlTx = kernway_orm_sql::SqlTx<pool::MssqlConnect>;
/// The SQL Server repository.
#[cfg(feature = "mssql")]
pub type MssqlRepository<T> = kernway_orm_sql::SqlRepository<T, pool::MssqlConnect>;
