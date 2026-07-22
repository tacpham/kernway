//! In-memory Repository implementation backed by a HashMap.
//!
//! Use for testing and prototyping without a real database.
//! Not suitable for production (data is lost on restart, no transactions).

use kernway_orm_core::{
    entity::Entity,
    error::OrmError,
    page::Page,
    query::QueryBuilder,
    repository::Repository,
};
use serde::{de::DeserializeOwned, Serialize};
use std::cmp::Ordering as CmpOrdering;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Predicate for in-memory filtering.
#[derive(Clone)]
enum Filter {
    Eq { field: &'static str, value: String },
    Ne { field: &'static str, value: String },
    Gt { field: &'static str, value: String },
    Lt { field: &'static str, value: String },
    Like { field: &'static str, pattern: String },
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
        self.filters.iter().all(|filter| match filter {
            Filter::Eq { field, value } => get_field_str(entity, field)
                .map(|candidate| candidate == *value)
                .unwrap_or(false),
            Filter::Ne { field, value } => get_field_str(entity, field)
                .map(|candidate| candidate != *value)
                .unwrap_or(false),
            Filter::Gt { field, value } => get_field_str(entity, field)
                .map(|candidate| candidate > *value)
                .unwrap_or(false),
            Filter::Lt { field, value } => get_field_str(entity, field)
                .map(|candidate| candidate < *value)
                .unwrap_or(false),
            Filter::Like { field, pattern } => get_field_str(entity, field)
                .map(|candidate| candidate.contains(pattern))
                .unwrap_or(false),
        })
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
}

impl<T> QueryBuilder<T> for MemoryQueryBuilder<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Default + PartialEq + Serialize + DeserializeOwned,
{
    fn filter_eq(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::Eq {
            field,
            value: value.to_string(),
        });
        self
    }

    fn filter_ne(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::Ne {
            field,
            value: value.to_string(),
        });
        self
    }

    fn filter_gt(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::Gt {
            field,
            value: value.to_string(),
        });
        self
    }

    fn filter_lt(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::Lt {
            field,
            value: value.to_string(),
        });
        self
    }

    fn filter_like(mut self: Box<Self>, field: &'static str, pattern: &str) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(Filter::Like {
            field,
            pattern: pattern.to_string(),
        });
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

    fn fetch_all(self: Box<Self>) -> Result<Vec<T>, OrmError> {
        let items = self.collect_items()?;
        Ok(self.apply_window(items))
    }

    fn fetch_one(self: Box<Self>) -> Result<Option<T>, OrmError> {
        Ok(self.fetch_all()?.into_iter().next())
    }

    fn fetch_count(self: Box<Self>) -> Result<u64, OrmError> {
        Ok(self.collect_items()?.len() as u64)
    }

    fn fetch_page(self: Box<Self>, page: u64, size: u64) -> Result<Page<T>, OrmError> {
        let items = self.collect_items()?;
        let total = items.len() as u64;

        if size == 0 {
            return Ok(Page::new(vec![], total, page, size));
        }

        let start = page.saturating_mul(size) as usize;
        let paged = items
            .into_iter()
            .skip(start)
            .take(size as usize)
            .collect();
        Ok(Page::new(paged, total, page, size))
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
    pub fn new() -> Self {
        Self {
            store: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(0),
        }
    }

    fn maybe_assign_id(&self, entity: T) -> Result<T, OrmError> {
        if entity.id() != &T::Id::default() {
            return Ok(entity);
        }

        let next = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let pk_field = T::columns()
            .iter()
            .find(|column| column.primary_key)
            .map(|column| column.field)
            .ok_or_else(|| OrmError::TypeConversion("entity is missing primary key metadata".to_string()))?;

        let new_id: T::Id = serde_json::from_value(serde_json::Value::from(next))
            .map_err(|e| OrmError::TypeConversion(format!("cannot assign generated id: {e}")))?;

        let mut value = serde_json::to_value(&entity)
            .map_err(|e| OrmError::TypeConversion(format!("cannot serialize entity: {e}")))?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| OrmError::TypeConversion("entity must serialize as an object".to_string()))?;
        object.insert(
            pk_field.to_string(),
            serde_json::to_value(&new_id)
                .map_err(|e| OrmError::TypeConversion(format!("cannot serialize id: {e}")))?,
        );

        serde_json::from_value(value)
            .map_err(|e| OrmError::TypeConversion(format!("cannot deserialize entity: {e}")))
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
    fn find_by_id(&self, id: &T::Id) -> Result<Option<T>, OrmError> {
        let store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        Ok(store.get(id).cloned())
    }

    fn find_all(&self) -> Result<Vec<T>, OrmError> {
        let store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        Ok(store.values().cloned().collect())
    }

    fn find_all_by_ids(&self, ids: &[T::Id]) -> Result<Vec<T>, OrmError> {
        let store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        Ok(ids.iter().filter_map(|id| store.get(id).cloned()).collect())
    }

    fn count(&self) -> Result<u64, OrmError> {
        let store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        Ok(store.len() as u64)
    }

    fn exists_by_id(&self, id: &T::Id) -> Result<bool, OrmError> {
        let store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        Ok(store.contains_key(id))
    }

    fn save(&self, entity: T) -> Result<T, OrmError> {
        let entity = self.maybe_assign_id(entity)?;
        let id = entity.id().clone();
        let mut store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        store.insert(id, entity.clone());
        Ok(entity)
    }

    fn save_all(&self, entities: Vec<T>) -> Result<Vec<T>, OrmError> {
        entities.into_iter().map(|entity| self.save(entity)).collect()
    }

    fn delete_by_id(&self, id: &T::Id) -> Result<(), OrmError> {
        let mut store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        store.remove(id);
        Ok(())
    }

    fn delete_all_by_ids(&self, ids: &[T::Id]) -> Result<(), OrmError> {
        let mut store = self
            .store
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        for id in ids {
            store.remove(id);
        }
        Ok(())
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

    let left_value = get_field_str(left, field).unwrap_or_default();
    let right_value = get_field_str(right, field).unwrap_or_default();
    let ordering = left_value.cmp(&right_value);

    match order {
        Order::Asc(_) => ordering,
        Order::Desc(_) => ordering.reverse(),
    }
}

fn get_field_str<T>(entity: &T, field: &str) -> Option<String>
where
    T: Serialize,
{
    let value = serde_json::to_value(entity).ok()?;
    match &value[field] {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::InMemoryRepository;
    use kernway_orm_core::{repository::Repository, Entity};
    use kernway_orm_macro::entity;
    use serde::{Deserialize, Serialize};

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
        let saved = repo.save(sample("first")).unwrap();
        let found = repo.find_by_id(saved.id()).unwrap().unwrap();

        assert_eq!(saved.id, 1);
        assert_eq!(found.title, "first");
    }

    #[test]
    fn memory_repo_find_all() {
        let repo = InMemoryRepository::<Todo>::new();
        repo.save(sample("a")).unwrap();
        repo.save(sample("b")).unwrap();

        let mut items = repo.find_all().unwrap();
        items.sort_by(|left, right| left.title.cmp(&right.title));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "a");
        assert_eq!(items[1].title, "b");
    }

    #[test]
    fn memory_repo_delete() {
        let repo = InMemoryRepository::<Todo>::new();
        let saved = repo.save(sample("gone")).unwrap();

        repo.delete_by_id(saved.id()).unwrap();

        assert!(repo.find_by_id(saved.id()).unwrap().is_none());
    }

    #[test]
    fn memory_repo_count() {
        let repo = InMemoryRepository::<Todo>::new();
        repo.save(sample("one")).unwrap();
        repo.save(sample("two")).unwrap();

        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn memory_repo_query_filter_eq() {
        let repo = InMemoryRepository::<Todo>::new();
        repo.save(sample("alpha")).unwrap();
        repo.save(sample("beta")).unwrap();

        let items = repo
            .query()
            .filter_eq("title", "alpha")
            .fetch_all()
            .unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "alpha");
    }

    #[test]
    fn memory_repo_query_paginate() {
        let repo = InMemoryRepository::<Todo>::new();
        for title in ["a", "b", "c", "d", "e"] {
            repo.save(sample(title)).unwrap();
        }

        let page = repo
            .query()
            .order_by_asc("id")
            .fetch_page(1, 2)
            .unwrap();

        assert_eq!(page.total, 5);
        assert_eq!(page.total_pages, 3);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].id, 3);
        assert_eq!(page.items[1].id, 4);
    }
}
