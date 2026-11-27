//! `MssqlDriver` — the entry point: a [`SqlDb`] (pool, hand-written SQL,
//! procedures, repositories, transactions — all shared) plus how to connect.

use crate::config::MssqlConfig;
use crate::pool::MssqlConnect;
use crate::MssqlTx;
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

const DEFAULT_POOL_SIZE: usize = 8;

/// A configured handle to a SQL Server database, backed by a per-core pool.
/// Derefs to [`SqlDb`]: `execute`, `query`, `query_as`, `cursor`, `call`, …
/// `!Send` by design: create one per shard.
pub struct MssqlDriver {
    db: SqlDb<MssqlConnect>,
}

impl MssqlDriver {
    /// Connect from a `mssql://` URL or an ADO.NET connection string,
    /// opening one connection eagerly so bad settings fail fast.
    pub async fn connect(s: &str) -> Result<Self, OrmError> {
        Self::connect_with(MssqlConfig::parse(s)?, DEFAULT_POOL_SIZE).await
    }

    /// Connect from an explicit config, with a chosen pool size.
    pub async fn connect_with(cfg: MssqlConfig, pool_size: usize) -> Result<Self, OrmError> {
        Ok(Self { db: SqlDb::open(MssqlConnect(cfg), pool_size).await? })
    }
}

impl Deref for MssqlDriver {
    type Target = SqlDb<MssqlConnect>;
    fn deref(&self) -> &Self::Target {
        &self.db
    }
}

impl Driver for MssqlDriver {
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

impl TransactionalDriver for MssqlDriver {
    type Tx = MssqlTx;
    fn transaction<'a, R, F>(&'a self, work: F) -> BoxFuture<'a, Result<R, OrmError>>
    where
        F: FnOnce(MssqlTx) -> BoxFuture<'a, Result<R, OrmError>> + 'a,
        R: 'a,
    {
        self.db.transaction(work)
    }
}
