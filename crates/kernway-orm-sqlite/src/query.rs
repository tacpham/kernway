use crate::repository::{column_for_field, filter_value_for_field, map_err, row_to_entity};
use kernway_orm_core::{
    entity::Entity,
    error::OrmError,
    page::Page,
    query::QueryBuilder,
    spec::Spec,
    BoxFuture,
};
use rusqlite::{params_from_iter, types::Value as SqlValue, Connection};
use serde::{de::DeserializeOwned, Serialize};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
enum Filter {
    Eq { col: &'static str, val: SqlValue },
    Ne { col: &'static str, val: SqlValue },
    Gt { col: &'static str, val: SqlValue },
    Lt { col: &'static str, val: SqlValue },
    Gte { col: &'static str, val: SqlValue },
    Lte { col: &'static str, val: SqlValue },
    Like { col: &'static str, pat: String },
    In { col: &'static str, vals: Vec<SqlValue> },
    Between { col: &'static str, from: SqlValue, to: SqlValue },
    IsNull { col: &'static str },
    IsNotNull { col: &'static str },
}

#[derive(Clone)]
enum SortDir {
    Asc,
    Desc,
}

#[derive(Clone)]
struct Sort {
    col: &'static str,
    dir: SortDir,
}

/// Fluent SQLite query builder.
///
/// It accumulates filters, ordering, and limits, then renders them into a
/// single parameterised `SELECT` at the terminal operation.
pub struct SqliteQueryBuilder<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    conn: Arc<Mutex<Connection>>,
    filters: Vec<Filter>,
    spec: Option<Spec>,
    order: Vec<Sort>,
    lim: Option<u64>,
    off: u64,
    error: Option<OrmError>,
    _marker: PhantomData<T>,
}

impl<T> SqliteQueryBuilder<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    /// Create a query builder over a shared SQLite connection.
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self {
            conn,
            filters: Vec::new(),
            spec: None,
            order: Vec::new(),
            lim: None,
            off: 0,
            error: None,
            _marker: PhantomData,
        }
    }

    /// Validate every field a spec references, so an unknown field is reported
    /// (the same way the fluent filters resolve columns as they are added).
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
                self.resolve_col(f);
            }
        }
    }

    fn resolve_col(&mut self, field: &str) -> Option<&'static str> {
        match column_for_field::<T>(field) {
            Some(c) => Some(c.name),
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

    fn build_where_clause(&self) -> (String, Vec<SqlValue>) {
        let mut parts = Vec::new();
        let mut params = Vec::new();

        for filter in &self.filters {
            match filter {
                Filter::Eq { col, val } => {
                    let idx = params.len() + 1;
                    parts.push(format!("{} = ?{}", col, idx));
                    params.push(val.clone());
                }
                Filter::Ne { col, val } => {
                    let idx = params.len() + 1;
                    parts.push(format!("{} != ?{}", col, idx));
                    params.push(val.clone());
                }
                Filter::Gt { col, val } => {
                    let idx = params.len() + 1;
                    parts.push(format!("{} > ?{}", col, idx));
                    params.push(val.clone());
                }
                Filter::Lt { col, val } => {
                    let idx = params.len() + 1;
                    parts.push(format!("{} < ?{}", col, idx));
                    params.push(val.clone());
                }
                Filter::Gte { col, val } => {
                    let idx = params.len() + 1;
                    parts.push(format!("{} >= ?{}", col, idx));
                    params.push(val.clone());
                }
                Filter::Lte { col, val } => {
                    let idx = params.len() + 1;
                    parts.push(format!("{} <= ?{}", col, idx));
                    params.push(val.clone());
                }
                Filter::Like { col, pat } => {
                    let idx = params.len() + 1;
                    parts.push(format!("{} LIKE ?{}", col, idx));
                    params.push(SqlValue::Text(pat.clone()));
                }
                Filter::In { col, vals } => {
                    if vals.is_empty() {
                        parts.push("1 = 0".to_string());
                    } else {
                        let placeholders: Vec<String> = (0..vals.len())
                            .map(|offset| format!("?{}", params.len() + offset + 1))
                            .collect();
                        parts.push(format!("{} IN ({})", col, placeholders.join(", ")));
                        params.extend(vals.iter().cloned());
                    }
                }
                Filter::Between { col, from, to } => {
                    let start = params.len() + 1;
                    parts.push(format!("{} BETWEEN ?{} AND ?{}", col, start, start + 1));
                    params.push(from.clone());
                    params.push(to.clone());
                }
                Filter::IsNull { col } => parts.push(format!("{} IS NULL", col)),
                Filter::IsNotNull { col } => parts.push(format!("{} IS NOT NULL", col)),
            }
        }

        if let Some(spec) = &self.spec {
            parts.push(spec_to_sql::<T>(spec, &mut params));
        }

        if parts.is_empty() {
            (String::new(), params)
        } else {
            (format!(" WHERE {}", parts.join(" AND ")), params)
        }
    }

    fn build_select_query(&self) -> (String, Vec<SqlValue>) {
        let col_list: Vec<&str> = T::columns().iter().map(|c| c.name).collect();
        let (where_clause, params) = self.build_where_clause();
        let mut sql = format!(
            "SELECT {} FROM {}{}",
            col_list.join(", "),
            T::table_name(),
            where_clause
        );

        if !self.order.is_empty() {
            let parts: Vec<String> = self
                .order
                .iter()
                .map(|order| match order.dir {
                    SortDir::Asc => format!("{} ASC", order.col),
                    SortDir::Desc => format!("{} DESC", order.col),
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

        (sql, params)
    }

    fn build_count_query(&self) -> (String, Vec<SqlValue>) {
        let (where_clause, params) = self.build_where_clause();
        (
            format!("SELECT COUNT(*) FROM {}{}", T::table_name(), where_clause),
            params,
        )
    }

    fn fetch_all_sync(mut self: Box<Self>) -> Result<Vec<T>, OrmError> {
        if let Some(err) = self.error.take() {
            return Err(err);
        }
        let cols = T::columns();
        let (sql, params) = self.build_select_query();
        let conn = self
            .conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        let mut stmt = conn.prepare(&sql).map_err(map_err)?;
        let rows = stmt
            .query_map(params_from_iter(params.iter()), |row| row_to_entity::<T>(row, cols))
            .map_err(map_err)?;
        rows.map(|row| row.map_err(map_err)).collect()
    }

    fn fetch_one_sync(mut self: Box<Self>) -> Result<Option<T>, OrmError> {
        self.lim = Some(1);
        let mut items = self.fetch_all_sync()?;
        Ok(items.pop())
    }

    fn fetch_count_sync(mut self: Box<Self>) -> Result<u64, OrmError> {
        if let Some(err) = self.error.take() {
            return Err(err);
        }
        let (sql, params) = self.build_count_query();
        let count: i64 = self
            .conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?
            .query_row(&sql, params_from_iter(params.iter()), |row| row.get(0))
            .map_err(map_err)?;
        Ok(count as u64)
    }

    fn fetch_page_sync(mut self: Box<Self>, page: u64, size: u64) -> Result<Page<T>, OrmError> {
        if let Some(err) = self.error.take() {
            return Err(err);
        }
        let (count_sql, count_params) = self.build_count_query();
        let total: i64 = self
            .conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?
            .query_row(&count_sql, params_from_iter(count_params.iter()), |row| {
                row.get(0)
            })
            .map_err(map_err)?;

        if size == 0 {
            return Ok(Page::new(Vec::new(), total as u64, page, size));
        }

        self.off = page.saturating_mul(size);
        self.lim = Some(size);
        let items = self.fetch_all_sync()?;
        Ok(Page::new(items, total as u64, page, size))
    }
}

impl<T> QueryBuilder<T> for SqliteQueryBuilder<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    fn filter_eq(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            let val = filter_value_for_field::<T>(field, value);
            self.filters.push(Filter::Eq { col, val });
        }
        self
    }

    fn filter_ne(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            let val = filter_value_for_field::<T>(field, value);
            self.filters.push(Filter::Ne { col, val });
        }
        self
    }

    fn filter_gt(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            let val = filter_value_for_field::<T>(field, value);
            self.filters.push(Filter::Gt { col, val });
        }
        self
    }

    fn filter_lt(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            let val = filter_value_for_field::<T>(field, value);
            self.filters.push(Filter::Lt { col, val });
        }
        self
    }

    fn filter_gte(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            let val = filter_value_for_field::<T>(field, value);
            self.filters.push(Filter::Gte { col, val });
        }
        self
    }

    fn filter_lte(mut self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            let val = filter_value_for_field::<T>(field, value);
            self.filters.push(Filter::Lte { col, val });
        }
        self
    }

    fn filter_like(
        mut self: Box<Self>,
        field: &'static str,
        pattern: &str,
    ) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            self.filters.push(Filter::Like {
                col,
                pat: pattern.to_string(),
            });
        }
        self
    }

    fn filter_in(
        mut self: Box<Self>,
        field: &'static str,
        values: Vec<String>,
    ) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            let vals = values
                .iter()
                .map(|value| filter_value_for_field::<T>(field, value))
                .collect();
            self.filters.push(Filter::In { col, vals });
        }
        self
    }

    fn filter_between(
        mut self: Box<Self>,
        field: &'static str,
        from: &str,
        to: &str,
    ) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            let from = filter_value_for_field::<T>(field, from);
            let to = filter_value_for_field::<T>(field, to);
            self.filters.push(Filter::Between { col, from, to });
        }
        self
    }

    fn filter_is_null(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            self.filters.push(Filter::IsNull { col });
        }
        self
    }

    fn filter_is_not_null(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            self.filters.push(Filter::IsNotNull { col });
        }
        self
    }

    fn filter_spec(mut self: Box<Self>, spec: Spec) -> Box<dyn QueryBuilder<T>> {
        self.validate_spec(&spec); // reports unknown fields via self.error
        self.spec = Some(spec);
        self
    }

    fn order_by_asc(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            self.order.push(Sort {
                col,
                dir: SortDir::Asc,
            });
        }
        self
    }

    fn order_by_desc(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            self.order.push(Sort {
                col,
                dir: SortDir::Desc,
            });
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
        Box::pin(async move {
            rt_core::spawn_blocking(move || self.fetch_all_sync())
                .await
                .ok_or_else(|| OrmError::Connection("blocking task panicked".to_string()))?
        })
    }

    fn fetch_one(self: Box<Self>) -> BoxFuture<'static, Result<Option<T>, OrmError>> {
        Box::pin(async move {
            rt_core::spawn_blocking(move || self.fetch_one_sync())
                .await
                .ok_or_else(|| OrmError::Connection("blocking task panicked".to_string()))?
        })
    }

    fn fetch_count(self: Box<Self>) -> BoxFuture<'static, Result<u64, OrmError>> {
        Box::pin(async move {
            rt_core::spawn_blocking(move || self.fetch_count_sync())
                .await
                .ok_or_else(|| OrmError::Connection("blocking task panicked".to_string()))?
        })
    }

    fn fetch_page(
        self: Box<Self>,
        page: u64,
        size: u64,
    ) -> BoxFuture<'static, Result<Page<T>, OrmError>> {
        Box::pin(async move {
            rt_core::spawn_blocking(move || self.fetch_page_sync(page, size))
                .await
                .ok_or_else(|| OrmError::Connection("blocking task panicked".to_string()))?
        })
    }
}

/// Render a [`Spec`] tree into a parameterised SQL boolean expression, appending
/// its bound values to `params` in placeholder order. Left-to-right evaluation of
/// `format!` arguments keeps `?N` numbering correct across AND/OR branches.
fn spec_to_sql<T>(spec: &Spec, params: &mut Vec<SqlValue>) -> String
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    let col = |field: &str| {
        column_for_field::<T>(field)
            .map(|c| c.name.to_string())
            .unwrap_or_else(|| field.to_string())
    };
    match spec {
        Spec::Eq(f, v) => {
            params.push(filter_value_for_field::<T>(f, v));
            format!("{} = ?{}", col(f), params.len())
        }
        Spec::Ne(f, v) => {
            params.push(filter_value_for_field::<T>(f, v));
            format!("{} != ?{}", col(f), params.len())
        }
        Spec::Gt(f, v) => {
            params.push(filter_value_for_field::<T>(f, v));
            format!("{} > ?{}", col(f), params.len())
        }
        Spec::Lt(f, v) => {
            params.push(filter_value_for_field::<T>(f, v));
            format!("{} < ?{}", col(f), params.len())
        }
        Spec::Gte(f, v) => {
            params.push(filter_value_for_field::<T>(f, v));
            format!("{} >= ?{}", col(f), params.len())
        }
        Spec::Lte(f, v) => {
            params.push(filter_value_for_field::<T>(f, v));
            format!("{} <= ?{}", col(f), params.len())
        }
        Spec::Like(f, v) => {
            params.push(SqlValue::Text(v.clone()));
            format!("{} LIKE ?{}", col(f), params.len())
        }
        Spec::In(f, vs) => {
            let ph: Vec<String> = vs
                .iter()
                .map(|v| {
                    params.push(filter_value_for_field::<T>(f, v));
                    format!("?{}", params.len())
                })
                .collect();
            format!("{} IN ({})", col(f), ph.join(", "))
        }
        Spec::Between(f, lo, hi) => {
            params.push(filter_value_for_field::<T>(f, lo));
            let a = params.len();
            params.push(filter_value_for_field::<T>(f, hi));
            let b = params.len();
            format!("{} BETWEEN ?{} AND ?{}", col(f), a, b)
        }
        Spec::IsNull(f) => format!("{} IS NULL", col(f)),
        Spec::IsNotNull(f) => format!("{} IS NOT NULL", col(f)),
        Spec::And(a, b) => format!(
            "({} AND {})",
            spec_to_sql::<T>(a, params),
            spec_to_sql::<T>(b, params)
        ),
        Spec::Or(a, b) => format!(
            "({} OR {})",
            spec_to_sql::<T>(a, params),
            spec_to_sql::<T>(b, params)
        ),
        Spec::Not(s) => format!("NOT ({})", spec_to_sql::<T>(s, params)),
    }
}
