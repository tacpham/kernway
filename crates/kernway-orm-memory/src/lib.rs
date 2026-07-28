//! In-memory Repository implementation backed by a HashMap.
//!
//! Use for testing and prototyping without a real database.
//! Not suitable for production (data is lost on restart, no transactions).

use kernway_orm_core::{
    entity::Entity, error::OrmError, page::Page, query::QueryBuilder, repository::Repository,
    BoxFuture,
};
use serde::{de::DeserializeOwned, Serialize};
use std::cmp::Ordering as CmpOrdering;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Predicate for in-memory filtering.
#[derive(Clone)]
enum Filter {
    Eq {
        field: &'static str,
        value: String,
    },
    Ne {
        field: &'static str,
        value: String,
    },
    Gt {
        field: &'static str,
        value: String,
    },
    Lt {
        field: &'static str,
        value: String,
    },
    Gte {
        field: &'static str,
        value: String,
    },
    Lte {
        field: &'static str,
        value: String,
    },
    Like {
        field: &'static str,
        pattern: String,
    },
    In {
        field: &'static str,
        values: Vec<String>,
    },
    Between {
        field: &'static str,
        from: String,
        to: String,
    },
    IsNull {
        field: &'static str,
    },
    IsNotNull {
        field: &'static str,
    },
}

/// Sort direction.
#[derive(Clone)]
enum Order {
    Asc(&'static str),
    Desc(&'static str),
}

/// In-memory QueryBuilder implementation.
pub struct MemoryQueryBuilder<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Default + PartialEq + Serialize + DeserializeOwned,
{
    store: Arc<Mutex<HashMap<T::Id, T>>>,
    filters: Vec<Filter>,
    spec: Option<kernway_orm_core::spec::Spec>,
    order: Option<Order>,
    limit: Option<u64>,
    offset: u64,
}

impl<T> MemoryQueryBuilder<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Default + PartialEq + Serialize + DeserializeOwned,
{
    fn new(store: Arc<Mutex<HashMap<T::Id, T>>>) -> Self {
        Self {
            store,
            filters: Vec::new(),
            spec: None,
            order: None,
            limit: None,
            offset: 0,
        }
    }

    fn collect_items(&self) -> Result<Vec<T>, OrmError> {
        let store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        let mut items: Vec<T> = store.values().cloned().collect();
        drop(store);

        items.retain(|entity| self.matches_filters(entity));

        if let Some(order) = &self.order {
            items.sort_by(|left, right| compare_entities(left, right, order));
        }

        Ok(items)
    }

    fn matches_filters(&self, entity: &T) -> bool {
        let filters_ok = self.filters.iter().all(|filter| match filter {
            Filter::Eq { field, value } => get_field_str(entity, field)
                .map(|candidate| candidate == *value)
                .unwrap_or(false),
            Filter::Ne { field, value } => get_field_str(entity, field)
                .map(|candidate| candidate != *value)
                .unwrap_or(false),
            Filter::Gt { field, value } => field_value(entity, field)
                .and_then(|v| cmp_field_to_query(&v, value))
                .map(|o| o == CmpOrdering::Greater)
                .unwrap_or(false),
            Filter::Lt { field, value } => field_value(entity, field)
                .and_then(|v| cmp_field_to_query(&v, value))
                .map(|o| o == CmpOrdering::Less)
                .unwrap_or(false),
            Filter::Gte { field, value } => field_value(entity, field)
                .and_then(|v| cmp_field_to_query(&v, value))
                .map(|o| matches!(o, CmpOrdering::Greater | CmpOrdering::Equal))
                .unwrap_or(false),
            Filter::Lte { field, value } => field_value(entity, field)
                .and_then(|v| cmp_field_to_query(&v, value))
                .map(|o| matches!(o, CmpOrdering::Less | CmpOrdering::Equal))
                .unwrap_or(false),
            Filter::Like { field, pattern } => get_field_str(entity, field)
                .map(|candidate| candidate.contains(pattern))
                .unwrap_or(false),
            Filter::In { field, values } => field_value(entity, field)
                .map(|candidate| {
                    values
                        .iter()
                        .any(|value| cmp_field_to_query(&candidate, value) == Some(CmpOrdering::Equal))
                })
                .unwrap_or(false),
            Filter::Between { field, from, to } => field_value(entity, field)
                .map(|candidate| {
                    let lower = cmp_field_to_query(&candidate, from)
                        .map(|o| matches!(o, CmpOrdering::Greater | CmpOrdering::Equal))
                        .unwrap_or(false);
                    let upper = cmp_field_to_query(&candidate, to)
                        .map(|o| matches!(o, CmpOrdering::Less | CmpOrdering::Equal))
                        .unwrap_or(false);
                    lower && upper
                })
                .unwrap_or(false),
            Filter::IsNull { field } => raw_field_value(entity, field)
                .map(|value| value.is_null())
                .unwrap_or(false),
            Filter::IsNotNull { field } => raw_field_value(entity, field)
                .map(|value| !value.is_null())
                .unwrap_or(false),
        });
        // A Spec (composable OR/AND/NOT), if present, must also hold.
        let spec_ok = match &self.spec {
            Some(s) => s.matches(&|f: &str| get_field_str(entity, f)),
            None => true,
        };
        filters_ok && spec_ok
    }

    fn apply_window(&self, items: Vec<T>) -> Vec<T> {
        let start = usize::try_from(self.offset).unwrap_or(usize::MAX);
        let iter = items.into_iter().skip(start);
        match self.limit {
            Some(limit) => iter
                .take(usize::try_from(limit).unwrap_or(usize::MAX))
                .collect(),
            None => iter.collect(),
        }
    }

    fn fetch_all_sync(self: Box<Self>) -> Result<Vec<T>, OrmError> {
        let items = self.collect_items()?;
        Ok(self.apply_window(items))
    }

    fn fetch_one_sync(mut self: Box<Self>) -> Result<Option<T>, OrmError> {
        self.limit = Some(1);
        Ok(self.fetch_all_sync()?.into_iter().next())
    }

    fn fetch_count_sync(self: Box<Self>) -> Result<u64, OrmError> {
        Ok(self.collect_items()?.len() as u64)
    }

    fn fetch_page_sync(self: Box<Self>, page: u64, size: u64) -> Result<Page<T>, OrmError> {
        let items = self.collect_items()?;
        let total = items.len() as u64;

        if size == 0 {
            return Ok(Page::new(vec![], total, page, size));
        }

        let start = page.saturating_mul(size) as usize;
        let paged = items.into_iter().skip(start).take(size as usize).collect();
        Ok(Page::new(paged, total, page, size))
    }
}

impl<T> QueryBuilder<T> for MemoryQueryBuilder<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Default + PartialEq + Serialize + DeserializeOwned,
{
    fn filter_eq(
        mut self: Box<Self>,
        field: &'static str,
        value: &str,
    ) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::Eq {
            field,
            value: value.to_string(),
        });
        self
    }

    fn filter_ne(
        mut self: Box<Self>,
        field: &'static str,
        value: &str,
    ) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::Ne {
            field,
            value: value.to_string(),
        });
        self
    }

    fn filter_gt(
        mut self: Box<Self>,
        field: &'static str,
        value: &str,
    ) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::Gt {
            field,
            value: value.to_string(),
        });
        self
    }

    fn filter_lt(
        mut self: Box<Self>,
        field: &'static str,
        value: &str,
    ) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::Lt {
            field,
            value: value.to_string(),
        });
        self
    }

    fn filter_gte(
        mut self: Box<Self>,
        field: &'static str,
        value: &str,
    ) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::Gte {
            field,
            value: value.to_string(),
        });
        self
    }

    fn filter_lte(
        mut self: Box<Self>,
        field: &'static str,
        value: &str,
    ) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::Lte {
            field,
            value: value.to_string(),
        });
        self
    }

    fn filter_like(
        mut self: Box<Self>,
        field: &'static str,
        pattern: &str,
    ) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::Like {
            field,
            pattern: pattern.to_string(),
        });
        self
    }

    fn filter_in(
        mut self: Box<Self>,
        field: &'static str,
        values: Vec<String>,
    ) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::In { field, values });
        self
    }

    fn filter_between(
        mut self: Box<Self>,
        field: &'static str,
        from: &str,
        to: &str,
    ) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::Between {
            field,
            from: from.to_string(),
            to: to.to_string(),
        });
        self
    }

    fn filter_is_null(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::IsNull { field });
        self
    }

    fn filter_is_not_null(
        mut self: Box<Self>,
        field: &'static str,
    ) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::IsNotNull { field });
        self
    }

    fn filter_spec(
        mut self: Box<Self>,
        spec: kernway_orm_core::spec::Spec,
    ) -> Box<dyn QueryBuilder<T>> {
        self.spec = Some(spec);
        self
    }

    fn order_by_asc(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self.order = Some(Order::Asc(field));
        self
    }

    fn order_by_desc(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self.order = Some(Order::Desc(field));
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
        Box::pin(async move { self.fetch_all_sync() })
    }

    fn fetch_one(self: Box<Self>) -> BoxFuture<'static, Result<Option<T>, OrmError>> {
        Box::pin(async move { self.fetch_one_sync() })
    }

    fn fetch_count(self: Box<Self>) -> BoxFuture<'static, Result<u64, OrmError>> {
        Box::pin(async move { self.fetch_count_sync() })
    }

    fn fetch_page(
        self: Box<Self>,
        page: u64,
        size: u64,
    ) -> BoxFuture<'static, Result<Page<T>, OrmError>> {
        Box::pin(async move { self.fetch_page_sync(page, size) })
    }
}

/// In-memory repository implementation.
pub struct InMemoryRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Default + PartialEq + Serialize + DeserializeOwned,
{
    store: Arc<Mutex<HashMap<T::Id, T>>>,
    next_id: AtomicU64,
}

impl<T> InMemoryRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Default + PartialEq + Serialize + DeserializeOwned,
{
    /// Create an empty repository.
    ///
    /// Storage is process-local and vanishes with the value — this backend
    /// exists for tests and for running an app before a real database is wired
    /// up, not for persistence.
    pub fn new() -> Self {
        Self {
            store: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(0),
        }
    }

    fn maybe_assign_id(&self, entity: T) -> Result<T, OrmError> {
        if entity.id() != T::Id::default() {
            return Ok(entity);
        }

        let next = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let pk_field = T::columns()
            .iter()
            .find(|column| column.primary_key)
            .map(|column| column.field)
            .ok_or_else(|| {
                OrmError::TypeConversion("entity is missing primary key metadata".to_string())
            })?;

        let new_id: T::Id = serde_json::from_value(serde_json::Value::from(next))
            .map_err(|e| OrmError::TypeConversion(format!("cannot assign generated id: {e}")))?;

        let mut value = serde_json::to_value(&entity)
            .map_err(|e| OrmError::TypeConversion(format!("cannot serialize entity: {e}")))?;
        let object = value.as_object_mut().ok_or_else(|| {
            OrmError::TypeConversion("entity must serialize as an object".to_string())
        })?;
        object.insert(
            pk_field.to_string(),
            serde_json::to_value(&new_id)
                .map_err(|e| OrmError::TypeConversion(format!("cannot serialize id: {e}")))?,
        );

        serde_json::from_value(value)
            .map_err(|e| OrmError::TypeConversion(format!("cannot deserialize entity: {e}")))
    }

    fn find_by_id_sync(&self, id: &T::Id) -> Result<Option<T>, OrmError> {
        let store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        Ok(store.get(id).cloned())
    }

    fn find_all_sync(&self) -> Result<Vec<T>, OrmError> {
        let store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        Ok(store.values().cloned().collect())
    }

    fn find_all_by_ids_sync(&self, ids: &[T::Id]) -> Result<Vec<T>, OrmError> {
        let store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        Ok(ids.iter().filter_map(|id| store.get(id).cloned()).collect())
    }

    fn count_sync(&self) -> Result<u64, OrmError> {
        let store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        Ok(store.len() as u64)
    }

    fn exists_by_id_sync(&self, id: &T::Id) -> Result<bool, OrmError> {
        let store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        Ok(store.contains_key(id))
    }

    fn save_sync(&self, entity: T) -> Result<T, OrmError> {
        let entity = self.maybe_assign_id(entity)?;
        let id = entity.id();
        let mut store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        store.insert(id, entity.clone());
        Ok(entity)
    }

    fn save_all_sync(&self, entities: Vec<T>) -> Result<Vec<T>, OrmError> {
        entities
            .into_iter()
            .map(|entity| self.save_sync(entity))
            .collect()
    }

    fn delete_by_id_sync(&self, id: &T::Id) -> Result<(), OrmError> {
        let mut store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        store.remove(id);
        Ok(())
    }

    fn delete_all_by_ids_sync(&self, ids: &[T::Id]) -> Result<(), OrmError> {
        let mut store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        for id in ids {
            store.remove(id);
        }
        Ok(())
    }
}

impl<T> Default for InMemoryRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Default + PartialEq + Serialize + DeserializeOwned,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Repository<T> for InMemoryRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Default + PartialEq + Serialize + DeserializeOwned,
{
    fn find_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        let id = id.clone();
        Box::pin(async move { self.find_by_id_sync(&id) })
    }

    fn find_all<'a>(&'a self) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        Box::pin(async move { self.find_all_sync() })
    }

    fn find_all_by_ids<'a>(&'a self, ids: &'a [T::Id]) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        let ids = ids.to_vec();
        Box::pin(async move { self.find_all_by_ids_sync(&ids) })
    }

    fn count<'a>(&'a self) -> BoxFuture<'a, Result<u64, OrmError>> {
        Box::pin(async move { self.count_sync() })
    }

    fn exists_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<bool, OrmError>> {
        let id = id.clone();
        Box::pin(async move { self.exists_by_id_sync(&id) })
    }

    fn save<'a>(&'a self, entity: T) -> BoxFuture<'a, Result<T, OrmError>> {
        Box::pin(async move { self.save_sync(entity) })
    }

    fn save_all<'a>(&'a self, entities: Vec<T>) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        Box::pin(async move { self.save_all_sync(entities) })
    }

    fn delete_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>> {
        let id = id.clone();
        Box::pin(async move { self.delete_by_id_sync(&id) })
    }

    fn delete_all_by_ids<'a>(&'a self, ids: &'a [T::Id]) -> BoxFuture<'a, Result<(), OrmError>> {
        let ids = ids.to_vec();
        Box::pin(async move { self.delete_all_by_ids_sync(&ids) })
    }

    fn query(&self) -> Box<dyn QueryBuilder<T>> {
        Box::new(MemoryQueryBuilder::new(Arc::clone(&self.store)))
    }
}

fn compare_entities<T>(left: &T, right: &T, order: &Order) -> CmpOrdering
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Default + PartialEq + Serialize + DeserializeOwned,
{
    let field = match order {
        Order::Asc(field) | Order::Desc(field) => field,
    };

    let left_value = field_value(left, field);
    let right_value = field_value(right, field);
    let ordering = match (left_value, right_value) {
        (Some(l), Some(r)) => compare_field_values(&l, &r),
        (None, Some(_)) => CmpOrdering::Less,
        (Some(_), None) => CmpOrdering::Greater,
        (None, None) => CmpOrdering::Equal,
    };

    match order {
        Order::Asc(_) => ordering,
        Order::Desc(_) => ordering.reverse(),
    }
}

/// Extract a field as its raw JSON value.
fn raw_field_value<T>(entity: &T, field: &str) -> Option<serde_json::Value>
where
    T: Serialize,
{
    let value = serde_json::to_value(entity).ok()?;
    value.get(field).cloned()
}

/// Extract a scalar field as its raw JSON value (String/Number/Bool).
fn field_value<T>(entity: &T, field: &str) -> Option<serde_json::Value>
where
    T: Serialize,
{
    match raw_field_value(entity, field)? {
        v @ (serde_json::Value::String(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::Bool(_)) => Some(v),
        _ => None,
    }
}

/// Stringified field, kept for Eq/Ne/Like (substring/equality) semantics.
fn get_field_str<T>(entity: &T, field: &str) -> Option<String>
where
    T: Serialize,
{
    match field_value(entity, field)? {
        serde_json::Value::String(s) => Some(s),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Order two field values numeric-aware: numbers compare by magnitude
/// ("9" < "10"), matching the SQL backend instead of lexicographically.
fn compare_field_values(a: &serde_json::Value, b: &serde_json::Value) -> CmpOrdering {
    use serde_json::Value::{Bool, Number, String as JStr};
    match (a, b) {
        (Number(x), Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(xf), Some(yf)) => xf.partial_cmp(&yf).unwrap_or(CmpOrdering::Equal),
            _ => x.to_string().cmp(&y.to_string()),
        },
        (JStr(x), JStr(y)) => x.cmp(y),
        (Bool(x), Bool(y)) => x.cmp(y),
        _ => a.to_string().cmp(&b.to_string()),
    }
}

/// Compare an entity field value against a query string, numeric-aware so a
/// numeric field orders by magnitude against the query value.
fn cmp_field_to_query(field_val: &serde_json::Value, query: &str) -> Option<CmpOrdering> {
    match field_val {
        serde_json::Value::Number(n) => {
            let lhs = n.as_f64()?;
            match query.parse::<f64>() {
                Ok(rhs) => lhs.partial_cmp(&rhs),
                Err(_) => Some(n.to_string().as_str().cmp(query)),
            }
        }
        serde_json::Value::String(s) => Some(s.as_str().cmp(query)),
        serde_json::Value::Bool(b) => Some(b.to_string().as_str().cmp(query)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::InMemoryRepository;
    use kernway_orm_core::{error::OrmError, repository::Repository, Entity};
    use kernway_orm_macro::entity;
    use serde::{Deserialize, Serialize};
    use std::future::Future;

    fn block_on<F: Future>(future: F) -> F::Output {
        rt_core::Executor::new().unwrap().block_on(future).unwrap()
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[entity(table = "todos")]
    struct Todo {
        #[id(strategy = "auto")]
        id: u64,
        title: String,
        done: bool,
    }

    fn sample(title: &str) -> Todo {
        Todo {
            id: 0,
            title: title.to_string(),
            done: false,
        }
    }

    #[test]
    fn memory_repo_save_and_find_by_id() {
        let repo = InMemoryRepository::<Todo>::new();
        let saved = block_on(repo.save(sample("first"))).unwrap();
        let found = block_on(repo.find_by_id(&saved.id())).unwrap().unwrap();

        assert_eq!(saved.id, 1);
        assert_eq!(found.title, "first");
    }

    #[test]
    fn memory_repo_find_all() {
        let repo = InMemoryRepository::<Todo>::new();
        block_on(repo.save(sample("a"))).unwrap();
        block_on(repo.save(sample("b"))).unwrap();

        let mut items = block_on(repo.find_all()).unwrap();
        items.sort_by(|left, right| left.title.cmp(&right.title));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "a");
        assert_eq!(items[1].title, "b");
    }

    #[test]
    fn memory_repo_delete() {
        let repo = InMemoryRepository::<Todo>::new();
        let saved = block_on(repo.save(sample("gone"))).unwrap();

        block_on(repo.delete_by_id(&saved.id())).unwrap();

        assert!(block_on(repo.find_by_id(&saved.id())).unwrap().is_none());
    }

    #[test]
    fn memory_repo_count() {
        let repo = InMemoryRepository::<Todo>::new();
        block_on(repo.save(sample("one"))).unwrap();
        block_on(repo.save(sample("two"))).unwrap();

        assert_eq!(block_on(repo.count()).unwrap(), 2);
    }

    #[test]
    fn memory_repo_query_filter_eq() {
        let repo = InMemoryRepository::<Todo>::new();
        block_on(repo.save(sample("alpha"))).unwrap();
        block_on(repo.save(sample("beta"))).unwrap();

        let items = block_on(repo.query().filter_eq("title", "alpha").fetch_all()).unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "alpha");
    }

    #[test]
    fn memory_repo_query_paginate() {
        let repo = InMemoryRepository::<Todo>::new();
        for title in ["a", "b", "c", "d", "e"] {
            block_on(repo.save(sample(title))).unwrap();
        }

        let page = block_on(repo.query().order_by_asc("id").fetch_page(1, 2)).unwrap();

        assert_eq!(page.total, 5);
        assert_eq!(page.total_pages, 3);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].id, 3);
        assert_eq!(page.items[1].id, 4);
    }

    // --- M1.0 correctness regression: numeric fields compare by magnitude ---

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[entity(table = "metrics")]
    struct Metric {
        #[id(strategy = "auto")]
        id: u64,
        score: i64,
    }

    fn metric(score: i64) -> Metric {
        Metric { id: 0, score }
    }

    #[test]
    fn memory_numeric_order_is_by_magnitude_not_lexicographic() {
        let repo = InMemoryRepository::<Metric>::new();
        for s in [100, 9, 20, 3] {
            block_on(repo.save(metric(s))).unwrap();
        }
        let items = block_on(repo.query().order_by_asc("score").fetch_all()).unwrap();
        let scores: Vec<i64> = items.iter().map(|m| m.score).collect();
        assert_eq!(scores, vec![3, 9, 20, 100]);
    }

    #[test]
    fn memory_numeric_filter_gt_is_numeric() {
        let repo = InMemoryRepository::<Metric>::new();
        for s in [9, 10, 100] {
            block_on(repo.save(metric(s))).unwrap();
        }
        let mut scores: Vec<i64> = block_on(repo.query().filter_gt("score", "9").fetch_all())
            .unwrap()
            .iter()
            .map(|m| m.score)
            .collect();
        scores.sort();
        assert_eq!(scores, vec![10, 100]);
    }

    /// Composite key `(warehouse, sku)` — works out of the box because the store
    /// is keyed by the whole `T::Id`, and a tuple Id is `Hash + Eq`.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[entity(table = "stock")]
    struct Stock {
        #[id()]
        warehouse: String,
        #[id()]
        sku: String,
        quantity: i32,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[entity(table = "people")]
    struct Person {
        #[id()]
        id: u64,
        role: String,
        age: i64,
        tier: String,
    }

    fn people() -> Vec<Person> {
        vec![
            Person { id: 1, role: "ADMIN".into(), age: 30, tier: "silver".into() },
            Person { id: 2, role: "ADMIN".into(), age: 15, tier: "gold".into() },
            Person { id: 3, role: "ADMIN".into(), age: 15, tier: "silver".into() },
            Person { id: 4, role: "USER".into(), age: 40, tier: "gold".into() },
        ]
    }

    #[test]
    fn memory_filter_spec_or_and_not() {
        use kernway_orm_core::spec::Spec;
        let repo = InMemoryRepository::<Person>::new();
        for p in people() {
            block_on(repo.save(p)).unwrap();
        }

        // role = ADMIN AND (age > 18 OR tier = gold)
        let spec = Spec::eq("role", "ADMIN").and(Spec::gt("age", "18").or(Spec::eq("tier", "gold")));
        let mut ids: Vec<u64> = block_on(repo.query().filter_spec(spec).fetch_all())
            .unwrap()
            .iter()
            .map(|p| p.id)
            .collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2]); // 3 = admin/15/silver excluded; 4 = USER excluded

        // NOT (tier = gold) → silver ones.
        let mut silver: Vec<u64> =
            block_on(repo.query().filter_spec(Spec::eq("tier", "gold").not()).fetch_all())
                .unwrap()
                .iter()
                .map(|p| p.id)
                .collect();
        silver.sort();
        assert_eq!(silver, vec![1, 3]);
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[entity(table = "users")]
    struct User {
        #[id(strategy = "auto")]
        id: u64,
        email: String,
        role: String,
        age: i64,
    }

    // Derived repository — method names generate the queries.
    #[kernway_orm_macro::repository(entity = User)]
    #[allow(async_fn_in_trait)]
    trait UserRepo {
        async fn find_by_email(&self, email: &str) -> Result<Option<User>, OrmError>;
        async fn find_by_role(&self, role: &str) -> Result<Vec<User>, OrmError>;
        async fn find_by_role_and_age_gt(&self, role: &str, age: i64) -> Result<Vec<User>, OrmError>;
        async fn count_by_role(&self, role: &str) -> Result<u64, OrmError>;
        async fn exists_by_email(&self, email: &str) -> Result<bool, OrmError>;
    }

    #[test]
    fn memory_derived_repository() {
        let repo: Box<dyn Repository<User>> = Box::new(InMemoryRepository::<User>::new());
        for (email, role, age) in [("a@x", "ADMIN", 30i64), ("b@x", "ADMIN", 15), ("c@x", "USER", 40)] {
            block_on(repo.save(User { id: 0, email: email.into(), role: role.into(), age })).unwrap();
        }
        let users = UserRepoImpl::new(repo);

        // find_by_email → Option
        assert_eq!(block_on(users.find_by_email("a@x")).unwrap().unwrap().role, "ADMIN");
        assert!(block_on(users.find_by_email("nope")).unwrap().is_none());

        // find_by_role → Vec
        assert_eq!(block_on(users.find_by_role("ADMIN")).unwrap().len(), 2);

        // find_by_role_and_age_gt → the AND with a > operator
        let over = block_on(users.find_by_role_and_age_gt("ADMIN", 18)).unwrap();
        assert_eq!(over.len(), 1);
        assert_eq!(over[0].email, "a@x");

        // count_by_role and exists_by_email
        assert_eq!(block_on(users.count_by_role("ADMIN")).unwrap(), 2);
        assert!(block_on(users.exists_by_email("a@x")).unwrap());
        assert!(!block_on(users.exists_by_email("nope")).unwrap());
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[entity(table = "posts")]
    struct Post {
        #[id(strategy = "auto")]
        id: u64,
        #[many_to_one(entity = "users")]
        user_id: u64,
        title: String,
    }

    #[test]
    fn many_to_one_relation_metadata() {
        use kernway_orm_core::RelationKind;
        // The #[many_to_one] field is recorded as a relation...
        let rels = Post::relations();
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].field, "user_id");
        assert_eq!(rels[0].kind, RelationKind::ManyToOne);
        assert_eq!(rels[0].target_table, "users");
        assert_eq!(rels[0].foreign_key, "user_id");
        // ...and it is still a normal column (the FK).
        assert!(Post::columns().iter().any(|c| c.name == "user_id"));
        // An entity with no relations declares none (the default).
        assert!(User::relations().is_empty());

        // Loading stays explicit: a derived finder over the FK column.
        let repo: Box<dyn Repository<Post>> = Box::new(InMemoryRepository::<Post>::new());
        block_on(repo.save(Post { id: 0, user_id: 7, title: "hi".into() })).unwrap();
        let by_user = block_on(repo.query().filter_eq("user_id", "7").fetch_all()).unwrap();
        assert_eq!(by_user.len(), 1);
    }

    #[test]
    fn memory_composite_primary_key() {
        let repo = InMemoryRepository::<Stock>::new();
        let key = |w: &str, s: &str| (w.to_string(), s.to_string());

        block_on(repo.save(Stock { warehouse: "WH1".into(), sku: "SKU42".into(), quantity: 100 })).unwrap();
        block_on(repo.save(Stock { warehouse: "WH2".into(), sku: "SKU42".into(), quantity: 5 })).unwrap();
        assert_eq!(block_on(repo.count()).unwrap(), 2);

        assert_eq!(block_on(repo.find_by_id(&key("WH1", "SKU42"))).unwrap().unwrap().quantity, 100);
        assert!(block_on(repo.find_by_id(&key("WH3", "SKU42"))).unwrap().is_none());

        // Re-saving the same composite key overwrites (update), not a new row.
        block_on(repo.save(Stock { warehouse: "WH1".into(), sku: "SKU42".into(), quantity: 250 })).unwrap();
        assert_eq!(block_on(repo.count()).unwrap(), 2);
        assert_eq!(block_on(repo.find_by_id(&key("WH1", "SKU42"))).unwrap().unwrap().quantity, 250);

        block_on(repo.delete_by_id(&key("WH1", "SKU42"))).unwrap();
        assert!(block_on(repo.find_by_id(&key("WH1", "SKU42"))).unwrap().is_none());
        assert!(block_on(repo.exists_by_id(&key("WH2", "SKU42"))).unwrap());
    }
}
