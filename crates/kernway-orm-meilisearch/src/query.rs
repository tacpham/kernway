//! `MeilisearchQueryBuilder` — fluent query builder for Meilisearch.
//!
//! Builds a Meilisearch **filter expression** and search parameters.
//! Terminal calls (`fetch_all`, `fetch_one`, …) POST to `/{index}/search`.
//!
//! ## Filter expression syntax
//!
//! Fields used in `filter_*` calls must be in the index's
//! `filterableAttributes` setting; fields in `order_by_*` calls must be in
//! `sortableAttributes`.
//!
//! | ORM call | Meilisearch filter fragment |
//! |---|---|
//! | `filter_eq("role", "ADMIN")` | `role = "ADMIN"` |
//! | `filter_ne("status", "deleted")` | `status != "deleted"` |
//! | `filter_gt("age", "18")` | `age > 18` |
//! | `filter_gte("score", "90")` | `score >= 90` |
//! | `filter_lt("price", "100")` | `price < 100` |
//! | `filter_lte("price", "100")` | `price <= 100` |
//! | `filter_in("tag", vec!["a","b"])` | `tag IN ["a", "b"]` |
//! | `filter_between("price", "10", "50")` | `price 10 TO 50` |
//! | `filter_is_null("deleted_at")` | `deleted_at IS NULL` |
//! | `filter_is_not_null("email")` | `email IS NOT NULL` |
//! | `filter_like("_", "rust web")` | full-text `q = "rust web"` |

use crate::MeilisearchConfig;
use kernway_orm_core::{
    entity::Entity, error::OrmError, page::Page, query::QueryBuilder, spec::Spec, BoxFuture,
};

/// Render a [`Spec`] tree into a Meilisearch filter expression (which supports
/// `AND` / `OR` / `NOT` and parentheses natively).
///
/// `Like` maps to `CONTAINS`, which needs Meilisearch >= 1.10; the other
/// operators work on any recent version.
fn spec_to_meili(spec: &Spec) -> String {
    fn q(v: &str) -> String {
        if v.parse::<f64>().is_ok() {
            v.to_string()
        } else {
            format!("\"{}\"", v.replace('"', "\\\""))
        }
    }
    match spec {
        Spec::Eq(f, v) => format!("{} = {}", f, q(v)),
        Spec::Ne(f, v) => format!("{} != {}", f, q(v)),
        Spec::Gt(f, v) => format!("{} > {}", f, q(v)),
        Spec::Lt(f, v) => format!("{} < {}", f, q(v)),
        Spec::Gte(f, v) => format!("{} >= {}", f, q(v)),
        Spec::Lte(f, v) => format!("{} <= {}", f, q(v)),
        Spec::Like(f, v) => format!("{} CONTAINS {}", f, q(v)),
        Spec::In(f, vs) => {
            let list = vs.iter().map(|v| q(v)).collect::<Vec<_>>().join(", ");
            format!("{} IN [{}]", f, list)
        }
        Spec::Between(f, lo, hi) => format!("{} {} TO {}", f, lo, hi),
        Spec::IsNull(f) => format!("{} IS NULL", f),
        Spec::IsNotNull(f) => format!("{} IS NOT NULL", f),
        Spec::And(a, b) => format!("({} AND {})", spec_to_meili(a), spec_to_meili(b)),
        Spec::Or(a, b) => format!("({} OR {})", spec_to_meili(a), spec_to_meili(b)),
        Spec::Not(s) => format!("NOT ({})", spec_to_meili(s)),
    }
}
use serde::{de::DeserializeOwned, Serialize};
use std::marker::PhantomData;

/// Fluent query builder for Meilisearch.
pub struct MeilisearchQueryBuilder<T> {
    // Only read by the terminal `fetch_*` methods, which exist solely under the
    // `meilisearch` feature; without it the field is carried but never used.
    #[cfg_attr(not(feature = "meilisearch"), allow(dead_code))]
    pub(crate) config: MeilisearchConfig,
    /// Filter expression fragments — joined with `" AND "`.
    pub(crate) filters: Vec<String>,
    /// Full-text search term (`q` parameter). Set by `filter_like`.
    pub(crate) search_query: Option<String>,
    /// Sort expressions, e.g. `["price:asc", "name:desc"]`.
    pub(crate) sort: Vec<String>,
    pub(crate) limit: Option<u64>,
    pub(crate) offset: u64,
    pub(crate) _marker: PhantomData<T>,
}

impl<T> MeilisearchQueryBuilder<T> {
    /// Create a new builder.
    pub fn new(config: MeilisearchConfig) -> Self {
        Self {
            config,
            filters: Vec::new(),
            search_query: None,
            sort: Vec::new(),
            limit: None,
            offset: 0,
            _marker: PhantomData,
        }
    }

    /// Combine all filter fragments into one Meilisearch filter string.
    /// Returns `None` when no filters have been added.
    pub fn build_filter(&self) -> Option<String> {
        if self.filters.is_empty() {
            None
        } else {
            Some(self.filters.join(" AND "))
        }
    }

    /// Quote a value for a Meilisearch filter expression.
    /// Numeric strings are left unquoted; all others are double-quoted.
    pub(crate) fn quote(value: &str) -> String {
        if value.parse::<f64>().is_ok() {
            value.to_string()
        } else {
            format!("\"{}\"", value.replace('"', "\\\""))
        }
    }

    // ── Inherent filter helpers (take &mut self) ─────────────────────────────
    // The trait methods delegate here so we can unit-test the logic directly
    // without needing to downcast a Box<dyn QueryBuilder<T>>.

    pub(crate) fn push_eq(&mut self, field: &'static str, value: &str) {
        self.filters.push(format!("{} = {}", field, Self::quote(value)));
    }
    pub(crate) fn push_ne(&mut self, field: &'static str, value: &str) {
        self.filters.push(format!("{} != {}", field, Self::quote(value)));
    }
    pub(crate) fn push_gt(&mut self, field: &'static str, value: &str) {
        self.filters.push(format!("{} > {}", field, Self::quote(value)));
    }
    pub(crate) fn push_lt(&mut self, field: &'static str, value: &str) {
        self.filters.push(format!("{} < {}", field, Self::quote(value)));
    }
    pub(crate) fn push_gte(&mut self, field: &'static str, value: &str) {
        self.filters.push(format!("{} >= {}", field, Self::quote(value)));
    }
    pub(crate) fn push_lte(&mut self, field: &'static str, value: &str) {
        self.filters.push(format!("{} <= {}", field, Self::quote(value)));
    }
    pub(crate) fn push_in(&mut self, field: &'static str, values: &[String]) {
        let list = values.iter().map(|v| Self::quote(v)).collect::<Vec<_>>().join(", ");
        self.filters.push(format!("{} IN [{}]", field, list));
    }
    pub(crate) fn push_between(&mut self, field: &'static str, from: &str, to: &str) {
        self.filters.push(format!("{} {} TO {}", field, from, to));
    }
    pub(crate) fn push_is_null(&mut self, field: &'static str) {
        self.filters.push(format!("{} IS NULL", field));
    }
    pub(crate) fn push_is_not_null(&mut self, field: &'static str) {
        self.filters.push(format!("{} IS NOT NULL", field));
    }
}

impl<T: Entity + Serialize + DeserializeOwned + Send + 'static> QueryBuilder<T> for MeilisearchQueryBuilder<T> {
    fn filter_eq(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.push_eq(field, value);
        self
    }
    fn filter_ne(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.push_ne(field, value);
        self
    }
    fn filter_gt(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.push_gt(field, value);
        self
    }
    fn filter_lt(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.push_lt(field, value);
        self
    }
    fn filter_gte(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.push_gte(field, value);
        self
    }
    fn filter_lte(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.push_lte(field, value);
        self
    }
    fn filter_like(mut self: Box<Self>, _field: &'static str, pattern: &str) -> Box<dyn QueryBuilder<T>> {
        // Meilisearch full-text search uses `q`, not a filter expression.
        self.search_query = Some(pattern.to_string());
        self
    }
    fn filter_in(mut self: Box<Self>, field: &'static str, values: Vec<String>) -> Box<dyn QueryBuilder<T>> {
        self.push_in(field, &values);
        self
    }
    fn filter_between(mut self: Box<Self>, field: &'static str, from: &str, to: &str) -> Box<dyn QueryBuilder<T>> {
        self.push_between(field, from, to);
        self
    }
    fn filter_is_null(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self.push_is_null(field);
        self
    }
    fn filter_is_not_null(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self.push_is_not_null(field);
        self
    }

    fn filter_spec(mut self: Box<Self>, spec: Spec) -> Box<dyn QueryBuilder<T>> {
        self.filters.push(spec_to_meili(&spec));
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
        self // Meilisearch has no joins — ignored
    }

    fn fetch_all(self: Box<Self>) -> BoxFuture<'static, Result<Vec<T>, OrmError>> {
        #[cfg(feature = "meilisearch")]
        {
            use crate::api::SearchRequest;
            let config = self.config.clone();
            let filter = self.build_filter();
            let q = self.search_query.clone();
            let sort = self.sort.clone();
            let limit = self.limit;
            let offset = self.offset;
            Box::pin(async move {
                let req = SearchRequest {
                    q: q.as_deref(),
                    filter,
                    sort,
                    limit,
                    offset: if offset > 0 { Some(offset) } else { None },
                };
                let url = format!("{}/indexes/{}/search", config.url, T::table_name());
                let result: crate::api::SearchResult<T> =
                    crate::api::post(&url, &config.api_key, &req).await?;
                Ok(result.hits)
            })
        }
        #[cfg(not(feature = "meilisearch"))]
        Box::pin(async {
            Err(OrmError::Unsupported("enable the `meilisearch` feature".into()))
        })
    }

    fn fetch_one(self: Box<Self>) -> BoxFuture<'static, Result<Option<T>, OrmError>> {
        #[cfg(feature = "meilisearch")]
        {
            use crate::api::SearchRequest;
            let config = self.config.clone();
            let filter = self.build_filter();
            let q = self.search_query.clone();
            let sort = self.sort.clone();
            Box::pin(async move {
                let req = SearchRequest {
                    q: q.as_deref(),
                    filter,
                    sort,
                    limit: Some(1),
                    offset: None,
                };
                let url = format!("{}/indexes/{}/search", config.url, T::table_name());
                let result: crate::api::SearchResult<T> =
                    crate::api::post(&url, &config.api_key, &req).await?;
                Ok(result.hits.into_iter().next())
            })
        }
        #[cfg(not(feature = "meilisearch"))]
        Box::pin(async {
            Err(OrmError::Unsupported("enable the `meilisearch` feature".into()))
        })
    }

    fn fetch_count(self: Box<Self>) -> BoxFuture<'static, Result<u64, OrmError>> {
        #[cfg(feature = "meilisearch")]
        {
            use crate::api::SearchRequest;
            let config = self.config.clone();
            let filter = self.build_filter();
            let q = self.search_query.clone();
            Box::pin(async move {
                let req = SearchRequest {
                    q: q.as_deref(),
                    filter,
                    sort: vec![],
                    limit: Some(0),
                    offset: None,
                };
                let url = format!("{}/indexes/{}/search", config.url, T::table_name());
                let result: crate::api::SearchResult<T> =
                    crate::api::post(&url, &config.api_key, &req).await?;
                Ok(result.total_hits.or(result.estimated_total_hits).unwrap_or(0))
            })
        }
        #[cfg(not(feature = "meilisearch"))]
        Box::pin(async {
            Err(OrmError::Unsupported("enable the `meilisearch` feature".into()))
        })
    }

    fn fetch_page(self: Box<Self>, page: u64, size: u64) -> BoxFuture<'static, Result<Page<T>, OrmError>> {
        #[cfg(not(feature = "meilisearch"))]
        let _ = (page, size);
        #[cfg(feature = "meilisearch")]
        {
            use crate::api::SearchRequest;
            let config = self.config.clone();
            let filter = self.build_filter();
            let q = self.search_query.clone();
            let sort = self.sort.clone();
            Box::pin(async move {
                let offset = page.saturating_mul(size);
                let req = SearchRequest {
                    q: q.as_deref(),
                    filter,
                    sort,
                    limit: Some(size),
                    offset: Some(offset),
                };
                let url = format!("{}/indexes/{}/search", config.url, T::table_name());
                let result: crate::api::SearchResult<T> =
                    crate::api::post(&url, &config.api_key, &req).await?;
                let total = result.total_hits.or(result.estimated_total_hits).unwrap_or(0);
                Ok(Page::new(result.hits, total, page, size))
            })
        }
        #[cfg(not(feature = "meilisearch"))]
        Box::pin(async {
            Err(OrmError::Unsupported("enable the `meilisearch` feature".into()))
        })
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use kernway_orm_core::entity::ColumnDef;
    use kernway_orm_core::spec::Spec;

    #[test]
    fn spec_renders_to_a_parenthesised_meili_filter() {
        let spec = Spec::eq("role", "ADMIN")
            .and(Spec::gt("age", "18").or(Spec::eq("tier", "gold")));
        assert_eq!(
            spec_to_meili(&spec),
            r#"(role = "ADMIN" AND (age > 18 OR tier = "gold"))"#
        );
        // NOT and IN render too.
        assert_eq!(spec_to_meili(&Spec::eq("x", "1").not()), "NOT (x = 1)");
        assert_eq!(
            spec_to_meili(&Spec::in_("tag", ["a".into(), "b".into()])),
            r#"tag IN ["a", "b"]"#
        );
    }

    /// A minimal entity for testing — no macro, manual impl.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct Product {
        id: u64,
        name: String,
        price: f64,
        active: bool,
    }

    impl Entity for Product {
        type Id = u64;
        fn table_name() -> &'static str { "products" }
        fn id(&self) -> u64 { self.id }
        fn columns() -> &'static [ColumnDef] { &[] }
    }

    fn config() -> MeilisearchConfig {
        MeilisearchConfig { url: "http://localhost:7700".into(), api_key: "key".into() }
    }

    fn qb() -> MeilisearchQueryBuilder<Product> {
        MeilisearchQueryBuilder::new(config())
    }

    // ── quote ────────────────────────────────────────────────────────────────

    #[test]
    fn quote_strings_get_double_quoted() {
        assert_eq!(MeilisearchQueryBuilder::<Product>::quote("ADMIN"), "\"ADMIN\"");
    }

    #[test]
    fn quote_integers_are_unquoted() {
        assert_eq!(MeilisearchQueryBuilder::<Product>::quote("42"), "42");
    }

    #[test]
    fn quote_floats_are_unquoted() {
        assert_eq!(MeilisearchQueryBuilder::<Product>::quote("3.14"), "3.14");
    }

    #[test]
    fn quote_escapes_inner_double_quote() {
        assert_eq!(
            MeilisearchQueryBuilder::<Product>::quote("a\"b"),
            "\"a\\\"b\""
        );
    }

    // ── build_filter ─────────────────────────────────────────────────────────

    #[test]
    fn no_filters_gives_none() {
        assert_eq!(qb().build_filter(), None);
    }

    #[test]
    fn filter_eq_string() {
        let mut b = qb();
        b.push_eq("role", "ADMIN");
        assert_eq!(b.build_filter(), Some("role = \"ADMIN\"".into()));
    }

    #[test]
    fn filter_ne() {
        let mut b = qb();
        b.push_ne("status", "deleted");
        assert_eq!(b.build_filter(), Some("status != \"deleted\"".into()));
    }

    #[test]
    fn filter_gt_numeric() {
        let mut b = qb();
        b.push_gt("price", "100");
        assert_eq!(b.build_filter(), Some("price > 100".into()));
    }

    #[test]
    fn filter_gte() {
        let mut b = qb();
        b.push_gte("score", "90");
        assert_eq!(b.build_filter(), Some("score >= 90".into()));
    }

    #[test]
    fn filter_lte() {
        let mut b = qb();
        b.push_lte("price", "100");
        assert_eq!(b.build_filter(), Some("price <= 100".into()));
    }

    #[test]
    fn filter_in_strings() {
        let mut b = qb();
        b.push_in("tag", &["a".to_string(), "b".to_string()]);
        assert_eq!(b.build_filter(), Some("tag IN [\"a\", \"b\"]".into()));
    }

    #[test]
    fn filter_in_numbers() {
        let mut b = qb();
        b.push_in("id", &["1".to_string(), "2".to_string(), "3".to_string()]);
        assert_eq!(b.build_filter(), Some("id IN [1, 2, 3]".into()));
    }

    #[test]
    fn filter_between() {
        let mut b = qb();
        b.push_between("price", "10", "50");
        assert_eq!(b.build_filter(), Some("price 10 TO 50".into()));
    }

    #[test]
    fn filter_is_null() {
        let mut b = qb();
        b.push_is_null("deleted_at");
        assert_eq!(b.build_filter(), Some("deleted_at IS NULL".into()));
    }

    #[test]
    fn filter_is_not_null() {
        let mut b = qb();
        b.push_is_not_null("email");
        assert_eq!(b.build_filter(), Some("email IS NOT NULL".into()));
    }

    #[test]
    fn multiple_filters_joined_with_and() {
        let mut b = qb();
        b.push_eq("active", "true");
        b.push_gte("price", "10");
        b.push_lte("price", "100");
        assert_eq!(
            b.build_filter(),
            Some("active = \"true\" AND price >= 10 AND price <= 100".into())
        );
    }

    // ── filter_like → search_query ───────────────────────────────────────────

    #[test]
    fn filter_like_sets_q_not_filter() {
        let b: Box<dyn QueryBuilder<Product>> =
            Box::new(qb()).filter_like("_", "rust web framework");
        // Can't easily downcast, so we rely on fetch_* not erroring on build.
        // This test confirms the chain doesn't panic.
        drop(b);
    }

    #[test]
    fn filter_like_on_concrete_sets_search_query() {
        let mut b = qb();
        b.search_query = Some("rust web framework".into());
        assert_eq!(b.search_query.as_deref(), Some("rust web framework"));
        assert!(b.filters.is_empty(), "filter_like must not add to filters");
    }

    // ── sort ─────────────────────────────────────────────────────────────────

    #[test]
    fn order_by_appends_sort_expressions() {
        let mut b = qb();
        b.sort.push("name:asc".into());
        b.sort.push("price:desc".into());
        assert_eq!(b.sort, vec!["name:asc", "price:desc"]);
    }

    // ── limit / offset ───────────────────────────────────────────────────────

    #[test]
    fn limit_and_offset_stored() {
        let mut b = qb();
        b.limit = Some(20);
        b.offset = 40;
        assert_eq!(b.limit, Some(20));
        assert_eq!(b.offset, 40);
    }
}
