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

pub mod conn;
pub mod error;
pub mod resp;

use std::net::SocketAddr;
use std::sync::Mutex;

pub use conn::Connection;
pub use error::RedisError;
pub use resp::Value;

/// How many idle connections a pool keeps around between bursts.
const DEFAULT_MAX_IDLE: usize = 16;

/// A pool of Redis connections behind a `&self`, safe to share across cores.
///
/// A Kernway task is pinned to its core, but a `SessionStore` bean is one instance
/// shared by every core, so it needs `&self` access to connections. The pool keeps
/// a stack of idle connections and hands one out per command. The `Mutex` is held
/// only to pop or push a connection — **never across the `.await`** of the command
/// itself — so a slow Redis round trip never blocks another core at the lock.
pub struct Pool {
    addr: SocketAddr,
    /// `AUTH` credentials, applied once to each freshly-dialled connection.
    auth: Option<Auth>,
    idle: Mutex<Vec<Connection>>,
    max_idle: usize,
}

/// Credentials for `AUTH`: a password, and an optional ACL user.
struct Auth {
    user: Option<String>,
    password: String,
}

impl Pool {
    /// A pool that dials `addr`. No connection is opened until the first command.
    #[must_use]
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            addr,
            auth: None,
            idle: Mutex::new(Vec::new()),
            max_idle: DEFAULT_MAX_IDLE,
        }
    }

    /// Authenticate every connection with a password (a `requirepass` server).
    #[must_use]
    pub fn with_auth(mut self, password: impl Into<String>) -> Self {
        self.auth = Some(Auth {
            user: None,
            password: password.into(),
        });
        self
    }

    /// Authenticate every connection as an ACL user (`AUTH user password`).
    #[must_use]
    pub fn with_user_auth(mut self, user: impl Into<String>, password: impl Into<String>) -> Self {
        self.auth = Some(Auth {
            user: Some(user.into()),
            password: password.into(),
        });
        self
    }

    /// Set how many idle connections to retain (default 16).
    #[must_use]
    pub fn with_max_idle(mut self, max_idle: usize) -> Self {
        self.max_idle = max_idle;
        self
    }

    /// Check a connection out — an idle one if there is any, else a fresh dial
    /// (authenticated if the pool carries credentials). The lock is released
    /// before any dial or handshake happens.
    pub async fn checkout(&self) -> Result<Connection, RedisError> {
        let idle = self.idle.lock().unwrap().pop();
        if let Some(conn) = idle {
            return Ok(conn);
        }
        let mut conn = Connection::connect(self.addr).await?;
        if let Some(auth) = &self.auth {
            conn.auth(auth.user.as_deref(), &auth.password).await?;
        }
        Ok(conn)
    }

    /// Return a healthy connection to the pool (dropped if the pool is full).
    /// Only call this for a connection whose last command *succeeded* — a
    /// connection that errored may be mid-reply and must be dropped instead.
    pub fn checkin(&self, conn: Connection) {
        let mut idle = self.idle.lock().unwrap();
        if idle.len() < self.max_idle {
            idle.push(conn);
        }
    }

    /// Run one operation against a checked-out connection, returning it to the
    /// pool on success and dropping it on error.
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
        match f(&mut conn).await {
            Ok(value) => {
                self.checkin(conn);
                Ok(value)
            }
            // Drop the connection: after an error it may hold a partial reply.
            Err(err) => Err(err),
        }
    }
}
