//! The fluent query builder, shared by every SQL backend. Filter values are
//! always bound parameters; placeholders are numbered when the final
//! statement is assembled, so `?`, `$n` and `:n` dialects all work.

use crate::engine::{col_list, Sql};
use crate::param::{column_for_field, filter_param, Param};
use crate::{SqlConn, SqlConnect};
use kernway_orm_core::{entity::Entity, error::OrmError, page::Page, query::QueryBuilder, spec::Spec, BoxFuture};
use kernway_wire::Pool;
use serde::{de::DeserializeOwned, Serialize};
use std::marker::PhantomData;
use std::rc::Rc;

/// A piece of a predicate.
#[derive(Clone)]
pub(crate) enum Frag {
    Raw(&'static str),
    Ident(&'static str),
    Param(Param),
}

/// Render a [`Spec`] over entity `U`'s fields into fragments, each column
/// prefixed with `qual` (`""`, or a table alias such as `"t1."`). An unknown
/// field records an error in `error` and renders as `1 = 0`.
pub(crate) fn spec_frags<U: Entity>(spec: &Spec, qual: &'static str, out: &mut Vec<Frag>, error: &mut Option<OrmError>) {
    let mut cmp = |f: &str, op: &'static str, v: &str, out: &mut Vec<Frag>| {
        if qcol::<U>(f, qual, out, error) {
            out.extend([Frag::Raw(op), Frag::Param(filter_param::<U>(f, v))]);
        }
    };
    match spec {
        Spec::Eq(f, v) => cmp(f, " = ", v, out),
        Spec::Ne(f, v) => cmp(f, " <> ", v, out),
        Spec::Gt(f, v) => cmp(f, " > ", v, out),
        Spec::Lt(f, v) => cmp(f, " < ", v, out),
        Spec::Gte(f, v) => cmp(f, " >= ", v, out),
        Spec::Lte(f, v) => cmp(f, " <= ", v, out),
        Spec::Like(f, v) => {
            if qcol::<U>(f, qual, out, error) {
                out.extend([Frag::Raw(" LIKE "), Frag::Param(Param::Str(v.clone()))]);
            }
        }
        Spec::In(_, vs) if vs.is_empty() => out.push(Frag::Raw("1 = 0")),
        Spec::In(f, vs) => {
            if qcol::<U>(f, qual, out, error) {
                out.push(Frag::Raw(" IN ("));
                for (i, v) in vs.iter().enumerate() {
                    if i > 0 {
                        out.push(Frag::Raw(", "));
                    }
                    out.push(Frag::Param(filter_param::<U>(f, v)));
                }
                out.push(Frag::Raw(")"));
            }
        }
        Spec::Between(f, lo, hi) => {
            if qcol::<U>(f, qual, out, error) {
                out.extend([
                    Frag::Raw(" BETWEEN "),
                    Frag::Param(filter_param::<U>(f, lo)),
                    Frag::Raw(" AND "),
                    Frag::Param(filter_param::<U>(f, hi)),
                ]);
            }
        }
        Spec::IsNull(f) => {
            if qcol::<U>(f, qual, out, error) {
                out.push(Frag::Raw(" IS NULL"));
            }
        }
        Spec::IsNotNull(f) => {
            if qcol::<U>(f, qual, out, error) {
                out.push(Frag::Raw(" IS NOT NULL"));
            }
        }
        Spec::And(a, b) | Spec::Or(a, b) => {
            let op = if matches!(spec, Spec::And(..)) { " AND " } else { " OR " };
            out.push(Frag::Raw("("));
            spec_frags::<U>(a, qual, out, error);
            out.push(Frag::Raw(op));
            spec_frags::<U>(b, qual, out, error);
            out.push(Frag::Raw(")"));
        }
        Spec::Not(inner) => {
            out.push(Frag::Raw("NOT ("));
            spec_frags::<U>(inner, qual, out, error);
            out.push(Frag::Raw(")"));
        }
    }
}

/// Push `qual` + the quoted column of `U`'s field `f`; an unknown field
/// records an error and pushes `1 = 0` instead (returns false).
fn qcol<U: Entity>(f: &str, qual: &'static str, out: &mut Vec<Frag>, error: &mut Option<OrmError>) -> bool {
    match column_for_field::<U>(f) {
        Some(c) => {
            if !qual.is_empty() {
                out.push(Frag::Raw(qual));
            }
            out.push(Frag::Ident(c.name));
            true
        }
        None => {
            error.get_or_insert_with(|| OrmError::Query(format!("unknown field '{f}' for entity '{}'", U::table_name())));
            out.push(Frag::Raw("1 = 0"));
            false
        }
    }
}

/// Append predicate fragments to a statement.
pub(crate) fn push_frags(q: &mut Sql, frags: &[Frag]) {
    for f in frags {
        match f {
            Frag::Raw(s) => {
                q.push(s);
            }
            Frag::Ident(n) => {
                q.ident(n);
            }
            Frag::Param(p) => {
                q.param(p.clone());
            }
        }
    }
}

/// The query builder over a pool of `C` connections.
pub struct SqlQueryBuilder<T, C: SqlConnect> {
    pool: Rc<Pool<C>>,
    predicates: Vec<Vec<Frag>>,
    order: Vec<(&'static str, bool)>,
    limit: Option<u64>,
    offset: u64,
    error: Option<OrmError>,
    _marker: PhantomData<T>,
}

impl<T, C> SqlQueryBuilder<T, C>
where
    T: Entity + Serialize + DeserializeOwned,
    C: SqlConnect,
{
    /// A builder over a shared pool.
    pub fn new(pool: Rc<Pool<C>>) -> Self {
        Self { pool, predicates: Vec::new(), order: Vec::new(), limit: None, offset: 0, error: None, _marker: PhantomData }
    }

    fn resolve(&mut self, field: &str) -> Option<&'static str> {
        match column_for_field::<T>(field) {
            Some(c) => Some(c.name),
            None => {
                self.error.get_or_insert_with(|| {
                    OrmError::Query(format!("unknown field '{field}' for entity '{}'", T::table_name()))
                });
                None
            }
        }
    }

    fn cmp(mut self: Box<Self>, field: &str, op: &'static str, value: &str) -> Box<Self> {
        if let Some(col) = self.resolve(field) {
            self.predicates.push(vec![Frag::Ident(col), Frag::Raw(op), Frag::Param(filter_param::<T>(field, value))]);
        }
        self
    }

    /// Render a [`Spec`] into fragments; unknown fields record an error.
    fn spec_frags(&mut self, spec: &Spec, out: &mut Vec<Frag>) {
        spec_frags::<T>(spec, "", out, &mut self.error);
    }

    fn where_into(&self, q: &mut Sql) {
        for (i, pred) in self.predicates.iter().enumerate() {
            q.push(if i == 0 { " WHERE " } else { " AND " });
            push_frags(q, pred);
        }
    }

    fn select_sql(&self) -> Sql {
        let d = <C::Sql as SqlConn>::dialect();
        let mut q = Sql::new(d);
        q.push("SELECT ").push(&col_list::<T>(d)).push(" FROM ").ident(T::table_name());
        self.where_into(&mut q);
        for (i, (col, asc)) in self.order.iter().enumerate() {
            q.push(if i == 0 { " ORDER BY " } else { ", " }).ident(col).push(if *asc { " ASC" } else { " DESC" });
        }
        let window = d.limit_offset(self.limit, self.offset);
        q.push(&window);
        q
    }

    fn count_sql(&self) -> Sql {
        let mut q = Sql::new(<C::Sql as SqlConn>::dialect());
        q.push("SELECT COUNT(*) FROM ").ident(T::table_name());
        self.where_into(&mut q);
        q
    }

    async fn rows(&self) -> Result<Vec<T>, OrmError> {
        let q = self.select_sql();
        let mut g = self.pool.acquire().await?;
        let r = g.select::<T>(&q.text, &q.params).await;
        if matches!(r, Err(OrmError::Connection(_))) {
            g.poison();
        }
        r
    }

    async fn total(&self) -> Result<u64, OrmError> {
        let q = self.count_sql();
        let mut g = self.pool.acquire().await?;
        let r = g.select_count(&q.text, &q.params).await;
        if matches!(r, Err(OrmError::Connection(_))) {
            g.poison();
        }
        r
    }

    fn take_error(&mut self) -> Result<(), OrmError> {
        self.error.take().map_or(Ok(()), Err)
    }
}

impl<T, C> QueryBuilder<T> for SqlQueryBuilder<T, C>
where
    T: Entity + Serialize + DeserializeOwned + 'static,
    T::Id: Serialize + DeserializeOwned,
    C: SqlConnect,
{
    fn filter_eq(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.cmp(field, " = ", value)
    }
    fn filter_ne(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.cmp(field, " <> ", value)
    }
    fn filter_gt(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.cmp(field, " > ", value)
    }
    fn filter_lt(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.cmp(field, " < ", value)
    }
    fn filter_gte(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.cmp(field, " >= ", value)
    }
    fn filter_lte(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.cmp(field, " <= ", value)
    }
    fn filter_like(mut self: Box<Self>, field: &'static str, pattern: &str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve(field) {
            self.predicates.push(vec![Frag::Ident(col), Frag::Raw(" LIKE "), Frag::Param(Param::Str(pattern.to_string()))]);
        }
        self
    }
    fn filter_in(mut self: Box<Self>, field: &'static str, values: Vec<String>) -> Box<dyn QueryBuilder<T>> {
        let mut frags = Vec::new();
        self.spec_frags(&Spec::In(field, values), &mut frags);
        self.predicates.push(frags);
        self
    }
    fn filter_between(mut self: Box<Self>, field: &'static str, from: &str, to: &str) -> Box<dyn QueryBuilder<T>> {
        let mut frags = Vec::new();
        self.spec_frags(&Spec::Between(field, from.to_string(), to.to_string()), &mut frags);
        self.predicates.push(frags);
        self
    }
    fn filter_is_null(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve(field) {
            self.predicates.push(vec![Frag::Ident(col), Frag::Raw(" IS NULL")]);
        }
        self
    }
    fn filter_is_not_null(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve(field) {
            self.predicates.push(vec![Frag::Ident(col), Frag::Raw(" IS NOT NULL")]);
        }
        self
    }
    /// Document-store field existence; a no-op for SQL tables (every column exists).
    fn filter_exists(self: Box<Self>, _field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self
    }
    /// See [`filter_exists`](Self::filter_exists).
    fn filter_not_exists(self: Box<Self>, _field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self
    }
    fn filter_spec(mut self: Box<Self>, spec: Spec) -> Box<dyn QueryBuilder<T>> {
        let mut frags = Vec::new();
        self.spec_frags(&spec, &mut frags);
        self.predicates.push(frags);
        self
    }
    fn order_by_asc(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve(field) {
            self.order.push((col, true));
        }
        self
    }
    fn order_by_desc(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve(field) {
            self.order.push((col, false));
        }
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
    /// Relation loading is not implemented by the SQL backends; a no-op.
    fn with(self: Box<Self>, _relation: &'static str) -> Box<dyn QueryBuilder<T>> {
        self
    }
    fn fetch_all(mut self: Box<Self>) -> BoxFuture<'static, Result<Vec<T>, OrmError>> {
        Box::pin(async move {
            self.take_error()?;
            self.rows().await
        })
    }
    fn fetch_one(mut self: Box<Self>) -> BoxFuture<'static, Result<Option<T>, OrmError>> {
        Box::pin(async move {
            self.take_error()?;
            self.limit = Some(1);
            Ok(self.rows().await?.into_iter().next())
        })
    }
    fn fetch_count(mut self: Box<Self>) -> BoxFuture<'static, Result<u64, OrmError>> {
        Box::pin(async move {
            self.take_error()?;
            self.total().await
        })
    }
    fn fetch_page(mut self: Box<Self>, page: u64, size: u64) -> BoxFuture<'static, Result<Page<T>, OrmError>> {
        Box::pin(async move {
            self.take_error()?;
            let total = self.total().await?;
            if size == 0 {
                return Ok(Page::new(Vec::new(), total, page, size));
            }
            self.offset = page.saturating_mul(size);
            self.limit = Some(size);
            let items = self.rows().await?;
            Ok(Page::new(items, total, page, size))
        })
    }
}
