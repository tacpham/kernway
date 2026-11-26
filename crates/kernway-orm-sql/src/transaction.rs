//! The transaction scope shared by every SQL backend: one pooled connection
//! for the whole closure, shared with its repositories through
//! `Rc<RefCell<…>>` (single shard, no locks); commit on `Ok`, roll back on
//! `Err`.

use crate::engine;
use crate::raw::{CallArg, CallResult, Outcome, Rows, ServerCursor, Value};
use crate::{Param, SqlConn, SqlConnect};
use kernway_orm_core::{
    entity::Entity,
    error::OrmError,
    transaction::{Transaction, TxRepository},
    BoxFuture,
};
use kernway_wire::{Pool, Pooled};
use serde::{de::DeserializeOwned, Serialize};
use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;

type SharedConn<C> = Rc<RefCell<Option<Pooled<C>>>>;

fn finished() -> OrmError {
    OrmError::Transaction("transaction already finished".into())
}

/// A live transaction handle over a `C` connection.
pub struct SqlTx<C: SqlConnect> {
    conn: SharedConn<C>,
}

#[allow(clippy::await_holding_refcell_ref)] // one statement at a time on the transaction's connection
impl<C: SqlConnect> SqlTx<C> {
    fn conn(&self) -> Result<std::cell::RefMut<'_, Pooled<C>>, OrmError> {
        let slot = self.conn.borrow_mut();
        if slot.is_none() {
            return Err(finished());
        }
        Ok(std::cell::RefMut::map(slot, |s| s.as_mut().expect("checked above")))
    }

    /// Run a statement with bound parameters inside the transaction; returns
    /// the rows affected.
    pub async fn execute_with(&self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        SqlConn::execute(&mut **self.conn()?, sql, params).await
    }

    /// Run one DML statement per row of `rows` inside the transaction.
    pub async fn execute_many(&self, sql: &str, rows: &[Vec<Param>]) -> Result<u64, OrmError> {
        self.conn()?.execute_many(sql, rows).await
    }

    /// Run a query inside the transaction and collect its rows.
    pub async fn query(&self, sql: &str, params: &[Param]) -> Result<Rows, OrmError> {
        self.conn()?.query_rows(sql, params).await
    }

    /// Run a query inside the transaction and map its rows onto `T` by name.
    pub async fn query_as<T: Entity + DeserializeOwned>(&self, sql: &str, params: &[Param]) -> Result<Vec<T>, OrmError> {
        self.query(sql, params).await?.into_entities()
    }

    /// Stream a query's rows inside the transaction.
    pub async fn query_each<F>(&self, sql: &str, params: &[Param], each: F) -> Result<Outcome, OrmError>
    where
        F: FnMut(Vec<Value>) -> Result<(), OrmError>,
    {
        self.conn()?.query_each(sql, params, each).await
    }

    /// Open a server-side cursor inside the transaction: fetch its rows in
    /// batches with [`fetch`](Self::fetch) and run other statements on the
    /// transaction in between.
    pub async fn open_cursor(&self, sql: &str, params: &[Param]) -> Result<ServerCursor, OrmError> {
        self.conn()?.cursor_open(sql, params).await
    }

    /// The next batch of up to `max_rows` rows of `cursor`; `None` once every
    /// row has been fetched.
    pub async fn fetch(&self, cursor: &mut ServerCursor, max_rows: usize) -> Result<Option<Rows>, OrmError> {
        if cursor.is_done() {
            return Ok(None);
        }
        let rows = self.conn()?.cursor_fetch(cursor, max_rows).await?;
        if rows.is_empty() && cursor.is_done() {
            return Ok(None);
        }
        Ok(Some(Rows { columns: cursor.columns().to_vec(), rows }))
    }

    /// Release a cursor before its end.
    pub async fn close_cursor(&self, cursor: ServerCursor) -> Result<(), OrmError> {
        self.conn()?.cursor_close(cursor).await
    }

    /// Call a stored function inside the transaction.
    pub async fn call_function(&self, name: &str, returns: crate::OutKind, args: &[CallArg]) -> Result<CallResult, OrmError> {
        self.conn()?.call_function(name, returns, args).await
    }

    /// Call a stored procedure inside the transaction.
    pub async fn call(&self, name: &str, args: &[CallArg]) -> Result<CallResult, OrmError> {
        self.conn()?.call(name, args).await
    }
}

impl<C: SqlConnect> Transaction for SqlTx<C> {
    fn repository<T>(&self) -> Box<dyn TxRepository<T> + '_>
    where
        T: Entity + Serialize + DeserializeOwned + 'static,
    {
        Box::new(SqlTxRepository::<T, C> { conn: Rc::clone(&self.conn), _marker: PhantomData })
    }
}

struct SqlTxRepository<T, C: SqlConnect> {
    conn: SharedConn<C>,
    _marker: PhantomData<T>,
}

macro_rules! in_tx {
    ($conn:expr, |$c:ident| $body:expr) => {{
        let conn = Rc::clone($conn);
        Box::pin(async move {
            // One connection serves the whole transaction; its statements run
            // one at a time, so the borrow is held for the call (a second
            // concurrent call on the same transaction would be a bug).
            let mut slot = conn.borrow_mut();
            let $c: &mut C::Sql = &mut *slot.as_mut().ok_or_else(finished)?;
            $body.await
        })
    }};
}

#[allow(clippy::await_holding_refcell_ref)]
impl<T, C> TxRepository<T> for SqlTxRepository<T, C>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
    C: SqlConnect,
{
    fn find_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        in_tx!(&self.conn, |c| engine::find_by_id::<T, C::Sql>(c, id, false))
    }
    fn find_by_id_for_update<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        in_tx!(&self.conn, |c| engine::find_by_id::<T, C::Sql>(c, id, true))
    }
    fn save<'a>(&'a self, entity: T) -> BoxFuture<'a, Result<T, OrmError>> {
        in_tx!(&self.conn, |c| engine::save::<T, C::Sql>(c, entity))
    }
    fn delete_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>> {
        in_tx!(&self.conn, |c| engine::delete_by_id::<T, C::Sql>(c, id))
    }
}

/// Run `work` in a transaction on one pooled connection: begin, then commit
/// if it returns `Ok`, roll back if it returns `Err`.
pub async fn run_transaction<'a, C, R, F>(pool: Rc<Pool<C>>, work: F) -> Result<R, OrmError>
where
    C: SqlConnect,
    F: FnOnce(SqlTx<C>) -> BoxFuture<'a, Result<R, OrmError>> + 'a,
    R: 'a,
{
    let guard = pool.acquire().await?;
    run_transaction_on(guard, work).await.1
}

/// [`run_transaction`] on a connection the caller already holds, which it
/// gets back afterwards (`None` if the transaction lost it) — for work that
/// must happen on the same connection before and after the transaction
/// (SQLite's change sessions record one connection's changes).
pub async fn run_transaction_on<'a, C, R, F>(mut guard: Pooled<C>, work: F) -> (Option<Pooled<C>>, Result<R, OrmError>)
where
    C: SqlConnect,
    F: FnOnce(SqlTx<C>) -> BoxFuture<'a, Result<R, OrmError>> + 'a,
    R: 'a,
{
    // Poisoned until the transaction has ended: dropped half-way (a timeout
    // around the whole transaction), the connection — still inside the
    // transaction — is discarded, never handed to the next caller.
    guard.poison();
    if let Err(e) = guard.begin().await {
        if !matches!(e, OrmError::Connection(_)) {
            guard.mark_healthy();
        }
        return (Some(guard), Err(e));
    }
    let cell: SharedConn<C> = Rc::new(RefCell::new(Some(guard)));
    let result = work(SqlTx { conn: Rc::clone(&cell) }).await;
    let Some(mut guard) = cell.borrow_mut().take() else {
        return (None, Err(OrmError::Transaction("connection lost during transaction".into())));
    };
    let result = match result {
        Ok(value) => match guard.commit().await {
            Ok(()) => {
                guard.mark_healthy();
                Ok(value)
            }
            Err(e) => {
                // A refused commit (a deferred constraint) leaves the
                // transaction to roll back; a dead connection stays poisoned.
                if !matches!(e, OrmError::Connection(_)) && guard.rollback().await.is_ok() {
                    guard.mark_healthy();
                }
                Err(e)
            }
        },
        Err(err) => {
            if guard.rollback().await.is_ok() {
                guard.mark_healthy();
            }
            Err(err)
        }
    };
    (Some(guard), result)
}
