use crate::{entity::Entity, error::OrmError, query::QueryBuilder, BoxFuture};

/// CRUD operations for a single Entity type.
/// Equivalent to JpaRepository<T, ID> in Spring Data JPA.
pub trait Repository<T: Entity>: Send + Sync {
    // --- Read ---

    /// Find by primary key. Returns None if not found.
    fn find_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>>;

    /// Load all records. Use query() for filtering large tables.
    fn find_all<'a>(&'a self) -> BoxFuture<'a, Result<Vec<T>, OrmError>>;

    /// Load multiple records by IDs in one query.
    fn find_all_by_ids<'a>(&'a self, ids: &'a [T::Id]) -> BoxFuture<'a, Result<Vec<T>, OrmError>>;

    /// Total record count.
    fn count<'a>(&'a self) -> BoxFuture<'a, Result<u64, OrmError>>;

    /// Check existence without loading the entity.
    fn exists_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<bool, OrmError>>;

    // --- Write ---

    /// Insert or update. Uses INSERT if entity has no ID, UPDATE otherwise.
    fn save<'a>(&'a self, entity: T) -> BoxFuture<'a, Result<T, OrmError>>;

    /// Batch save — more efficient than calling save() in a loop.
    fn save_all<'a>(&'a self, entities: Vec<T>) -> BoxFuture<'a, Result<Vec<T>, OrmError>>;

    /// Delete by primary key.
    fn delete_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>>;

    /// Batch delete.
    fn delete_all_by_ids<'a>(&'a self, ids: &'a [T::Id]) -> BoxFuture<'a, Result<(), OrmError>>;

    // --- Fluent query ---

    /// Entry point for the fluent query builder.
    ///
    /// # Example
    /// ```rust,ignore
    /// repo.query()
    ///     .filter_eq("role", "ADMIN")
    ///     .order_by_desc("created_at")
    ///     .fetch_page(0, 20)
    /// ```
    fn query(&self) -> Box<dyn QueryBuilder<T>>;
}
