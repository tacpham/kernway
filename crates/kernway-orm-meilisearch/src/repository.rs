//! MeilisearchRepository — `Repository<T>` backed by Meilisearch.

use crate::driver::MeilisearchConfig;
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

/// Meilisearch repository for entity `T`.
///
/// Uses the entity's `table_name()` as the Meilisearch index name.
pub struct MeilisearchRepository<T> {
    config: MeilisearchConfig,
    _marker: PhantomData<T>,
}

impl<T> MeilisearchRepository<T> {
    /// Create a repository from Meilisearch connection settings.
    pub fn new(config: MeilisearchConfig) -> Self {
        Self {
            config,
            _marker: PhantomData,
        }
    }
}

/// Meilisearch query builder.
///
/// Builds a Meilisearch filter expression and search parameters. On `fetch_*`,
/// it would send a search request to `POST /indexes/{index}/search`.
pub struct MeilisearchQueryBuilder<T> {
    config: MeilisearchConfig,
    filters: Vec<String>,
    sort: Vec<String>,
    limit: Option<u64>,
    offset: u64,
    _marker: PhantomData<T>,
}

impl<T> MeilisearchQueryBuilder<T> {
    fn new(config: MeilisearchConfig) -> Self {
        Self {
            config,
            filters: Vec::new(),
            sort: Vec::new(),
            limit: None,
            offset: 0,
            _marker: PhantomData,
        }
    }
}

impl<T: Entity + Send + 'static> QueryBuilder<T> for MeilisearchQueryBuilder<T> {
    fn filter_eq(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(format!("{} = \"{}\"", field, value));
        self
    }
    fn filter_ne(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(format!("{} != \"{}\"", field, value));
        self
    }
    fn filter_gt(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(format!("{} > {}", field, value));
        self
    }
    fn filter_lt(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(format!("{} < {}", field, value));
        self
    }
    fn filter_gte(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(format!("{} >= {}", field, value));
        self
    }
    fn filter_lte(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(format!("{} <= {}", field, value));
        self
    }
    fn filter_like(mut self: Box<Self>, _field: &'static str, pattern: &str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(format!("__fts:{}", pattern));
        self
    }
    fn filter_in(mut self: Box<Self>, field: &'static str, values: Vec<String>) -> Box<dyn QueryBuilder<T>> {
        let list = values.iter().map(|v| format!("\"{}\"", v)).collect::<Vec<_>>().join(", ");
        self.filters.push(format!("{} IN [{}]", field, list));
        self
    }
    fn filter_between(mut self: Box<Self>, field: &'static str, from: &str, to: &str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(format!("{} {} TO {}", field, from, to));
        self
    }
    fn filter_is_null(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(format!("{} IS NULL", field));
        self
    }
    fn filter_is_not_null(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(format!("{} IS NOT NULL", field));
        self
    }
    fn order_by_asc(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self.sort.push(format!("{}:asc", field));
        self
    }
    fn order_by_desc(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self.sort.push(format!("{}:desc", field));
        self
    }
    fn limit(mut self: Box<Self>, n: u64) -> Box<dyn QueryBuilder<T>> {
        self.limit = Some(n);
        self
    }
    fn offset(mut self: Box<Self>, n: u64) -> Box<dyn QueryBuilder<T>> {
        self.offset = n;
        self
    }
    fn with(self: Box<Self>, _relation: &'static str) -> Box<dyn QueryBuilder<T>> {
        self
    }
    fn fetch_all(self: Box<Self>) -> BoxFuture<'static, Result<Vec<T>, OrmError>> {
        let _ = &self.config;
        Box::pin(async move {
            Err(OrmError::Unsupported("MeilisearchQueryBuilder — not yet implemented".into()))
        })
    }
    fn fetch_one(self: Box<Self>) -> BoxFuture<'static, Result<Option<T>, OrmError>> {
        let _ = &self.config;
        Box::pin(async move {
            Err(OrmError::Unsupported("MeilisearchQueryBuilder — not yet implemented".into()))
        })
    }
    fn fetch_count(self: Box<Self>) -> BoxFuture<'static, Result<u64, OrmError>> {
        let _ = &self.config;
        Box::pin(async move {
            Err(OrmError::Unsupported("MeilisearchQueryBuilder — not yet implemented".into()))
        })
    }
    fn fetch_page(self: Box<Self>, _page: u64, _size: u64) -> BoxFuture<'static, Result<Page<T>, OrmError>> {
        let _ = &self.config;
        Box::pin(async move {
            Err(OrmError::Unsupported("MeilisearchQueryBuilder — not yet implemented".into()))
        })
    }
}

impl<T> Repository<T> for MeilisearchRepository<T>
where
    T: Entity + Serialize + DeserializeOwned + Send + 'static,
{
    fn find_by_id<'a>(&'a self, _id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        let _ = (&self.config.url, &self.config.api_key);
        Box::pin(async { Err(OrmError::Unsupported("MeilisearchRepository — not yet implemented".into())) })
    }
    fn find_all<'a>(&'a self) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        let _ = (&self.config.url, &self.config.api_key);
        Box::pin(async { Err(OrmError::Unsupported("MeilisearchRepository — not yet implemented".into())) })
    }
    fn find_all_by_ids<'a>(&'a self, _ids: &'a [T::Id]) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        let _ = (&self.config.url, &self.config.api_key);
        Box::pin(async { Err(OrmError::Unsupported("MeilisearchRepository — not yet implemented".into())) })
    }
    fn count<'a>(&'a self) -> BoxFuture<'a, Result<u64, OrmError>> {
        let _ = (&self.config.url, &self.config.api_key);
        Box::pin(async { Err(OrmError::Unsupported("MeilisearchRepository — not yet implemented".into())) })
    }
    fn exists_by_id<'a>(&'a self, _id: &'a T::Id) -> BoxFuture<'a, Result<bool, OrmError>> {
        let _ = (&self.config.url, &self.config.api_key);
        Box::pin(async { Err(OrmError::Unsupported("MeilisearchRepository — not yet implemented".into())) })
    }
    fn save<'a>(&'a self, _entity: T) -> BoxFuture<'a, Result<T, OrmError>> {
        let _ = (&self.config.url, &self.config.api_key);
        Box::pin(async move { Err(OrmError::Unsupported("MeilisearchRepository — not yet implemented".into())) })
    }
    fn save_all<'a>(&'a self, _entities: Vec<T>) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        let _ = (&self.config.url, &self.config.api_key);
        Box::pin(async { Err(OrmError::Unsupported("MeilisearchRepository — not yet implemented".into())) })
    }
    fn delete_by_id<'a>(&'a self, _id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>> {
        let _ = (&self.config.url, &self.config.api_key);
        Box::pin(async { Err(OrmError::Unsupported("MeilisearchRepository — not yet implemented".into())) })
    }
    fn delete_all_by_ids<'a>(&'a self, _ids: &'a [T::Id]) -> BoxFuture<'a, Result<(), OrmError>> {
        let _ = (&self.config.url, &self.config.api_key);
        Box::pin(async { Err(OrmError::Unsupported("MeilisearchRepository — not yet implemented".into())) })
    }
    fn query(&self) -> Box<dyn QueryBuilder<T>> {
        Box::new(MeilisearchQueryBuilder::new(self.config.clone()))
    }
}
