//! [`SqlDb`]: the pooled database handle every SQL driver is built on.

use crate::engine;
use crate::raw::{CallArg, CallResult, OutKind, Outcome, Rows, ServerCursor, Value};
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
/// Check a connection out for one operation. It stays poisoned until
/// [`settle`] sees the operation finish, so a future dropped half-way never
/// returns a connection in an unknown state to the pool.
pub(crate) async fn checkout<C: SqlConnect>(pool: &Rc<Pool<C>>) -> Result<Pooled<C>, OrmError> {
    let mut g = pool.acquire().await?;
    g.poison();
    Ok(g)
}

/// The operation finished: the connection goes back to the pool unless it
/// failed at the connection level.
pub(crate) fn settle<R, C: SqlConnect>(g: &mut Pooled<C>, r: Result<R, OrmError>) -> Result<R, OrmError> {
    if matches!(r, Err(OrmError::Connection(_))) {
        g.poison();
    } else {
        g.mark_healthy();
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
        let mut g = checkout(&self.pool).await?;
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
        let mut g = checkout(&self.pool).await?;
        let r = SqlConn::execute(&mut *g, sql, params).await;
        settle(&mut g, r)
    }

    /// Run one INSERT / UPDATE / DELETE for every row of `rows` (array DML
    /// where the database has it); returns the total rows affected. Outside
    /// a transaction a failure part-way leaves earlier rows applied — run it
    /// inside [`transaction`](kernway_orm_core::transaction::TransactionalDriver::transaction)
    /// for all-or-nothing.
    pub async fn execute_many(&self, sql: &str, rows: &[Vec<Param>]) -> Result<u64, OrmError> {
        let mut g = checkout(&self.pool).await?;
        let r = g.execute_many(sql, rows).await;
        settle(&mut g, r)
    }

    /// Run any query with bound parameters — joins, aggregates, subqueries,
    /// window functions — and collect its rows as typed [`Value`]s.
    pub async fn query(&self, sql: &str, params: &[Param]) -> Result<Rows, OrmError> {
        let mut g = checkout(&self.pool).await?;
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

    /// Open a server-side cursor over a query on a connection of its own:
    /// fetch the rows in batches with [`DbCursor::next_batch`]. Within a
    /// transaction, use [`SqlTx::open_cursor`] to interleave other statements.
    pub async fn cursor(&self, sql: &str, params: &[Param]) -> Result<DbCursor<C>, OrmError> {
        let mut g = checkout(&self.pool).await?;
        let r = g.cursor_open(sql, params).await;
        let cursor = settle(&mut g, r)?;
        Ok(DbCursor { conn: Some(g), cursor: Some(cursor) })
    }

    /// Run a statement and stream its rows to `each` without collecting them.
    pub async fn query_each<F>(&self, sql: &str, params: &[Param], each: F) -> Result<Outcome, OrmError>
    where
        F: FnMut(Vec<Value>) -> Result<(), OrmError>,
    {
        let mut g = checkout(&self.pool).await?;
        let r = g.query_each(sql, params, each).await;
        settle(&mut g, r)
    }

    /// Call a stored procedure: its result sets, and its `OUT` / `IN OUT`
    /// argument values in order.
    pub async fn call(&self, name: &str, args: &[CallArg]) -> Result<CallResult, OrmError> {
        let mut g = checkout(&self.pool).await?;
        let r = g.call(name, args).await;
        settle(&mut g, r)
    }

    /// Call a stored function: its value is [`CallResult::value`].
    ///
    /// ```rust,ignore
    /// let total = db.call_function("order_total", OutKind::Number, &[42.into()]).await?.value;
    /// ```
    pub async fn call_function(&self, name: &str, returns: OutKind, args: &[CallArg]) -> Result<CallResult, OrmError> {
        let mut g = checkout(&self.pool).await?;
        let r = g.call_function(name, returns, args).await;
        settle(&mut g, r)
    }

    /// Run a script of several `;`-separated statements.
    pub async fn execute_script(&self, script: &str) -> Result<(), OrmError> {
        let mut g = checkout(&self.pool).await?;
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
            let mut g = checkout(&self.pool).await?;
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

/// A server-side cursor holding its pooled connection (see [`SqlDb::cursor`]).
/// Dropped before its end, the connection is discarded rather than pooled
/// (the server frees the cursor with it); [`close`](Self::close) releases it
/// cleanly instead.
pub struct DbCursor<C: SqlConnect> {
    conn: Option<Pooled<C>>,
    cursor: Option<ServerCursor>,
}

impl<C: SqlConnect> DbCursor<C> {
    /// Column names of the result.
    pub fn columns(&self) -> &[String] {
        self.cursor.as_ref().map_or(&[], ServerCursor::columns)
    }

    /// The next batch of up to `max_rows` rows; `None` once every row has
    /// been fetched (the connection then goes back to the pool).
    pub async fn next_batch(&mut self, max_rows: usize) -> Result<Option<Rows>, OrmError> {
        let (Some(g), Some(cursor)) = (self.conn.as_mut(), self.cursor.as_mut()) else {
            return Ok(None);
        };
        let r = g.cursor_fetch(cursor, max_rows).await;
        let rows = settle(g, r)?;
        let columns = cursor.columns().to_vec();
        if cursor.is_done() {
            self.cursor = None;
            self.conn = None; // back to the pool
        }
        if rows.is_empty() && self.cursor.is_none() {
            return Ok(None);
        }
        Ok(Some(Rows { columns, rows }))
    }

    /// Release the cursor and return the connection to the pool.
    pub async fn close(mut self) -> Result<(), OrmError> {
        if let (Some(mut g), Some(cursor)) = (self.conn.take(), self.cursor.take()) {
            let r = g.cursor_close(cursor).await;
            settle(&mut g, r)?;
        }
        Ok(())
    }
}

impl<C: SqlConnect> Drop for DbCursor<C> {
    fn drop(&mut self) {
        if self.cursor.is_some() {
            if let Some(g) = self.conn.as_mut() {
                g.poison();
            }
        }
    }
}
