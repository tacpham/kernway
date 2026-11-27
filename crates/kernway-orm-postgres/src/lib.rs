//! # kernway-orm-postgres
//!
//! PostgreSQL for kernway's ORM — a **self-written driver** for the
//! frontend/backend protocol v3 on the per-core async reactor (no libpq, no
//! tokio):
//!
//! - TCP (every address of a name, several hosts with
//!   `target_session_attrs`) or a Unix-domain socket; TLS per `sslmode`
//!   (`verify-full` checks the name), client certificates;
//! - SCRAM-SHA-256 with `-PLUS` channel binding, MD5, clear-text, trust /
//!   certificate authentication;
//! - the extended protocol: prepared statements cached per connection,
//!   parameters sent for the type the server resolved, numbers / booleans /
//!   `bytea` read in binary, rows decoded straight into entities;
//! - pipelined batches (`execute_many`), server-side cursors (portals),
//!   `COPY` in and out, `LISTEN` / `NOTIFY`, notices, cancel requests,
//!   procedures (`CALL` with `OUT` arguments) and functions, scripts.
//!
//! The ORM surface — repositories, the query builder, transactions — is the
//! shared `kernway-orm-sql` layer; this crate supplies only what is
//! PostgreSQL's own.
//!
//! Enable the `postgres` feature to compile the driver; without it the crate
//! still exports [`PostgresDialect`].
//!
//! ```rust,ignore
//! use kernway_orm_core::{Driver, Repository};
//! use kernway_orm_postgres::PostgresDriver;
//!
//! let db = PostgresDriver::connect("postgres://shop:secret@127.0.0.1/shop?sslmode=verify-full").await?;
//! let tickets = db.repository::<Ticket>();
//! db.copy_in("COPY items (name, qty) FROM STDIN (FORMAT csv)", b"a,1\nb,2\n").await?;
//! ```

pub mod dialect;
pub use dialect::PostgresDialect;

#[cfg(feature = "postgres")]
mod auth;
#[cfg(feature = "postgres")]
pub mod backend;
#[cfg(feature = "postgres")]
pub mod config;
#[cfg(feature = "postgres")]
pub mod driver;
#[cfg(feature = "postgres")]
pub mod pool;
#[cfg(feature = "postgres")]
pub mod protocol;
#[cfg(feature = "postgres")]
pub mod types;

#[cfg(feature = "postgres")]
pub use config::{ChannelBinding, Host, PgConfig, SslMode, TargetSession};
#[cfg(feature = "postgres")]
pub use driver::{Listener, PostgresDriver};
#[cfg(feature = "postgres")]
pub use pool::PgConnect;
#[cfg(feature = "postgres")]
pub use protocol::{CancelToken, Column, Conn, Executed, Notification, PgError, QueryResult, Statement};
/// The shared SQL types used with this driver: parameters, typed values, results.
#[cfg(feature = "postgres")]
pub use kernway_orm_sql::{
    col, params, Agg, CallArg, CallResult, Cmp, Col, DbCursor, OutKind, Outcome, Param, Rows, Select, ServerCursor, SqlDb, Value,
};
/// The PostgreSQL query builder.
#[cfg(feature = "postgres")]
pub type PgQueryBuilder<T> = kernway_orm_sql::SqlQueryBuilder<T, pool::PgConnect>;
/// The PostgreSQL repository.
#[cfg(feature = "postgres")]
pub type PgRepository<T> = kernway_orm_sql::SqlRepository<T, pool::PgConnect>;
/// A PostgreSQL transaction handle.
#[cfg(feature = "postgres")]
pub type PgTx = kernway_orm_sql::SqlTx<pool::PgConnect>;
