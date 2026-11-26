//! [`SqlDb`]: the pooled database handle every SQL driver is built on.

use crate::engine;
use crate::raw::{CallArg, CallResult, Outcome, Rows, Value};
use crate::{run_transaction, Param, SqlConn, SqlConnect, SqlRepository, SqlTx};
use kernway_orm_core::{
    driver::{Driver, DriverCapabilities},
    entity::Entity,
    error::OrmError,
    repository::Repository,
    transaction::TransactionalDriver,
    BoxFuture,
};
use kernway_wire::{Pool, Pooled};
use serde::{de::DeserializeOwned, Serialize};
use std::rc::Rc;

/// A database: a per-core pool of `C` connections plus everything the ORM
/// does with it. Drivers wrap one (`MySqlDriver`, `OracleDriver`) and add only
/// how to connect and what is truly their own.
///
/// `!Send` by design: the pool, its connections and every future stay on one
/// shard. Create one per shard.
pub struct SqlDb<C: SqlConnect> {
    pool: Rc<Pool<C>>,
}

impl<C: SqlConnect> Clone for SqlDb<C> {
    fn clone(&self) -> Self {
        Self { pool: Rc::clone(&self.pool) }
    }
}

/// Discard the connection if the error was connection-level.
fn settle<R, C: SqlConnect>(g: &mut Pooled<C>, r: Result<R, OrmError>) -> Result<R, OrmError> {
    if matches!(r, Err(OrmError::Connection(_))) {
        g.poison();
    }
    r
}

impl<C: SqlConnect> SqlDb<C> {
    /// A database over `connect`, holding up to `size` connections; one is
    /// opened eagerly so bad settings fail fast.
    pub async fn open(connect: C, size: usize) -> Result<Self, OrmError> {
        let pool = Pool::new(connect, size);
        pool.warm().await?;
        Ok(Self { pool })
    }

    /// The connection pool (for backend-specific operations).
    pub fn pool(&self) -> &Rc<Pool<C>> {
        &self.pool
    }

    /// Create `T`'s table if it does not exist.
    pub async fn ensure_table<T: Entity>(&self) -> Result<(), OrmError> {
        let mut g = self.pool.acquire().await?;
        let r = engine::ensure_table::<T, C::Sql>(&mut g).await;
        settle(&mut g, r)
    }

    /// Run one statement without parameters and discard any rows — DDL,
    /// `CREATE DATABASE`, setup. One statement per call (see
    /// [`execute_script`](Self::execute_script) for several).
    pub async fn execute(&self, sql: &str) -> Result<(), OrmError> {
        self.execute_with(sql, &[]).await.map(|_| ())
    }

    /// Run one statement with bound parameters; returns the rows affected.
    pub async fn execute_with(&self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        let mut g = self.pool.acquire().await?;
        let r = SqlConn::execute(&mut *g, sql, params).await;
        settle(&mut g, r)
    }

    /// Run one INSERT / UPDATE / DELETE for every row of `rows` (array DML
    /// where the database has it); returns the total rows affected. Outside
    /// a transaction a failure part-way leaves earlier rows applied — run it
    /// inside [`transaction`](kernway_orm_core::transaction::TransactionalDriver::transaction)
    /// for all-or-nothing.
    pub async fn execute_many(&self, sql: &str, rows: &[Vec<Param>]) -> Result<u64, OrmError> {
        let mut g = self.pool.acquire().await?;
        let r = g.execute_many(sql, rows).await;
        settle(&mut g, r)
    }

    /// Run any query with bound parameters — joins, aggregates, subqueries,
    /// window functions — and collect its rows as typed [`Value`]s.
    pub async fn query(&self, sql: &str, params: &[Param]) -> Result<Rows, OrmError> {
        let mut g = self.pool.acquire().await?;
        let r = g.query_rows(sql, params).await;
        settle(&mut g, r)
    }

    /// Run a query and map its rows onto `T` by column name (see
    /// [`Rows::into_entities`]).
    pub async fn query_as<T: Entity + DeserializeOwned>(&self, sql: &str, params: &[Param]) -> Result<Vec<T>, OrmError> {
        self.query(sql, params).await?.into_entities()
    }

    /// Run a query and return the first column of its first row (`Null` when
    /// there is no row).
    pub async fn query_scalar(&self, sql: &str, params: &[Param]) -> Result<Value, OrmError> {
        let rows = self.query(sql, params).await?;
        Ok(rows.rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null))
    }

    /// Run a statement and stream its rows to `each` without collecting them.
    pub async fn query_each<F>(&self, sql: &str, params: &[Param], each: F) -> Result<Outcome, OrmError>
    where
        F: FnMut(Vec<Value>) -> Result<(), OrmError>,
    {
        let mut g = self.pool.acquire().await?;
        let r = g.query_each(sql, params, each).await;
        settle(&mut g, r)
    }

    /// Call a stored procedure: its result sets, and its `OUT` / `IN OUT`
    /// argument values in order.
    pub async fn call(&self, name: &str, args: &[CallArg]) -> Result<CallResult, OrmError> {
        let mut g = self.pool.acquire().await?;
        let r = g.call(name, args).await;
        settle(&mut g, r)
    }

    /// Run a script of several `;`-separated statements.
    pub async fn execute_script(&self, script: &str) -> Result<(), OrmError> {
        let mut g = self.pool.acquire().await?;
        let r = g.execute_script(script).await;
        settle(&mut g, r)
    }
}

impl<C: SqlConnect> Driver for SqlDb<C> {
    fn repository<T>(&self) -> Box<dyn Repository<T>>
    where
        T: Entity + Serialize + DeserializeOwned + 'static,
    {
        Box::new(SqlRepository::<T, C>::new(Rc::clone(&self.pool)))
    }

    fn ping(&self) -> BoxFuture<'_, Result<(), OrmError>> {
        Box::pin(async move {
            let mut g = self.pool.acquire().await?;
            let d = <C::Sql as SqlConn>::dialect();
            let r = g.query_rows(d.ping_sql(), &[]).await.map(|_| ());
            settle(&mut g, r)
        })
    }

    fn capabilities(&self) -> DriverCapabilities {
        <C::Sql as SqlConn>::capabilities()
    }
}

impl<C: SqlConnect> TransactionalDriver for SqlDb<C> {
    type Tx = SqlTx<C>;

    fn transaction<'a, R, F>(&'a self, work: F) -> BoxFuture<'a, Result<R, OrmError>>
    where
        F: FnOnce(SqlTx<C>) -> BoxFuture<'a, Result<R, OrmError>> + 'a,
        R: 'a,
    {
        Box::pin(run_transaction(Rc::clone(&self.pool), work))
    }
}
