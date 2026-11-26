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
//! Scope today: plain TCP, user/password logon (11g and 12c verifiers), SQL and
//! PL/SQL with IN / OUT / IN OUT binds, NUMBER, VARCHAR2 / NVARCHAR2, DATE,
//! TIMESTAMP, BINARY_FLOAT / DOUBLE, RAW and BOOLEAN. Not yet: LOBs, REF
//! CURSOR, TLS (TCPS), native network encryption.

mod auth_crypto;
mod buf;
mod conn;
mod data_types_table;
mod exec;
mod stmt;
mod tns;
mod value;

pub use conn::{Conn, OracleConfig};
pub use exec::{Bind, Column, Executed, OutType};
pub use value::{DateTime, ResultSet, Value};
