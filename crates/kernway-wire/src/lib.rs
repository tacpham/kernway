//! Wire plumbing shared by the Kernway database drivers.
//!
//! Everything here is protocol-agnostic; how bytes are framed into messages,
//! how a session authenticates, and how results are encoded stay in each driver
//! (MySQL's 4-byte packet header, Postgres's type byte + length, …).
//!
//! - [`BufStream`]: a plaintext or TLS socket with a read buffer that hands out
//!   message bodies **in place** (one syscall serves many small messages) and a
//!   reused write buffer; upgrades to TLS mid-handshake.
//! - [`Pool`]: a per-core (`!Send`, lock-free) connection pool that discards
//!   connections marked broken ([`Pooled::poison`]).
//! - [`StmtCache`]: a bounded map of prepared statements by SQL text that hands
//!   back the evicted statement for the driver to close its own way.
//!
//! - [`offload`]: what blocks when opening a connection — host-name lookup,
//!   reading certificate files — run on the blocking pool and awaited.
//!
//! - [`scram`] (feature `scram`): the SCRAM-SHA-1 / -SHA-256 client exchange,
//!   with channel binding — PostgreSQL and MongoDB authenticate with it.
//!
//! All of it is single-shard, like the rest of the thread-per-core stack.

pub mod offload;
#[cfg(feature = "scram")]
pub mod scram;
pub mod snappy;
mod pool;
mod stmt_cache;
mod stream;

pub use pool::{Connect, Pool, Pooled};
pub use stmt_cache::StmtCache;
pub use stream::{BufStream, READ_CHUNK, READ_RETAIN};
