//! Oracle Database backend for kernway: a self-written **thin** driver that
//! speaks Oracle's network protocol (TNS framing + TTC messages) directly over
//! the per-core reactor — no Oracle Client / Instant Client libraries.
//!
//! The protocol is not publicly documented; this implementation follows
//! Oracle's own open-source thin driver, python-oracledb (UPL-1.0 /
//! Apache-2.0, <https://github.com/oracle/python-oracledb>), whose thin mode
//! is the reference for the wire formats used here.
//!
//! ```rust,ignore
//! let cfg = OracleConfig::from_url("oracle://scott:tiger@db:1521/FREEPDB1")?;
//! let mut conn = Conn::connect(&cfg).await?;
//! let rows = conn.execute("SELECT name FROM items WHERE price > :p", &[Bind::from(10i64)]).await?.rows;
//! let out = conn
//!     .execute("BEGIN :r := add_tax(:cents); END;", &[Bind::Out(OutType::Number), Bind::from(1000i64)])
//!     .await?
//!     .out;
//! ```
//!
//! Through the shared SQL layer, [`OracleDriver`] gives repositories, the
//! query builder, transactions and hand-written SQL (`query`, `query_as`,
//! streaming `query_each`, `execute_many` as array DML, `call` with OUT /
//! IN OUT / cursor arguments, implicit results and `DBMS_OUTPUT`,
//! `execute_script` with PL/SQL units ended by `/`).
//!
//! Scope: TCP or TCPS (incl. the listener's TLS renegotiation), user /
//! password logon (11g and 12c verifiers), SQL and PL/SQL with IN / OUT /
//! IN OUT binds, array DML, implicit result sets, REF CURSORs. Types:
//! NUMBER, BINARY_FLOAT / DOUBLE, VARCHAR2 / NVARCHAR2 / CHAR / LONG, RAW /
//! LONG RAW, DATE, TIMESTAMP (with time zone — an offset or a named region —
//! and with local time zone), INTERVAL YEAR TO MONTH / DAY TO SECOND, CLOB /
//! NCLOB / BLOB of any size (columns inline; PL/SQL through LOB locators and
//! temporary LOBs), BFILE, JSON (OSON decoded), VECTOR, BOOLEAN, ROWID /
//! UROWID, nested `CURSOR(…)` columns. Server-side cursors. Refused with a
//! clear error (convert in SQL instead): object types and XMLType.

mod auth_crypto;
mod backend;
mod buf;
mod conn;
mod dialect;
mod oson;
mod driver;
mod pool;
mod data_types_table;
mod exec;
mod lob;
mod stmt;
mod tns;
mod value;
#[cfg(test)]
mod server_checks;

pub use conn::{Conn, OracleConfig, SslMode};
pub use dialect::OracleDialect;
pub use driver::{OracleDriver, OracleRepository, OracleTx};
pub use pool::OracleConnect;
pub use exec::{Bind, Column, Executed, OpenCursor, OutType};
pub use value::{DateTime, ResultSet, Value};
