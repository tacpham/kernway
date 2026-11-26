//! The transaction scope shared by every SQL backend: one pooled connection
//! for the whole closure, shared with its repositories through
//! `Rc<RefCell<…>>` (single shard, no locks); commit on `Ok`, roll back on
//! `Err`.

use crate::engine;
use crate::raw::{CallArg, CallResult, Outcome, Rows, Value};
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
    let mut guard = pool.acquire().await?;
    if let Err(e) = guard.begin().await {
        if matches!(e, OrmError::Connection(_)) {
            guard.poison();
        }
        return Err(e);
    }
    let cell: SharedConn<C> = Rc::new(RefCell::new(Some(guard)));
    let result = work(SqlTx { conn: Rc::clone(&cell) }).await;
    let mut guard = cell
        .borrow_mut()
        .take()
        .ok_or_else(|| OrmError::Transaction("connection lost during transaction".into()))?;
    match result {
        Ok(value) => match guard.commit().await {
            Ok(()) => Ok(value),
            Err(e) => {
                if matches!(e, OrmError::Connection(_)) {
                    guard.poison();
                }
                Err(e)
            }
        },
        Err(err) => {
            if guard.rollback().await.is_err() {
                guard.poison();
            }
            Err(err)
        }
    }
}
