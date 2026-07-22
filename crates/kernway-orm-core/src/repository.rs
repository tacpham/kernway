use crate::{entity::Entity, error::OrmError, query::QueryBuilder};

/// CRUD operations for a single Entity type.
/// Equivalent to JpaRepository<T, ID> in Spring Data JPA.
///
/// All methods are synchronous (blocking). In a thread-per-request server,
/// each request thread blocks on DB calls — connection pool handles concurrency.
pub trait Repository<T: Entity>: Send + Sync {
    // --- Read ---

    /// Find by primary key. Returns None if not found.
    fn find_by_id(&self, id: &T::Id) -> Result<Option<T>, OrmError>;

    /// Load all records. Use query() for filtering large tables.
    fn find_all(&self) -> Result<Vec<T>, OrmError>;

    /// Load multiple records by IDs in one query.
    fn find_all_by_ids(&self, ids: &[T::Id]) -> Result<Vec<T>, OrmError>;

    /// Total record count.
    fn count(&self) -> Result<u64, OrmError>;

    /// Check existence without loading the entity.
    fn exists_by_id(&self, id: &T::Id) -> Result<bool, OrmError>;

    // --- Write ---

    /// Insert or update. Uses INSERT if entity has no ID, UPDATE otherwise.
    fn save(&self, entity: T) -> Result<T, OrmError>;

    /// Batch save — more efficient than calling save() in a loop.
    fn save_all(&self, entities: Vec<T>) -> Result<Vec<T>, OrmError>;

    /// Delete by primary key.
    fn delete_by_id(&self, id: &T::Id) -> Result<(), OrmError>;

    /// Batch delete.
    fn delete_all_by_ids(&self, ids: &[T::Id]) -> Result<(), OrmError>;

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
