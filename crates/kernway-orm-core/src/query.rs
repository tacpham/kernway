use crate::{entity::Entity, error::OrmError, page::Page};

/// Fluent, chainable query builder.
/// Equivalent to CriteriaBuilder + TypedQuery in JPA.
///
/// Sync — terminal methods return Result directly.
pub trait QueryBuilder<T: Entity>: Send {
    // --- Filters ---
    // Every filter narrows the result; they combine with AND. There is no OR —
    // reach for a raw query when you need one.

    /// `field = value`.
    fn filter_eq(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>>;
    /// `field <> value`.
    fn filter_ne(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>>;
    /// `field > value`.
    fn filter_gt(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>>;
    /// `field < value`.
    fn filter_lt(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>>;
    /// `field LIKE pattern` — `%` and `_` carry their usual SQL meaning.
    fn filter_like(self: Box<Self>, field: &'static str, pattern: &str) -> Box<dyn QueryBuilder<T>>;

    // --- Ordering ---

    /// Append `ORDER BY field ASC`. Repeated calls order by each in turn.
    fn order_by_asc(self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>>;
    /// Append `ORDER BY field DESC`.
    fn order_by_desc(self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>>;

    // --- Pagination ---

    /// Cap the number of rows returned.
    fn limit(self: Box<Self>, n: u64) -> Box<dyn QueryBuilder<T>>;
    /// Skip `n` rows. Pair with an explicit ordering — without one the skipped
    /// set is whatever the backend felt like returning.
    fn offset(self: Box<Self>, n: u64) -> Box<dyn QueryBuilder<T>>;

    // --- Eager loading (avoid N+1) ---

    /// Load `relation` in the same round trip instead of lazily.
    ///
    /// This is the explicit alternative to JPA's lazy loading: Rust has no
    /// bytecode proxies, so a relation you did not ask for is simply not
    /// loaded — and an N+1 can never happen behind your back.
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
