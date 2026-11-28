//! # kernway-orm-mysql
//!
//! MySQL backend for kernway's ORM — a **self-written wire-protocol driver**
//! on the per-core async reactor (no MySQL client library, no tokio):
//! `mysql_native_password` and `caching_sha2_password` auth, TLS
//! (`?ssl-mode=`), prepared statements with a per-connection cache, buffered
//! packet I/O, rows decoded straight into entities, stored procedures and
//! multi-result responses, and large values (LONGTEXT / LONGBLOB) split into
//! 16 MiB packets.
//!
//! The ORM surface — repositories, the query builder, transactions — is the
//! shared `kernway-orm-sql` layer; this crate supplies only what is MySQL's
//! own ([`backend`]: how a connection runs SQL, reports generated keys and
//! controls transactions).
//!
//! Enable the `mysql` feature to compile the driver; without it this crate still
//! exports [`MySqlDialect`] for shared SQL generation.
//!
//! ```rust,ignore
//! use kernway_orm_core::{Driver, TransactionalDriver, Repository};
//! use kernway_orm_mysql::MySqlDriver;
//!
//! let db = MySqlDriver::connect("mysql://user:pass@127.0.0.1:3306/shop").await?;
//! let tickets = db.repository::<Ticket>();
//!
//! // Money-safe read-modify-write: the row is locked for the whole closure.
//! db.transaction(|tx| Box::pin(async move {
//!     let repo = tx.repository::<Ticket>();
//!     let mut t = repo.find_by_id_for_update(&id).await?.ok_or(OrmError::NotFound)?;
//!     t.status = "paid".into();
//!     repo.save(t).await
//! })).await?;
//! ```

pub mod dialect;
pub use dialect::MySqlDialect;

#[cfg(feature = "mysql")]
pub mod protocol;
#[cfg(feature = "mysql")]
mod ed25519;
#[cfg(feature = "mysql")]
mod rsa;
#[cfg(feature = "mysql")]
pub mod pool;
#[cfg(feature = "mysql")]
pub mod value;
#[cfg(feature = "mysql")]
pub mod backend;
#[cfg(feature = "mysql")]
pub mod driver;

#[cfg(feature = "mysql")]
pub use driver::MySqlDriver;
#[cfg(feature = "mysql")]
pub use pool::MySqlConnect;
#[cfg(feature = "mysql")]
pub use protocol::{BindValue, Cell, Conn, Fetched, MysqlConfig, QueryResult};
/// The shared SQL types used with this driver: parameters, typed values, results.
#[cfg(feature = "mysql")]
pub use kernway_orm_sql::{
    col, params, Agg, CallArg, CallResult, Cmp, Col, ColumnInfo, DbCursor, OutKind, Outcome, Param, Rows, Select, ServerCursor, SqlDb, Value,
};
/// The MySQL query builder (the shared SQL query builder over MySQL connections).
#[cfg(feature = "mysql")]
pub type MySqlQueryBuilder<T> = kernway_orm_sql::SqlQueryBuilder<T, pool::MySqlConnect>;
/// The MySQL repository (the shared SQL repository over MySQL connections).
#[cfg(feature = "mysql")]
pub type MySqlRepository<T> = kernway_orm_sql::SqlRepository<T, pool::MySqlConnect>;
/// A MySQL transaction handle.
#[cfg(feature = "mysql")]
pub type MySqlTx = kernway_orm_sql::SqlTx<pool::MySqlConnect>;
