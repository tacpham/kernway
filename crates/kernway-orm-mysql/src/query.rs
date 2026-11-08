//! Fluent MySQL query builder — renders values as inlined escaped literals and
//! runs each terminal op asynchronously on a pooled per-core connection.

use crate::pool::Pool;
use crate::protocol::QueryResult;
use crate::value::{escape_like, filter_literal_for_field, resolve_column_name, row_to_entity};
use kernway_orm_core::{
    entity::Entity, error::OrmError, page::Page, query::QueryBuilder, spec::Spec, BoxFuture,
};
use serde::{de::DeserializeOwned, Serialize};
use std::marker::PhantomData;
use std::rc::Rc;

#[derive(Clone)]
enum SortDir {
    Asc,
    Desc,
}

/// Fluent MySQL query builder.
pub struct MySqlQueryBuilder<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    pool: Rc<Pool>,
    predicates: Vec<String>,
    spec: Option<Spec>,
    order: Vec<(&'static str, SortDir)>,
    lim: Option<u64>,
    off: u64,
    error: Option<OrmError>,
    _marker: PhantomData<T>,
}

impl<T> MySqlQueryBuilder<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    /// Create a query builder over a shared connection pool.
    pub fn new(pool: Rc<Pool>) -> Self {
        Self {
            pool,
            predicates: Vec::new(),
            spec: None,
            order: Vec::new(),
            lim: None,
            off: 0,
            error: None,
            _marker: PhantomData,
        }
    }

    fn resolve(&mut self, field: &str) -> Option<&'static str> {
        match resolve_column_name::<T>(field) {
            Some(c) => Some(c),
            None => {
                if self.error.is_none() {
                    self.error = Some(OrmError::Query(format!(
                        "unknown field '{}' for entity '{}'",
                        field,
                        T::table_name()
                    )));
                }
                None
            }
        }
    }

    fn validate_spec(&mut self, spec: &Spec) {
        match spec {
            Spec::And(a, b) | Spec::Or(a, b) => {
                self.validate_spec(a);
                self.validate_spec(b);
            }
            Spec::Not(s) => self.validate_spec(s),
            Spec::Eq(f, _)
            | Spec::Ne(f, _)
            | Spec::Gt(f, _)
            | Spec::Lt(f, _)
            | Spec::Gte(f, _)
            | Spec::Lte(f, _)
            | Spec::Like(f, _)
            | Spec::In(f, _)
            | Spec::Between(f, _, _)
            | Spec::IsNull(f)
            | Spec::IsNotNull(f) => {
                self.resolve(f);
            }
        }
    }

    fn where_clause(&self) -> String {
        let mut parts = self.predicates.clone();
        if let Some(spec) = &self.spec {
            parts.push(spec_to_sql::<T>(spec));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", parts.join(" AND "))
        }
    }

    fn col_list() -> String {
        T::columns()
            .iter()
            .map(|c| format!("`{}`", c.name))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn select_sql(&self) -> String {
        let mut sql = format!(
            "SELECT {} FROM `{}`{}",
            Self::col_list(),
            T::table_name(),
            self.where_clause()
        );
        if !self.order.is_empty() {
            let parts: Vec<String> = self
                .order
                .iter()
                .map(|(col, dir)| match dir {
                    SortDir::Asc => format!("`{}` ASC", col),
                    SortDir::Desc => format!("`{}` DESC", col),
                })
                .collect();
            sql.push_str(" ORDER BY ");
            sql.push_str(&parts.join(", "));
        }
        if let Some(limit) = self.lim {
            sql.push_str(&format!(" LIMIT {}", limit));
        }
        if self.off > 0 {
            sql.push_str(&format!(" OFFSET {}", self.off));
        }
        sql
    }

    fn count_sql(&self) -> String {
        format!("SELECT COUNT(*) FROM `{}`{}", T::table_name(), self.where_clause())
    }

    /// Acquire a pooled connection and run `sql`, poisoning it on a
    /// connection-level failure.
    async fn run(&self, sql: &str) -> Result<QueryResult, OrmError> {
        let mut g = self.pool.acquire().await?;
        let r = g.query(sql).await;
        if matches!(r, Err(OrmError::Connection(_))) {
            g.poison();
        }
        r
    }

    async fn fetch_all_run(mut self: Box<Self>) -> Result<Vec<T>, OrmError> {
        if let Some(err) = self.error.take() {
            return Err(err);
        }
        let sql = self.select_sql();
        match self.run(&sql).await? {
            QueryResult::Rows { rows, .. } => rows
                .iter()
                .map(|r| row_to_entity::<T>(r, T::columns()))
                .collect(),
            QueryResult::Ok { .. } => Ok(Vec::new()),
        }
    }

    async fn fetch_one_run(mut self: Box<Self>) -> Result<Option<T>, OrmError> {
        self.lim = Some(1);
        Ok(self.fetch_all_run().await?.into_iter().next())
    }

    async fn fetch_count_run(mut self: Box<Self>) -> Result<u64, OrmError> {
        if let Some(err) = self.error.take() {
            return Err(err);
        }
        let sql = self.count_sql();
        Ok(scalar_u64(self.run(&sql).await?))
    }

    async fn fetch_page_run(mut self: Box<Self>, page: u64, size: u64) -> Result<Page<T>, OrmError> {
        if let Some(err) = self.error.take() {
            return Err(err);
        }
        let count_sql = self.count_sql();
        let total = scalar_u64(self.run(&count_sql).await?);
        if size == 0 {
            return Ok(Page::new(Vec::new(), total, page, size));
        }
        self.off = page.saturating_mul(size);
        self.lim = Some(size);
        let items = self.fetch_all_run().await?;
        Ok(Page::new(items, total, page, size))
    }
}

fn scalar_u64(result: QueryResult) -> u64 {
    match result {
        QueryResult::Rows { rows, .. } => rows
            .first()
            .and_then(|r| r.first())
            .and_then(|c| c.as_ref())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0),
        QueryResult::Ok { .. } => 0,
    }
}

macro_rules! cmp_filter {
    ($self:ident, $field:ident, $value:ident, $op:literal) => {{
        if let Some(col) = $self.resolve($field) {
            let lit = filter_literal_for_field::<T>($field, $value);
            $self.predicates.push(format!("`{}` {} {}", col, $op, lit));
        }
        $self
    }};
}

impl<T> QueryBuilder<T> for MySqlQueryBuilder<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    fn filter_eq(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        cmp_filter!(self, field, value, "=")
    }
    fn filter_ne(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        cmp_filter!(self, field, value, "!=")
    }
    fn filter_gt(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        cmp_filter!(self, field, value, ">")
    }
    fn filter_lt(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        cmp_filter!(self, field, value, "<")
    }
    fn filter_gte(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        cmp_filter!(self, field, value, ">=")
    }
    fn filter_lte(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        cmp_filter!(self, field, value, "<=")
    }

    fn filter_like(mut self: Box<Self>, field: &'static str, pattern: &str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve(field) {
            self.predicates.push(format!("`{}` LIKE '{}'", col, escape_like(pattern)));
        }
        self
    }

    fn filter_in(mut self: Box<Self>, field: &'static str, values: Vec<String>) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve(field) {
            if values.is_empty() {
                self.predicates.push("1 = 0".to_string());
            } else {
                let lits: Vec<String> = values
                    .iter()
                    .map(|v| filter_literal_for_field::<T>(field, v))
                    .collect();
                self.predicates.push(format!("`{}` IN ({})", col, lits.join(", ")));
            }
        }
        self
    }

    fn filter_between(
        mut self: Box<Self>,
        field: &'static str,
        from: &str,
        to: &str,
    ) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve(field) {
            let from = filter_literal_for_field::<T>(field, from);
            let to = filter_literal_for_field::<T>(field, to);
            self.predicates.push(format!("`{}` BETWEEN {} AND {}", col, from, to));
        }
        self
    }

    fn filter_is_null(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve(field) {
            self.predicates.push(format!("`{}` IS NULL", col));
        }
        self
    }

    fn filter_is_not_null(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve(field) {
            self.predicates.push(format!("`{}` IS NOT NULL", col));
        }
        self
    }

    fn filter_exists(self: Box<Self>, _field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self
    }
    fn filter_not_exists(self: Box<Self>, _field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self
    }

    fn filter_spec(mut self: Box<Self>, spec: Spec) -> Box<dyn QueryBuilder<T>> {
        self.validate_spec(&spec);
        self.spec = Some(spec);
        self
    }

    fn order_by_asc(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve(field) {
            self.order.push((col, SortDir::Asc));
        }
        self
    }
    fn order_by_desc(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve(field) {
            self.order.push((col, SortDir::Desc));
        }
        self
    }

    fn limit(mut self: Box<Self>, n: u64) -> Box<dyn QueryBuilder<T>> {
        self.lim = Some(n);
        self
    }
    fn offset(mut self: Box<Self>, n: u64) -> Box<dyn QueryBuilder<T>> {
        self.off = n;
        self
    }
    fn with(self: Box<Self>, _relation: &'static str) -> Box<dyn QueryBuilder<T>> {
        self
    }

    fn fetch_all(self: Box<Self>) -> BoxFuture<'static, Result<Vec<T>, OrmError>> {
        Box::pin(async move { self.fetch_all_run().await })
    }
    fn fetch_one(self: Box<Self>) -> BoxFuture<'static, Result<Option<T>, OrmError>> {
        Box::pin(async move { self.fetch_one_run().await })
    }
    fn fetch_count(self: Box<Self>) -> BoxFuture<'static, Result<u64, OrmError>> {
        Box::pin(async move { self.fetch_count_run().await })
    }
    fn fetch_page(self: Box<Self>, page: u64, size: u64) -> BoxFuture<'static, Result<Page<T>, OrmError>> {
        Box::pin(async move { self.fetch_page_run(page, size).await })
    }
}

/// Render a [`Spec`] into an inlined-literal SQL boolean expression.
fn spec_to_sql<T>(spec: &Spec) -> String
where
    T: Entity + Serialize + DeserializeOwned,
{
    let col = |field: &str| resolve_column_name::<T>(field).unwrap_or(field).to_string();
    match spec {
        Spec::Eq(f, v) => format!("`{}` = {}", col(f), filter_literal_for_field::<T>(f, v)),
        Spec::Ne(f, v) => format!("`{}` != {}", col(f), filter_literal_for_field::<T>(f, v)),
        Spec::Gt(f, v) => format!("`{}` > {}", col(f), filter_literal_for_field::<T>(f, v)),
        Spec::Lt(f, v) => format!("`{}` < {}", col(f), filter_literal_for_field::<T>(f, v)),
        Spec::Gte(f, v) => format!("`{}` >= {}", col(f), filter_literal_for_field::<T>(f, v)),
        Spec::Lte(f, v) => format!("`{}` <= {}", col(f), filter_literal_for_field::<T>(f, v)),
        Spec::Like(f, v) => format!("`{}` LIKE '{}'", col(f), escape_like(v)),
        Spec::In(f, vs) => {
            let lits: Vec<String> = vs.iter().map(|v| filter_literal_for_field::<T>(f, v)).collect();
            format!("`{}` IN ({})", col(f), lits.join(", "))
        }
        Spec::Between(f, lo, hi) => format!(
            "`{}` BETWEEN {} AND {}",
            col(f),
            filter_literal_for_field::<T>(f, lo),
            filter_literal_for_field::<T>(f, hi)
        ),
        Spec::IsNull(f) => format!("`{}` IS NULL", col(f)),
        Spec::IsNotNull(f) => format!("`{}` IS NOT NULL", col(f)),
        Spec::And(a, b) => format!("({} AND {})", spec_to_sql::<T>(a), spec_to_sql::<T>(b)),
        Spec::Or(a, b) => format!("({} OR {})", spec_to_sql::<T>(a), spec_to_sql::<T>(b)),
        Spec::Not(s) => format!("NOT ({})", spec_to_sql::<T>(s)),
    }
}
