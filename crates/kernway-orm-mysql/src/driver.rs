//! `MySqlDriver` — the entry point: a [`SqlDb`] (pool, hand-written SQL,
//! procedures, repositories, transactions — all shared) plus how to connect
//! and what only MySQL has (`LOAD DATA LOCAL INFILE`).

use crate::pool::MySqlConnect;
use crate::protocol::MysqlConfig;
use crate::MySqlTx;
use kernway_orm_core::{
    driver::{Driver, DriverCapabilities},
    entity::Entity,
    error::OrmError,
    repository::Repository,
    transaction::TransactionalDriver,
    BoxFuture,
};
use kernway_orm_sql::SqlDb;
use serde::{de::DeserializeOwned, Serialize};
use std::ops::Deref;

/// Default per-core pool size — connections opened on demand, up to this many.
const DEFAULT_POOL_SIZE: usize = 8;

/// A configured handle to a MySQL database, backed by a per-core async pool.
///
/// Everything a [`SqlDb`] does is available directly (it derefs to one):
/// `execute`, `execute_with`, `query`, `query_as`, `query_scalar`,
/// `query_each`, `call`, `execute_script`, `ensure_table`.
///
/// `!Send` by design: the pool, its connections, and every future run on one
/// shard. Create one `MySqlDriver` per shard.
pub struct MySqlDriver {
    db: SqlDb<MySqlConnect>,
}

impl MySqlDriver {
    /// Connect from a `mysql://user:pass@host:port/db` URL, opening one
    /// connection eagerly so a bad URL or auth fails fast.
    pub async fn connect(url: &str) -> Result<Self, OrmError> {
        let cfg = MysqlConfig::from_url(url)?;
        Self::connect_with(cfg, DEFAULT_POOL_SIZE).await
    }

    /// Connect from an explicit config, with a chosen pool size.
    pub async fn connect_with(cfg: MysqlConfig, pool_size: usize) -> Result<Self, OrmError> {
        Ok(Self { db: SqlDb::open(MySqlConnect(cfg), pool_size).await? })
    }

    /// Bulk-load `data` with `LOAD DATA LOCAL INFILE '<name>' INTO TABLE …`
    /// (the fastest way to import CSV); returns the rows loaded. Only `data`
    /// is ever sent — the client never reads a file the server names. Needs
    /// `local_infile = ON` on the server.
    ///
    /// ```rust,ignore
    /// db.load_local_infile(
    ///     "LOAD DATA LOCAL INFILE 'x.csv' INTO TABLE items FIELDS TERMINATED BY ','",
    ///     csv.as_bytes(),
    /// ).await?;
    /// ```
    pub async fn load_local_infile(&self, sql: &str, data: &[u8]) -> Result<u64, OrmError> {
        let mut g = self.db.pool().acquire().await?;
        let r = g.load_local_infile(sql, data).await;
        if matches!(r, Err(OrmError::Connection(_))) {
            g.poison();
        }
        r
    }
}

impl Deref for MySqlDriver {
    type Target = SqlDb<MySqlConnect>;

    fn deref(&self) -> &Self::Target {
        &self.db
    }
}

impl Driver for MySqlDriver {
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

impl TransactionalDriver for MySqlDriver {
    type Tx = MySqlTx;

    fn transaction<'a, R, F>(&'a self, work: F) -> BoxFuture<'a, Result<R, OrmError>>
    where
        F: FnOnce(MySqlTx) -> BoxFuture<'a, Result<R, OrmError>> + 'a,
        R: 'a,
    {
        self.db.transaction(work)
    }
}
