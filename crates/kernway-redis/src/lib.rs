//! # kernway-redis
//!
//! A small async Redis client on Kernway's own runtime.
//!
//! It exists because the async runtime is ours ([rt-core]/[rt-net]): an
//! off-the-shelf async Redis client (`redis`, `fred`) is bolted to tokio's reactor
//! and cannot be driven by Kernway's per-core executor. So the client speaks RESP
//! directly over [`rt_net::AsyncTcpStream`], driven by the same reactor as the
//! server — the piece [KEP-0006] (async handlers) unblocked.
//!
//! Two layers, deliberately independent:
//!
//! - [`Connection`] — one socket, one request/reply at a time. [`resp`] is the
//!   pure protocol (encode/parse), unit-tested without a server.
//! - [`Pool`] — shares connections across cores for a `&self` caller (a DI bean),
//!   checking one out and back in around each command without ever holding a lock
//!   across an `.await`.
//!
//! It knows nothing about sessions — `RedisSessionStore` (in `kernway-security`)
//! is one *user* of this client, translating the session registry into commands.
//!
//! [rt-core]: https://docs.rs/rt-core
//! [rt-net]: https://docs.rs/rt-net
//! [KEP-0006]: https://github.com/tacpham/kernway/blob/main/docs/kep/0006-async-handlers.md

#![forbid(unsafe_code)]

pub mod cache;
pub mod cluster;
pub mod cmd;
pub mod commands;
pub mod conn;
pub mod error;
pub mod options;
pub mod pubsub;
pub mod resp;
mod sentinel;
pub mod stream;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::thread::{self, ThreadId};

pub use cache::RedisCache;
pub use cluster::{key_slot, ClusterClient, Shard};
pub use cmd::{pairs, Cmd, FromValue, ToArg};
pub use commands::{ScanPage, Script, SetCondition, SetOptions};
pub use conn::{Connection, PubSubMessage};
pub use options::{Endpoint, Options, Protocol, SentinelOptions, TlsOptions};
pub use error::RedisError;
pub use pubsub::{RedisConfig, RedisPubSub};
pub use resp::Value;
pub use stream::{StreamEntries, StreamEntry};

/// How many idle connections a pool keeps around between bursts.
const DEFAULT_MAX_IDLE: usize = 16;

/// A pool of Redis connections behind a `&self`, shared across cores as a DI bean.
///
/// A Kernway task is pinned to its core, but a `SessionStore` bean is one instance
/// shared by every core, so it needs `&self` access to connections. The pool keeps
/// idle connections and hands one out per command. The `Mutex` is held only to pop or
/// push a connection — **never across the `.await`** of the command itself — so a slow
/// Redis round trip never blocks another core at the lock.
///
/// **Per-shard, not shared across cores.** A socket is registered on the reactor of the
/// shard (thread) that dialled it; its readiness events are delivered there and nowhere
/// else. Reusing a pooled connection on a *different* shard would park the waiter on a
/// reactor that never owns the socket, so the read is never woken (it dies at the request
/// timeout) — the source of intermittent, shard-dependent hangs. Keying the idle bucket by
/// the owning [`ThreadId`] keeps each connection on its home shard. (Mirrors the same fix
/// in `kernway-http-client`.)
pub struct Pool {
    /// How every connection is opened and set up.
    options: Options,
    /// Idle connections bucketed by the shard (thread) that owns them — never cross-shard.
    idle: Mutex<HashMap<ThreadId, Vec<Connection>>>,
    max_idle: usize,
}

impl Pool {
    /// A pool that dials `addr` in RESP2 without authentication — the
    /// original constructor. No connection is opened until the first command.
    #[must_use]
    pub fn new(addr: SocketAddr) -> Self {
        Self::from_options(Options::tcp(addr.ip().to_string(), addr.port()).protocol(Protocol::Resp2))
    }

    /// A pool opening connections per `options` (TLS, auth, database,
    /// protocol, timeouts — see [`Options`]).
    #[must_use]
    pub fn from_options(options: Options) -> Self {
        Self { options, idle: Mutex::new(HashMap::new()), max_idle: DEFAULT_MAX_IDLE }
    }

    /// A pool from a URL (`redis://`, `rediss://`, `redis+unix://` — see
    /// [`Options::from_url`]).
    pub fn from_url(url: &str) -> Result<Self, RedisError> {
        Ok(Self::from_options(Options::from_url(url)?))
    }

    /// The options connections are opened with.
    pub fn options(&self) -> &Options {
        &self.options
    }

    /// Authenticate every connection with a password (a `requirepass` server).
    #[must_use]
    pub fn with_auth(mut self, password: impl Into<String>) -> Self {
        self.options.username = None;
        self.options.password = Some(password.into());
        self
    }

    /// Authenticate every connection as an ACL user (`AUTH user password`).
    #[must_use]
    pub fn with_user_auth(mut self, user: impl Into<String>, password: impl Into<String>) -> Self {
        self.options.username = Some(user.into());
        self.options.password = Some(password.into());
        self
    }

    /// Set how many idle connections to retain (default 16).
    #[must_use]
    pub fn with_max_idle(mut self, max_idle: usize) -> Self {
        self.max_idle = max_idle;
        self
    }

    /// Check a connection out — an idle one **owned by the current shard** if there is any,
    /// else a fresh dial (authenticated if the pool carries credentials). The lock is
    /// released before any dial or handshake happens.
    pub async fn checkout(&self) -> Result<Connection, RedisError> {
        let idle = self
            .idle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&thread::current().id())
            .and_then(|bucket| bucket.pop());
        if let Some(conn) = idle {
            return Ok(conn);
        }
        Connection::open(&self.options).await
    }

    /// Return a connection to the pool, tagged to the current shard. A broken
    /// connection (I/O error, timeout, protocol error), one left in subscribe
    /// mode, or one over the idle limit is dropped instead.
    pub fn checkin(&self, conn: Connection) {
        if conn.is_broken() || conn.is_subscribed() {
            return;
        }
        let mut map = self.idle.lock().unwrap_or_else(|e| e.into_inner());
        let bucket = map.entry(thread::current().id()).or_default();
        if bucket.len() < self.max_idle {
            bucket.push(conn);
        }
    }

    /// Run one operation against a checked-out connection, then return it to
    /// the pool — unless it broke (I/O error, timeout, protocol error), in
    /// which case it is dropped. A server error reply leaves it reusable.
    ///
    /// The closure is an `async` one that borrows the connection, so it can call
    /// any [`Connection`] method (`conn.get(…).await`, several in sequence, …):
    ///
    /// ```no_run
    /// # async fn demo(pool: &kernway_redis::Pool) -> Result<(), kernway_redis::RedisError> {
    /// let value = pool.with(async |conn| conn.get("session:abc").await).await?;
    /// # let _ = value; Ok(())
    /// # }
    /// ```
    pub async fn with<T, F>(&self, f: F) -> Result<T, RedisError>
    where
        F: AsyncFnOnce(&mut Connection) -> Result<T, RedisError>,
    {
        let mut conn = self.checkout().await?;
        let r = f(&mut conn).await;
        // A server error (`-ERR`) leaves the connection in step: keep it. A
        // broken one is dropped by `checkin`; a future dropped mid-command
        // drops the connection with it.
        self.checkin(conn);
        r
    }
}
