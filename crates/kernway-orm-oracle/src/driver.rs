//! `OracleDriver` — a [`SqlDb`] (pool, hand-written SQL, repositories,
//! transactions — all shared) plus how to connect to Oracle.

use crate::conn::OracleConfig;
use crate::pool::OracleConnect;
use kernway_orm_core::{
    driver::{Driver, DriverCapabilities},
    entity::Entity,
    error::OrmError,
    repository::Repository,
    transaction::TransactionalDriver,
    BoxFuture,
};
use kernway_orm_sql::{SqlDb, SqlRepository, SqlTx};
use serde::{de::DeserializeOwned, Serialize};
use std::ops::Deref;

const DEFAULT_POOL_SIZE: usize = 8;

/// A configured handle to an Oracle database, backed by a per-core pool.
/// Derefs to [`SqlDb`] for hand-written SQL. `!Send` by design: create one
/// per shard.
pub struct OracleDriver {
    db: SqlDb<OracleConnect>,
}

impl OracleDriver {
    /// Connect from an `oracle://user:password@host[:port]/service[?ssl-mode=…]`
    /// URL, opening one session eagerly so bad credentials fail fast.
    pub async fn connect(url: &str) -> Result<Self, OrmError> {
        Self::connect_with(OracleConfig::from_url(url)?, DEFAULT_POOL_SIZE).await
    }

    /// Connect from an explicit config with a chosen pool size.
    pub async fn connect_with(cfg: OracleConfig, pool_size: usize) -> Result<Self, OrmError> {
        Ok(Self { db: SqlDb::open(OracleConnect(cfg), pool_size).await? })
    }
}

impl Deref for OracleDriver {
    type Target = SqlDb<OracleConnect>;

    fn deref(&self) -> &Self::Target {
        &self.db
    }
}

/// The Oracle repository (the shared SQL repository over Oracle sessions).
pub type OracleRepository<T> = SqlRepository<T, OracleConnect>;
/// An Oracle transaction handle.
pub type OracleTx = SqlTx<OracleConnect>;

impl Driver for OracleDriver {
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

impl TransactionalDriver for OracleDriver {
    type Tx = OracleTx;

    fn transaction<'a, R, F>(&'a self, work: F) -> BoxFuture<'a, Result<R, OrmError>>
    where
        F: FnOnce(OracleTx) -> BoxFuture<'a, Result<R, OrmError>> + 'a,
        R: 'a,
    {
        self.db.transaction(work)
    }
}
