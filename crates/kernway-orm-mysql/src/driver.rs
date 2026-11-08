//! `MySqlDriver` — the entry point, backed by the native async protocol and a
//! per-core connection pool.

use crate::engine;
use crate::pool::Pool;
use crate::protocol::{MysqlConfig, QueryResult};
use crate::repository::MySqlRepository;
use crate::transaction::{MySqlTx, SharedConn};
use kernway_orm_core::{
    driver::{Driver, DriverCapabilities},
    entity::Entity,
    error::OrmError,
    repository::Repository,
    transaction::TransactionalDriver,
    BoxFuture,
};
use serde::{de::DeserializeOwned, Serialize};
use std::cell::RefCell;
use std::rc::Rc;

/// Default per-core pool size — connections opened on demand, up to this many.
const DEFAULT_POOL_SIZE: usize = 8;

/// A configured handle to a MySQL database, backed by a per-core async pool.
///
/// `!Send` by design: the pool, its connections, and every future run on one
/// shard. Create one `MySqlDriver` per shard.
pub struct MySqlDriver {
    pool: Rc<Pool>,
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
        let pool = Pool::new(cfg, pool_size);
        pool.warm().await?;
        Ok(Self { pool })
    }

    /// Create `T`'s table if it does not exist (explicit schema step — the async
    /// repository does not auto-create).
    pub async fn ensure_table<T>(&self) -> Result<(), OrmError>
    where
        T: Entity,
    {
        let mut guard = self.pool.acquire().await?;
        engine::ensure_table::<T>(&mut guard).await
    }
}

impl Driver for MySqlDriver {
    fn repository<T>(&self) -> Box<dyn Repository<T>>
    where
        T: Entity + Serialize + DeserializeOwned + 'static,
    {
        Box::new(MySqlRepository::<T>::new(Rc::clone(&self.pool)))
    }

    fn ping(&self) -> BoxFuture<'_, Result<(), OrmError>> {
        let pool = Rc::clone(&self.pool);
        Box::pin(async move {
            let mut guard = pool.acquire().await?;
            match guard.query("SELECT 1").await? {
                QueryResult::Rows { .. } | QueryResult::Ok { .. } => Ok(()),
            }
        })
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

impl TransactionalDriver for MySqlDriver {
    type Tx = MySqlTx;

    fn transaction<'a, R, F>(&'a self, work: F) -> BoxFuture<'a, Result<R, OrmError>>
    where
        F: FnOnce(MySqlTx) -> BoxFuture<'a, Result<R, OrmError>> + 'a,
        R: 'a,
    {
        let pool = Rc::clone(&self.pool);
        Box::pin(async move {
            let mut guard = pool.acquire().await?;
            guard.query("START TRANSACTION").await?;

            // Share the connection with the handle; reclaim it after the closure
            // to commit or roll back.
            let cell: SharedConn = Rc::new(RefCell::new(Some(guard)));
            let tx = MySqlTx { conn: Rc::clone(&cell) };
            let result = work(tx).await;

            let mut guard = cell
                .borrow_mut()
                .take()
                .ok_or_else(|| OrmError::Transaction("connection lost during transaction".into()))?;
            match result {
                Ok(value) => {
                    guard.query("COMMIT").await?;
                    Ok(value)
                }
                Err(err) => {
                    let _ = guard.query("ROLLBACK").await;
                    Err(err)
                }
            }
        })
    }
}
