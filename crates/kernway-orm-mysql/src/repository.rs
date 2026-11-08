//! `MySqlRepository<T>` — the autocommit CRUD surface.
//!
//! Each method acquires a connection from the per-core pool and runs the async
//! engine directly on the reactor — no blocking pool, no thread hop. A
//! connection-level error poisons the checked-out connection so it is discarded
//! rather than returned.

use crate::engine;
use crate::pool::{Pool, PooledConn};
use crate::query::MySqlQueryBuilder;
use kernway_orm_core::{
    entity::Entity, error::OrmError, query::QueryBuilder, repository::Repository, BoxFuture,
};
use serde::{de::DeserializeOwned, Serialize};
use std::marker::PhantomData;
use std::rc::Rc;

/// MySQL repository over a shared per-core connection pool.
pub struct MySqlRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    pool: Rc<Pool>,
    _marker: PhantomData<T>,
}

impl<T> MySqlRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    /// Build a repository over a shared pool. Schema is **not** created here — in
    /// the async model use [`crate::MySqlDriver::ensure_table`] explicitly.
    pub fn new(pool: Rc<Pool>) -> Self {
        Self { pool, _marker: PhantomData }
    }
}

/// Discard the checked-out connection if the error was connection-level.
fn poison_if_conn_err<R>(guard: &mut PooledConn, result: &Result<R, OrmError>) {
    if matches!(result, Err(OrmError::Connection(_))) {
        guard.poison();
    }
}

impl<T> Repository<T> for MySqlRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    fn find_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        let id = id.clone();
        let pool = Rc::clone(&self.pool);
        Box::pin(async move {
            let mut g = pool.acquire().await?;
            let r = engine::find_by_id::<T>(&mut g, &id, false).await;
            poison_if_conn_err(&mut g, &r);
            r
        })
    }

    fn find_all<'a>(&'a self) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        let pool = Rc::clone(&self.pool);
        Box::pin(async move {
            let mut g = pool.acquire().await?;
            let r = engine::find_all::<T>(&mut g).await;
            poison_if_conn_err(&mut g, &r);
            r
        })
    }

    fn find_all_by_ids<'a>(&'a self, ids: &'a [T::Id]) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        let ids = ids.to_vec();
        let pool = Rc::clone(&self.pool);
        Box::pin(async move {
            let mut g = pool.acquire().await?;
            let r = engine::find_all_by_ids::<T>(&mut g, &ids).await;
            poison_if_conn_err(&mut g, &r);
            r
        })
    }

    fn count<'a>(&'a self) -> BoxFuture<'a, Result<u64, OrmError>> {
        let pool = Rc::clone(&self.pool);
        Box::pin(async move {
            let mut g = pool.acquire().await?;
            let r = engine::count::<T>(&mut g).await;
            poison_if_conn_err(&mut g, &r);
            r
        })
    }

    fn exists_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<bool, OrmError>> {
        let id = id.clone();
        let pool = Rc::clone(&self.pool);
        Box::pin(async move {
            let mut g = pool.acquire().await?;
            let r = engine::exists_by_id::<T>(&mut g, &id).await;
            poison_if_conn_err(&mut g, &r);
            r
        })
    }

    fn save<'a>(&'a self, entity: T) -> BoxFuture<'a, Result<T, OrmError>> {
        let pool = Rc::clone(&self.pool);
        Box::pin(async move {
            let mut g = pool.acquire().await?;
            let r = engine::save::<T>(&mut g, entity).await;
            poison_if_conn_err(&mut g, &r);
            r
        })
    }

    fn save_all<'a>(&'a self, entities: Vec<T>) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        let pool = Rc::clone(&self.pool);
        Box::pin(async move {
            let mut g = pool.acquire().await?;
            let r = engine::save_all::<T>(&mut g, entities).await;
            poison_if_conn_err(&mut g, &r);
            r
        })
    }

    fn delete_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>> {
        let id = id.clone();
        let pool = Rc::clone(&self.pool);
        Box::pin(async move {
            let mut g = pool.acquire().await?;
            let r = engine::delete_by_id::<T>(&mut g, &id).await;
            poison_if_conn_err(&mut g, &r);
            r
        })
    }

    fn delete_all_by_ids<'a>(&'a self, ids: &'a [T::Id]) -> BoxFuture<'a, Result<(), OrmError>> {
        let ids = ids.to_vec();
        let pool = Rc::clone(&self.pool);
        Box::pin(async move {
            let mut g = pool.acquire().await?;
            let r = engine::delete_all_by_ids::<T>(&mut g, &ids).await;
            poison_if_conn_err(&mut g, &r);
            r
        })
    }

    fn query(&self) -> Box<dyn QueryBuilder<T>> {
        Box::new(MySqlQueryBuilder::<T>::new(Rc::clone(&self.pool)))
    }
}
