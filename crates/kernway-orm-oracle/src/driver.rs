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

impl OracleDriver {
    /// Open a scrollable cursor over a query on a pooled connection of its
    /// own: move with [`OracleScrollable::fetch`] (first / last / absolute /
    /// relative / next / prior), forwards and backwards.
    pub async fn scrollable(&self, sql: &str, params: &[kernway_orm_sql::Param]) -> Result<OracleScrollable, OrmError> {
        let mut conn = self.pool().acquire().await?;
        let cur = conn.open_scrollable(sql, &crate::backend::binds(params)).await;
        match cur {
            Ok(cur) => Ok(OracleScrollable { conn, cur: Some(cur) }),
            Err(e) => {
                if matches!(e, OrmError::Connection(_)) {
                    conn.poison();
                }
                Err(e)
            }
        }
    }
}

/// A scrollable cursor holding its pooled connection (see
/// [`OracleDriver::scrollable`]); closed when dropped.
pub struct OracleScrollable {
    conn: kernway_wire::Pooled<OracleConnect>,
    cur: Option<crate::exec::ScrollCursor>,
}

impl OracleScrollable {
    /// Move to `to` and fetch up to `max_rows` rows from there (none outside
    /// the result).
    pub async fn fetch(&mut self, to: crate::exec::Scroll, max_rows: u32) -> Result<kernway_orm_sql::Rows, OrmError> {
        let cur = self.cur.as_mut().ok_or_else(|| OrmError::Query("cursor closed".into()))?;
        let rows = match self.conn.scroll(cur, to, max_rows).await {
            Ok(r) => r,
            Err(e) => {
                if matches!(e, OrmError::Connection(_)) {
                    self.conn.poison();
                }
                return Err(e);
            }
        };
        let cols = cur.columns().to_vec();
        Ok(kernway_orm_sql::Rows {
            columns: cols.iter().map(|c| c.name.clone()).collect(),
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().zip(&cols).map(|(v, c)| crate::backend::to_shared(v, Some(c))).collect())
                .collect(),
        })
    }

    /// Row number (1-based) of the first row of the last batch.
    pub fn position(&self) -> u64 {
        self.cur.as_ref().map_or(0, crate::exec::ScrollCursor::position)
    }
}

impl Drop for OracleScrollable {
    fn drop(&mut self) {
        if let Some(cur) = self.cur.take() {
            self.conn.close_scrollable(cur);
        }
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
