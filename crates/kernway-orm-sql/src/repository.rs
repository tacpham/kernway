//! The pooled, autocommit repository shared by every SQL backend.

use crate::engine;
use crate::query::SqlQueryBuilder;
use crate::SqlConnect;
use kernway_orm_core::{entity::Entity, error::OrmError, query::QueryBuilder, repository::Repository, BoxFuture};
use kernway_wire::{Pool, Pooled};
use serde::{de::DeserializeOwned, Serialize};
use std::marker::PhantomData;
use std::rc::Rc;

/// A repository over a shared per-core pool of `C` connections. Each call
/// checks out a connection; a connection-level error discards it.
pub struct SqlRepository<T, C: SqlConnect> {
    pool: Rc<Pool<C>>,
    _marker: PhantomData<T>,
}

impl<T, C: SqlConnect> SqlRepository<T, C> {
    /// A repository over `pool`.
    pub fn new(pool: Rc<Pool<C>>) -> Self {
        Self { pool, _marker: PhantomData }
    }
}

/// Discard the checked-out connection if the error was connection-level.
fn settle<R, C: SqlConnect>(guard: &mut Pooled<C>, result: Result<R, OrmError>) -> Result<R, OrmError> {
    if matches!(result, Err(OrmError::Connection(_))) {
        guard.poison();
    }
    result
}

macro_rules! pooled {
    ($pool:expr, |$c:ident| $body:expr) => {{
        let pool = Rc::clone($pool);
        Box::pin(async move {
            let mut g = crate::db::checkout(&pool).await?;
            let $c = &mut *g;
            let r = $body.await;
            settle(&mut g, r)
        })
    }};
}

impl<T, C> Repository<T> for SqlRepository<T, C>
where
    T: Entity + Serialize + DeserializeOwned + 'static,
    T::Id: Serialize + DeserializeOwned,
    C: SqlConnect,
{
    fn find_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        pooled!(&self.pool, |c| engine::find_by_id::<T, C::Sql>(c, id, false))
    }
    fn find_all<'a>(&'a self) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        pooled!(&self.pool, |c| engine::find_all::<T, C::Sql>(c))
    }
    fn find_all_by_ids<'a>(&'a self, ids: &'a [T::Id]) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        pooled!(&self.pool, |c| engine::find_all_by_ids::<T, C::Sql>(c, ids))
    }
    fn count<'a>(&'a self) -> BoxFuture<'a, Result<u64, OrmError>> {
        pooled!(&self.pool, |c| engine::count::<T, C::Sql>(c))
    }
    fn exists_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<bool, OrmError>> {
        pooled!(&self.pool, |c| engine::exists_by_id::<T, C::Sql>(c, id))
    }
    fn save<'a>(&'a self, entity: T) -> BoxFuture<'a, Result<T, OrmError>> {
        pooled!(&self.pool, |c| engine::save::<T, C::Sql>(c, entity))
    }
    fn save_all<'a>(&'a self, entities: Vec<T>) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        pooled!(&self.pool, |c| engine::save_all::<T, C::Sql>(c, entities))
    }
    fn delete_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>> {
        pooled!(&self.pool, |c| engine::delete_by_id::<T, C::Sql>(c, id))
    }
    fn delete_all_by_ids<'a>(&'a self, ids: &'a [T::Id]) -> BoxFuture<'a, Result<(), OrmError>> {
        pooled!(&self.pool, |c| engine::delete_all_by_ids::<T, C::Sql>(c, ids))
    }
    fn query(&self) -> Box<dyn QueryBuilder<T>> {
        Box::new(SqlQueryBuilder::<T, C>::new(Rc::clone(&self.pool)))
    }
}
