use crate::{entity::Entity, error::OrmError, page::Page};

/// Fluent, chainable query builder.
/// Equivalent to CriteriaBuilder + TypedQuery in JPA.
///
/// Sync — terminal methods return Result directly.
pub trait QueryBuilder<T: Entity>: Send {
    // --- Filters ---
    fn filter_eq(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>>;
    fn filter_ne(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>>;
    fn filter_gt(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>>;
    fn filter_lt(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>>;
    fn filter_like(self: Box<Self>, field: &'static str, pattern: &str) -> Box<dyn QueryBuilder<T>>;

    // --- Ordering ---
    fn order_by_asc(self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>>;
    fn order_by_desc(self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>>;

    // --- Pagination ---
    fn limit(self: Box<Self>, n: u64) -> Box<dyn QueryBuilder<T>>;
    fn offset(self: Box<Self>, n: u64) -> Box<dyn QueryBuilder<T>>;

    // --- Eager loading (avoid N+1) ---
    fn with(self: Box<Self>, relation: &'static str) -> Box<dyn QueryBuilder<T>>;

    // --- Terminal operations ---

    /// Execute and return all matching records.
    fn fetch_all(self: Box<Self>) -> Result<Vec<T>, OrmError>;

    /// Execute and return the first matching record.
    fn fetch_one(self: Box<Self>) -> Result<Option<T>, OrmError>;

    /// Count matching records.
    fn fetch_count(self: Box<Self>) -> Result<u64, OrmError>;

    /// Paginated result. `page` is 0-indexed.
    fn fetch_page(self: Box<Self>, page: u64, size: u64) -> Result<Page<T>, OrmError>;
}
