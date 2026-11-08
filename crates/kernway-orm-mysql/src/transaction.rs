//! Transaction support for the async MySQL backend.
//!
//! A transaction owns one pooled connection for the whole closure. Its
//! repositories share that connection through `Rc<RefCell<Option<PooledConn>>>`
//! — single-shard, so no locks. `START TRANSACTION` → `work` → `COMMIT` on `Ok`,
//! `ROLLBACK` on `Err` (driven in [`crate::driver`]).

use crate::engine;
use crate::pool::PooledConn;
use crate::protocol::Conn;
use kernway_orm_core::{
    entity::Entity,
    error::OrmError,
    transaction::{Transaction, TxRepository},
    BoxFuture,
};
use serde::{de::DeserializeOwned, Serialize};
use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;

/// The transaction's connection, shared between the handle, its repositories,
/// and the driver (which reclaims it to commit/rollback). `None` once reclaimed.
pub(crate) type SharedConn = Rc<RefCell<Option<PooledConn>>>;

fn finished() -> OrmError {
    OrmError::Transaction("transaction already finished".into())
}

/// A live MySQL transaction handle.
pub struct MySqlTx {
    pub(crate) conn: SharedConn,
}

impl Transaction for MySqlTx {
    fn repository<T>(&self) -> Box<dyn TxRepository<T> + '_>
    where
        T: Entity + Serialize + DeserializeOwned + 'static,
    {
        Box::new(MySqlTxRepository::<T> {
            conn: Rc::clone(&self.conn),
            _marker: PhantomData,
        })
    }
}

struct MySqlTxRepository<T> {
    conn: SharedConn,
    _marker: PhantomData<T>,
}

impl<T> TxRepository<T> for MySqlTxRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    fn find_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        let conn = Rc::clone(&self.conn);
        let id = id.clone();
        Box::pin(async move {
            let mut slot = conn.borrow_mut();
            let c: &mut Conn = slot.as_mut().ok_or_else(finished)?;
            engine::find_by_id::<T>(c, &id, false).await
        })
    }

    fn find_by_id_for_update<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        let conn = Rc::clone(&self.conn);
        let id = id.clone();
        Box::pin(async move {
            let mut slot = conn.borrow_mut();
            let c: &mut Conn = slot.as_mut().ok_or_else(finished)?;
            engine::find_by_id::<T>(c, &id, true).await
        })
    }

    fn save<'a>(&'a self, entity: T) -> BoxFuture<'a, Result<T, OrmError>> {
        let conn = Rc::clone(&self.conn);
        Box::pin(async move {
            let mut slot = conn.borrow_mut();
            let c: &mut Conn = slot.as_mut().ok_or_else(finished)?;
            engine::save::<T>(c, entity).await
        })
    }

    fn delete_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>> {
        let conn = Rc::clone(&self.conn);
        let id = id.clone();
        Box::pin(async move {
            let mut slot = conn.borrow_mut();
            let c: &mut Conn = slot.as_mut().ok_or_else(finished)?;
            engine::delete_by_id::<T>(c, &id).await
        })
    }
}
