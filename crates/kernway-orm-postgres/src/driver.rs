//! `PostgresDriver` — the entry point: a [`SqlDb`] (pool, hand-written SQL,
//! routines, repositories, transactions — all shared) plus what only
//! PostgreSQL has: COPY and LISTEN / NOTIFY.

use crate::config::PgConfig;
use crate::pool::PgConnect;
use crate::protocol::{Conn, Notification};
use crate::PgTx;
use kernway_orm_core::{
    driver::{Driver, DriverCapabilities},
    entity::Entity,
    error::OrmError,
    repository::Repository,
    transaction::TransactionalDriver,
    BoxFuture,
};
use kernway_orm_sql::{Param, SqlDb};
use serde::{de::DeserializeOwned, Serialize};
use std::ops::Deref;

/// Default per-core pool size — connections opened on demand, up to this many.
const DEFAULT_POOL_SIZE: usize = 8;

/// A configured handle to a PostgreSQL database, backed by a per-core async pool.
///
/// Everything a [`SqlDb`] does is available directly (it derefs to one):
/// `execute`, `execute_with`, `query`, `query_as`, `query_scalar`,
/// `query_each`, `cursor`, `call`, `call_function`, `execute_script`.
///
/// `!Send` by design: the pool, its connections, and every future run on one
/// shard. Create one `PostgresDriver` per shard.
pub struct PostgresDriver {
    db: SqlDb<PgConnect>,
}

impl PostgresDriver {
    /// Connect from a `postgres://user:pass@host:port/db?…` URL (see
    /// [`PgConfig::from_url`]), opening one connection eagerly so a bad URL
    /// or bad credentials fail fast.
    pub async fn connect(url: &str) -> Result<Self, OrmError> {
        Self::connect_with(PgConfig::from_url(url)?, DEFAULT_POOL_SIZE).await
    }

    /// Connect from an explicit config, with a chosen pool size.
    pub async fn connect_with(cfg: PgConfig, pool_size: usize) -> Result<Self, OrmError> {
        Ok(Self { db: SqlDb::open(PgConnect(cfg), pool_size).await? })
    }

    /// The connection settings.
    pub fn config(&self) -> &PgConfig {
        &self.db.pool().connector().0
    }

    async fn with_conn<R>(&self, f: impl AsyncFnOnce(&mut Conn) -> Result<R, OrmError>) -> Result<R, OrmError> {
        let mut g = self.db.pool().acquire().await?;
        // Discarded if this future is dropped half-way (a COPY mid-stream).
        g.poison();
        let r = f(&mut g).await;
        if !(g.is_broken() || g.in_transaction()) {
            g.mark_healthy();
        }
        r
    }

    /// `COPY … FROM STDIN`: bulk-load `data` (text, CSV or binary, as the
    /// statement says); returns the rows copied.
    ///
    /// ```rust,ignore
    /// db.copy_in("COPY items (name, qty) FROM STDIN (FORMAT csv)", b"a,1\nb,2\n").await?;
    /// ```
    pub async fn copy_in(&self, sql: &str, data: &[u8]) -> Result<u64, OrmError> {
        self.with_conn(async |c| c.copy_in(sql, data).await).await
    }

    /// `COPY … TO STDOUT`: each chunk of output (one row in text / CSV
    /// format) goes to `each`; returns the rows copied.
    pub async fn copy_out(&self, sql: &str, each: impl FnMut(&[u8]) -> Result<(), OrmError>) -> Result<u64, OrmError> {
        self.with_conn(async |c| c.copy_out(sql, each).await).await
    }

    /// Send a notification on `channel` (`pg_notify`); delivered when the
    /// sending transaction commits (here: at once).
    pub async fn notify(&self, channel: &str, payload: &str) -> Result<(), OrmError> {
        self.db.execute_with("SELECT pg_catalog.pg_notify($1, $2)", &[Param::Str(channel.into()), Param::Str(payload.into())]).await.map(|_| ())
    }

    /// A connection of its own (outside the pool) for receiving
    /// notifications: [`Listener::listen`], then [`Listener::recv`].
    pub async fn listener(&self) -> Result<Listener, OrmError> {
        Ok(Listener { conn: Conn::connect(self.config()).await? })
    }
}

/// A dedicated connection that `LISTEN`s and receives notifications.
pub struct Listener {
    conn: Conn,
}

impl Listener {
    /// Start listening on `channel`.
    pub async fn listen(&mut self, channel: &str) -> Result<(), OrmError> {
        self.conn.simple(&format!("LISTEN {}", quote_ident(channel))).await
    }

    /// Stop listening on `channel`.
    pub async fn unlisten(&mut self, channel: &str) -> Result<(), OrmError> {
        self.conn.simple(&format!("UNLISTEN {}", quote_ident(channel))).await
    }

    /// The next notification, waiting for it. Cancel-safe: wrap it in a
    /// timeout to wait a bounded time.
    pub async fn recv(&mut self) -> Result<Notification, OrmError> {
        self.conn.wait_notification().await
    }

    /// Notifications already received, without waiting.
    pub fn try_recv(&mut self) -> Vec<Notification> {
        self.conn.take_notifications()
    }

    /// The underlying connection.
    pub fn conn(&mut self) -> &mut Conn {
        &mut self.conn
    }
}

fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

impl Deref for PostgresDriver {
    type Target = SqlDb<PgConnect>;

    fn deref(&self) -> &Self::Target {
        &self.db
    }
}

impl Driver for PostgresDriver {
    fn repository<T>(&self) -> Box<dyn Repository<T>>
    where
        T: Entity + Serialize + DeserializeOwned + 'static,
    {
        self.db.repository()
    }

    fn ping(&self) -> BoxFuture<'_, Result<(), OrmError>> {
        self.db.ping()
    }

    fn capabilities(&self) -> DriverCapabilities {
        Driver::capabilities(&self.db)
    }
}

impl TransactionalDriver for PostgresDriver {
    type Tx = PgTx;

    fn transaction<'a, R, F>(&'a self, work: F) -> BoxFuture<'a, Result<R, OrmError>>
    where
        F: FnOnce(PgTx) -> BoxFuture<'a, Result<R, OrmError>> + 'a,
        R: 'a,
    {
        self.db.transaction(work)
    }
}
