//! # kernway-orm-mysql
//!
//! MySQL backend for `kernway-orm-core` — a **self-written wire-protocol
//! driver**. It speaks the classic MySQL protocol directly over a blocking
//! `std::net::TcpStream`, with no external MySQL client crate and no tokio: the
//! only outside help is SHA-1 (for `mysql_native_password` auth) and rt-core's
//! blocking pool, which bridges the synchronous socket work onto `orm-core`'s
//! `Send` futures exactly as the SQLite backend does.
//!
//! Enable the `mysql` feature to compile the driver; without it this crate still
//! exports [`MySqlDialect`] for shared SQL generation.
//!
//! ```rust,ignore
//! use kernway_orm_core::{Driver, TransactionalDriver, Repository};
//! use kernway_orm_mysql::MySqlDriver;
//!
//! let db = MySqlDriver::connect("mysql://user:pass@127.0.0.1:3306/shop")?;
//! let tickets = db.repository::<Ticket>();
//!
//! // Money-safe read-modify-write: the row is locked for the whole closure.
//! let total = db.transaction(|tx| {
//!     let repo = tx.repository::<Ticket>();
//!     let mut t = repo.find_by_id_for_update(&id)?.ok_or(OrmError::NotFound)?;
//!     t.status = "paid".into();
//!     repo.save(t)?;
//!     Ok(t_total)
//! }).await?;
//! ```
//!
//! ## Status & hardening path
//!
//! Working now: handshake + `mysql_native_password` auth, `COM_QUERY` text
//! protocol with escaped-literal values, result-set parsing, CRUD, the query
//! builder, and transactions with `SELECT … FOR UPDATE`.
//!
//! Next (documented in [`protocol`]): prepared statements / the binary protocol
//! (stronger parameter safety than text escaping), `caching_sha2_password`
//! (MySQL 8 default — needs TLS/RSA), TLS, and a connection pool.

pub mod dialect;
pub use dialect::MySqlDialect;

#[cfg(feature = "mysql")]
pub mod protocol;
#[cfg(feature = "mysql")]
pub mod pool;
#[cfg(feature = "mysql")]
pub mod value;
#[cfg(feature = "mysql")]
pub mod engine;
#[cfg(feature = "mysql")]
pub mod query;
#[cfg(feature = "mysql")]
pub mod repository;
#[cfg(feature = "mysql")]
pub mod transaction;
#[cfg(feature = "mysql")]
pub mod driver;

#[cfg(feature = "mysql")]
pub use driver::MySqlDriver;
#[cfg(feature = "mysql")]
pub use protocol::{Conn, MysqlConfig, QueryResult};
#[cfg(feature = "mysql")]
pub use query::MySqlQueryBuilder;
#[cfg(feature = "mysql")]
pub use repository::MySqlRepository;
#[cfg(feature = "mysql")]
pub use transaction::MySqlTx;
