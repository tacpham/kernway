//! MySqlRepository — skeleton `Repository<T>` for MySQL.

use kernway_orm_core::{
    entity::Entity,
    error::OrmError,
    page::Page,
    query::QueryBuilder,
    repository::Repository,
    BoxFuture,
};
use serde::{de::DeserializeOwned, Serialize};
use std::marker::PhantomData;

/// MySQL repository skeleton.
pub struct MySqlRepository<T> {
    url: String,
    _marker: PhantomData<T>,
}

impl<T> MySqlRepository<T> {
    /// Create a new MySQL repository skeleton.
    pub fn new(url: String) -> Self {
        Self {
            url,
            _marker: PhantomData,
        }
    }
}

/// Query builder skeleton for MySQL.
pub struct MySqlQueryBuilder<T> {
    _marker: PhantomData<T>,
}

impl<T: Entity + Send + 'static> QueryBuilder<T> for MySqlQueryBuilder<T> {
    fn filter_eq(self: Box<Self>, _field: &'static str, _value: &str) -> Box<dyn QueryBuilder<T>> { self }
    fn filter_ne(self: Box<Self>, _field: &'static str, _value: &str) -> Box<dyn QueryBuilder<T>> { self }
    fn filter_gt(self: Box<Self>, _field: &'static str, _value: &str) -> Box<dyn QueryBuilder<T>> { self }
    fn filter_lt(self: Box<Self>, _field: &'static str, _value: &str) -> Box<dyn QueryBuilder<T>> { self }
    fn filter_gte(self: Box<Self>, _field: &'static str, _value: &str) -> Box<dyn QueryBuilder<T>> { self }
    fn filter_lte(self: Box<Self>, _field: &'static str, _value: &str) -> Box<dyn QueryBuilder<T>> { self }
    fn filter_like(self: Box<Self>, _field: &'static str, _pattern: &str) -> Box<dyn QueryBuilder<T>> { self }
    fn filter_in(self: Box<Self>, _field: &'static str, _values: Vec<String>) -> Box<dyn QueryBuilder<T>> { self }
    fn filter_between(self: Box<Self>, _field: &'static str, _from: &str, _to: &str) -> Box<dyn QueryBuilder<T>> { self }
    fn filter_is_null(self: Box<Self>, _field: &'static str) -> Box<dyn QueryBuilder<T>> { self }
    fn filter_is_not_null(self: Box<Self>, _field: &'static str) -> Box<dyn QueryBuilder<T>> { self }
    fn filter_spec(self: Box<Self>, _spec: kernway_orm_core::spec::Spec) -> Box<dyn QueryBuilder<T>> { self }
    fn order_by_asc(self: Box<Self>, _field: &'static str) -> Box<dyn QueryBuilder<T>> { self }
    fn order_by_desc(self: Box<Self>, _field: &'static str) -> Box<dyn QueryBuilder<T>> { self }
    fn limit(self: Box<Self>, _n: u64) -> Box<dyn QueryBuilder<T>> { self }
    fn offset(self: Box<Self>, _n: u64) -> Box<dyn QueryBuilder<T>> { self }
    fn with(self: Box<Self>, _relation: &'static str) -> Box<dyn QueryBuilder<T>> { self }
    fn fetch_all(self: Box<Self>) -> BoxFuture<'static, Result<Vec<T>, OrmError>> {
        Box::pin(async { Err(OrmError::Unsupported("MySqlQueryBuilder — not yet implemented".into())) })
    }
    fn fetch_one(self: Box<Self>) -> BoxFuture<'static, Result<Option<T>, OrmError>> {
        Box::pin(async { Err(OrmError::Unsupported("MySqlQueryBuilder — not yet implemented".into())) })
    }
    fn fetch_count(self: Box<Self>) -> BoxFuture<'static, Result<u64, OrmError>> {
        Box::pin(async { Err(OrmError::Unsupported("MySqlQueryBuilder — not yet implemented".into())) })
    }
    fn fetch_page(self: Box<Self>, _page: u64, _size: u64) -> BoxFuture<'static, Result<Page<T>, OrmError>> {
        Box::pin(async { Err(OrmError::Unsupported("MySqlQueryBuilder — not yet implemented".into())) })
    }
}

impl<T> Repository<T> for MySqlRepository<T>
where
    T: Entity + Serialize + DeserializeOwned + Send + 'static,
{
    fn find_by_id<'a>(&'a self, _id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        let _ = &self.url;
        Box::pin(async { Err(OrmError::Unsupported("MySqlRepository — not yet implemented".into())) })
    }
    fn find_all<'a>(&'a self) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        let _ = &self.url;
        Box::pin(async { Err(OrmError::Unsupported("MySqlRepository — not yet implemented".into())) })
    }
    fn find_all_by_ids<'a>(&'a self, _ids: &'a [T::Id]) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        let _ = &self.url;
        Box::pin(async { Err(OrmError::Unsupported("MySqlRepository — not yet implemented".into())) })
    }
    fn count<'a>(&'a self) -> BoxFuture<'a, Result<u64, OrmError>> {
        let _ = &self.url;
        Box::pin(async { Err(OrmError::Unsupported("MySqlRepository — not yet implemented".into())) })
    }
    fn exists_by_id<'a>(&'a self, _id: &'a T::Id) -> BoxFuture<'a, Result<bool, OrmError>> {
        let _ = &self.url;
        Box::pin(async { Err(OrmError::Unsupported("MySqlRepository — not yet implemented".into())) })
    }
    fn save<'a>(&'a self, _entity: T) -> BoxFuture<'a, Result<T, OrmError>> {
        let _ = &self.url;
        Box::pin(async move { Err(OrmError::Unsupported("MySqlRepository — not yet implemented".into())) })
    }
    fn save_all<'a>(&'a self, _entities: Vec<T>) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        let _ = &self.url;
        Box::pin(async { Err(OrmError::Unsupported("MySqlRepository — not yet implemented".into())) })
    }
    fn delete_by_id<'a>(&'a self, _id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>> {
        let _ = &self.url;
        Box::pin(async { Err(OrmError::Unsupported("MySqlRepository — not yet implemented".into())) })
    }
    fn delete_all_by_ids<'a>(&'a self, _ids: &'a [T::Id]) -> BoxFuture<'a, Result<(), OrmError>> {
        let _ = &self.url;
        Box::pin(async { Err(OrmError::Unsupported("MySqlRepository — not yet implemented".into())) })
    }
    fn query(&self) -> Box<dyn QueryBuilder<T>> {
        let _ = &self.url;
        Box::new(MySqlQueryBuilder { _marker: PhantomData })
    }
}
