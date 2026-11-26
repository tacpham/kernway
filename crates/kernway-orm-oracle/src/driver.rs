//! `OracleDriver` — the ORM entry point over a per-core session pool. The
//! repositories, query builder and transactions are the shared SQL layer.

use crate::conn::{Conn, OracleConfig};
use crate::pool::{OracleConnect, Pool};
use kernway_orm_core::{
    driver::{Driver, DriverCapabilities},
    entity::Entity,
    error::OrmError,
    repository::Repository,
    transaction::TransactionalDriver,
    BoxFuture,
};
use kernway_orm_sql::{engine, SqlConn, SqlRepository, SqlTx};
use serde::{de::DeserializeOwned, Serialize};
use std::rc::Rc;

const DEFAULT_POOL_SIZE: usize = 8;

/// A configured handle to an Oracle database, backed by a per-core pool.
/// `!Send` by design: create one per shard.
pub struct OracleDriver {
    pool: Rc<Pool>,
}

impl OracleDriver {
    /// Connect from an `oracle://user:password@host[:port]/service[?ssl-mode=…]`
    /// URL, opening one session eagerly so bad credentials fail fast.
    pub async fn connect(url: &str) -> Result<Self, OrmError> {
        Self::connect_with(OracleConfig::from_url(url)?, DEFAULT_POOL_SIZE).await
    }

    /// Connect from an explicit config with a chosen pool size.
    pub async fn connect_with(cfg: OracleConfig, pool_size: usize) -> Result<Self, OrmError> {
        let pool = Pool::new(OracleConnect(cfg), pool_size);
        pool.warm().await?;
        Ok(Self { pool })
    }

    /// Create `T`'s table if it does not exist.
    pub async fn ensure_table<T: Entity>(&self) -> Result<(), OrmError> {
        let mut g = self.pool.acquire().await?;
        engine::ensure_table::<T, Conn>(&mut g).await
    }

    /// Run one raw SQL statement or PL/SQL block without binds, committed.
    pub async fn execute(&self, sql: &str) -> Result<(), OrmError> {
        let mut g = self.pool.acquire().await?;
        let r = SqlConn::execute(&mut *g, sql, &[]).await;
        if matches!(r, Err(OrmError::Connection(_))) {
            g.poison();
        }
        r.map(|_| ())
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
        Box::new(OracleRepository::<T>::new(Rc::clone(&self.pool)))
    }

    fn ping(&self) -> BoxFuture<'_, Result<(), OrmError>> {
        Box::pin(async move { self.execute("SELECT 1 FROM dual").await })
    }

    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities {
            transactions: true,
            raw_query: true,
            full_text_search: false,
            json_columns: true,
            migrations: false,
        }
    }
}

impl TransactionalDriver for OracleDriver {
    type Tx = OracleTx;

    fn transaction<'a, R, F>(&'a self, work: F) -> BoxFuture<'a, Result<R, OrmError>>
    where
        F: FnOnce(OracleTx) -> BoxFuture<'a, Result<R, OrmError>> + 'a,
        R: 'a,
    {
        Box::pin(kernway_orm_sql::run_transaction(Rc::clone(&self.pool), work))
    }
}
