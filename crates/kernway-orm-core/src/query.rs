use crate::{entity::Entity, error::OrmError, page::Page, spec::Spec, BoxFuture};

/// Fluent, chainable query builder.
/// Equivalent to CriteriaBuilder + TypedQuery in JPA.
///
/// Chaining methods stay synchronous; terminal methods return boxed futures.
pub trait QueryBuilder<T: Entity>: Send {
    // --- Filters ---
    // Every fluent filter narrows the result; they combine with AND. For OR and
    // nested boolean logic, use [`filter_spec`](Self::filter_spec) with a
    // composable [`Spec`].

    /// `field = value`.
    fn filter_eq(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>>;
    /// `field <> value`.
    fn filter_ne(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>>;
    /// `field > value`.
    fn filter_gt(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>>;
    /// `field < value`.
    fn filter_lt(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>>;
    /// `field LIKE pattern` — `%` and `_` carry their usual SQL meaning.
    fn filter_like(self: Box<Self>, field: &'static str, pattern: &str)
        -> Box<dyn QueryBuilder<T>>;
    /// `field >= value` (greater than or equal).
    fn filter_gte(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>>;
    /// `field <= value` (less than or equal).
    fn filter_lte(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>>;
    /// `field IN (v1, v2, …)` — match any value in the list.
    fn filter_in(self: Box<Self>, field: &'static str, values: Vec<String>) -> Box<dyn QueryBuilder<T>>;
    /// `field BETWEEN from AND to` — inclusive range.
    fn filter_between(
        self: Box<Self>,
        field: &'static str,
        from: &str,
        to: &str,
    ) -> Box<dyn QueryBuilder<T>>;
    /// `field IS NULL`.
    fn filter_is_null(self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>>;
    /// `field IS NOT NULL`.
    fn filter_is_not_null(self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>>;

    /// Filter by a composable [`Spec`] — the way to express
    /// `OR` and nested boolean logic. It combines with any fluent filters (which
    /// stay `AND`-ed) with `AND`.
    ///
    /// ```ignore
    /// repo.query()
    ///     .filter_spec(Spec::eq("role", "ADMIN")
    ///         .and(Spec::gt("age", "18").or(Spec::eq("vip", "true"))))
    ///     .fetch_all().await?;
    /// ```
    fn filter_spec(self: Box<Self>, spec: Spec) -> Box<dyn QueryBuilder<T>>;

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
    fn fetch_all(self: Box<Self>) -> BoxFuture<'static, Result<Vec<T>, OrmError>>;

    /// Execute and return the first matching record.
    fn fetch_one(self: Box<Self>) -> BoxFuture<'static, Result<Option<T>, OrmError>>;

    /// Count matching records.
    fn fetch_count(self: Box<Self>) -> BoxFuture<'static, Result<u64, OrmError>>;

    /// Paginated result. `page` is 0-indexed.
    fn fetch_page(
        self: Box<Self>,
        page: u64,
        size: u64,
    ) -> BoxFuture<'static, Result<Page<T>, OrmError>>;
}
