//! [`Select`]: the query builder for what an entity repository cannot say —
//! joins between entities, `GROUP BY` with aggregates, `HAVING`. Written once
//! over the dialect, so the same query runs on every SQL backend. Columns are
//! named through entity fields ([`col`]), values are always bound parameters.
//!
//! ```ignore
//! #[derive(Deserialize)]
//! struct CustomerSales { name: String, orders: i64, revenue: f64 }
//!
//! let top: Vec<CustomerSales> = db
//!     .select::<Order>()
//!     .join::<Customer>(col::<Order>("customer_id"), "id")
//!     .filter(Spec::eq("status", "paid"))
//!     .group_by(col::<Customer>("name"))
//!     .aggregate("orders", Agg::Count)
//!     .aggregate("revenue", Agg::Sum(col::<Order>("total")))
//!     .having("orders", Cmp::Gte, 3)
//!     .order_by("revenue", false)
//!     .limit(10)
//!     .fetch_as()
//!     .await?;
//! ```

use crate::engine::Sql;
use crate::param::{column_for_field, Param};
use crate::query::{push_frags, spec_frags, Frag};
use crate::raw::Rows;
use crate::{SqlConnect, SqlDb, SqlTx};
use kernway_orm_core::{entity::Entity, error::OrmError, spec::Spec, SqlDialect};
use serde::de::DeserializeOwned;
use std::future::Future;
use std::marker::PhantomData;

/// Where a [`Select`] runs: a pooled database or a transaction.
pub trait RowSource {
    /// The SQL dialect of the backend.
    fn sql_dialect(&self) -> &'static dyn SqlDialect;
    /// Run a query and collect its rows.
    fn fetch_rows(&self, sql: &str, params: &[Param]) -> impl Future<Output = Result<Rows, OrmError>>;
}

impl<C: SqlConnect> RowSource for SqlDb<C> {
    fn sql_dialect(&self) -> &'static dyn SqlDialect {
        <C::Sql as crate::SqlConn>::dialect()
    }
    fn fetch_rows(&self, sql: &str, params: &[Param]) -> impl Future<Output = Result<Rows, OrmError>> {
        self.query(sql, params)
    }
}

impl<C: SqlConnect> RowSource for SqlTx<C> {
    fn sql_dialect(&self) -> &'static dyn SqlDialect {
        <C::Sql as crate::SqlConn>::dialect()
    }
    fn fetch_rows(&self, sql: &str, params: &[Param]) -> impl Future<Output = Result<Rows, OrmError>> {
        self.query(sql, params)
    }
}

/// A column named through an entity field: `col::<Order>("total")`.
#[derive(Debug, Clone, Copy)]
pub struct Col {
    table: &'static str,
    field: &'static str,
    column: Option<&'static str>,
}

/// The column behind `E`'s field `field` (or column name). An unknown field
/// makes the query fail when it runs.
pub fn col<E: Entity>(field: &'static str) -> Col {
    Col { table: E::table_name(), field, column: column_for_field::<E>(field).map(|c| c.name) }
}

/// An aggregate over the selected rows.
#[derive(Debug, Clone, Copy)]
pub enum Agg {
    /// `COUNT(*)`: rows in the group.
    Count,
    /// `COUNT(col)`: non-NULL values.
    CountOf(Col),
    /// `COUNT(DISTINCT col)`.
    CountDistinct(Col),
    /// `SUM(col)` (NULL for an empty group).
    Sum(Col),
    /// `AVG(col)`.
    Avg(Col),
    /// `MIN(col)`.
    Min(Col),
    /// `MAX(col)`.
    Max(Col),
}

/// A comparison for [`Select::having`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `>`
    Gt,
    /// `<`
    Lt,
    /// `>=`
    Gte,
    /// `<=`
    Lte,
}

impl Cmp {
    fn sql(self) -> &'static str {
        match self {
            Cmp::Eq => " = ",
            Cmp::Ne => " <> ",
            Cmp::Gt => " > ",
            Cmp::Lt => " < ",
            Cmp::Gte => " >= ",
            Cmp::Lte => " <= ",
        }
    }
}

/// Table aliases: `t0` is the root entity, `t1…` the joins in order.
const QUALS: [&str; 8] = ["t0.", "t1.", "t2.", "t3.", "t4.", "t5.", "t6.", "t7."];

#[derive(Clone, Copy, PartialEq, Eq)]
enum JoinKind {
    Inner,
    Left,
}

struct Join {
    kind: JoinKind,
    table: &'static str,
    /// `(alias of the left column, left column, right column)`.
    on: (usize, &'static str, &'static str),
}

/// What an output column or sort key is.
enum Output {
    Group(usize, &'static str),
    Measure(Agg),
}

/// A query over entity `T` and the entities joined to it — joins, `GROUP BY`
/// with aggregates, `HAVING` — started with [`SqlDb::select`] or
/// [`SqlTx::select`]:
///
/// ```ignore
/// let top: Vec<CustomerSales> = db
///     .select::<Order>()
///     .join::<Customer>(col::<Order>("customer_id"), "id")
///     .filter(Spec::eq("status", "paid"))
///     .group_by(col::<Customer>("name"))
///     .aggregate("orders", Agg::Count)
///     .aggregate("revenue", Agg::Sum(col::<Order>("total")))
///     .having("orders", Cmp::Gte, 3)
///     .order_by("revenue", false)
///     .fetch_as()
///     .await?;
/// ```
pub struct Select<'a, T, S: ?Sized> {
    src: &'a S,
    joins: Vec<Join>,
    filters: Vec<Vec<Frag>>,
    outputs: Vec<(String, Output)>,
    group_count: usize,
    having: Vec<(String, Cmp, Param)>,
    order: Vec<(OrderKey, bool)>,
    distinct: bool,
    limit: Option<u64>,
    offset: u64,
    error: Option<OrmError>,
    _t: PhantomData<T>,
}

enum OrderKey {
    Output(String),
    Column(usize, &'static str),
}

impl<C: SqlConnect> SqlDb<C> {
    /// Start a [`Select`] over entity `T` (joins, aggregates, `GROUP BY`).
    pub fn select<T: Entity>(&self) -> Select<'_, T, Self> {
        Select::new(self)
    }
}

impl<C: SqlConnect> SqlTx<C> {
    /// Start a [`Select`] over entity `T` inside the transaction.
    pub fn select<T: Entity>(&self) -> Select<'_, T, Self> {
        Select::new(self)
    }
}

impl<'a, T: Entity, S: RowSource + ?Sized> Select<'a, T, S> {
    fn new(src: &'a S) -> Self {
        Self {
            src,
            joins: Vec::new(),
            filters: Vec::new(),
            outputs: Vec::new(),
            group_count: 0,
            having: Vec::new(),
            order: Vec::new(),
            distinct: false,
            limit: None,
            offset: 0,
            error: None,
            _t: PhantomData,
        }
    }

    fn fail(&mut self, msg: String) {
        self.error.get_or_insert(OrmError::Query(msg));
    }

    /// The alias index of `table`: 0 for `T`, else its join.
    fn alias_of(&mut self, table: &str) -> Option<usize> {
        if table == T::table_name() {
            return Some(0);
        }
        match self.joins.iter().position(|j| j.table == table) {
            Some(i) => Some(i + 1),
            None => {
                self.fail(format!("table '{table}' is not part of the query (join it first)"));
                None
            }
        }
    }

    /// Resolve a [`Col`] to `(alias, column)`.
    fn resolve(&mut self, c: Col) -> Option<(usize, &'static str)> {
        let Some(column) = c.column else {
            self.fail(format!("unknown field '{}' of table '{}'", c.field, c.table));
            return None;
        };
        Some((self.alias_of(c.table)?, column))
    }

    fn add_join<U: Entity>(mut self, kind: JoinKind, left: Col, right_field: &'static str) -> Self {
        if U::table_name() == T::table_name() || self.joins.iter().any(|j| j.table == U::table_name()) {
            self.fail(format!("table '{}' is already part of the query", U::table_name()));
            return self;
        }
        if self.joins.len() + 1 >= QUALS.len() {
            self.fail(format!("at most {} joins", QUALS.len() - 1));
            return self;
        }
        let Some((la, lc)) = self.resolve(left) else { return self };
        let Some(rc) = column_for_field::<U>(right_field) else {
            self.fail(format!("unknown field '{right_field}' of table '{}'", U::table_name()));
            return self;
        };
        self.joins.push(Join { kind, table: U::table_name(), on: (la, lc, rc.name) });
        self
    }

    /// `INNER JOIN` entity `U` on `left = U.right_field`; `left` belongs to
    /// `T` or to an entity joined before.
    pub fn join<U: Entity>(self, left: Col, right_field: &'static str) -> Self {
        self.add_join::<U>(JoinKind::Inner, left, right_field)
    }

    /// `LEFT JOIN` entity `U` on `left = U.right_field`: rows of `T` without
    /// a match are kept (`U`'s columns NULL).
    pub fn left_join<U: Entity>(self, left: Col, right_field: &'static str) -> Self {
        self.add_join::<U>(JoinKind::Left, left, right_field)
    }

    /// Keep rows matching `spec` over `T`'s fields (`WHERE`; several filters
    /// are ANDed).
    pub fn filter(mut self, spec: Spec) -> Self {
        let mut frags = Vec::new();
        spec_frags::<T>(&spec, QUALS[0], &mut frags, &mut self.error);
        self.filters.push(frags);
        self
    }

    /// Keep rows matching `spec` over the fields of joined entity `U`.
    pub fn filter_on<U: Entity>(mut self, spec: Spec) -> Self {
        let Some(alias) = self.alias_of(U::table_name()) else { return self };
        let mut frags = Vec::new();
        spec_frags::<U>(&spec, QUALS[alias], &mut frags, &mut self.error);
        self.filters.push(frags);
        self
    }

    /// Group by a column; it is returned under its field name.
    pub fn group_by(self, c: Col) -> Self {
        let name = c.field.to_string();
        self.group_by_as(c, &name)
    }

    /// Group by a column, returned under `alias`.
    pub fn group_by_as(mut self, c: Col, alias: &str) -> Self {
        if let Some((a, column)) = self.resolve(c) {
            // Groups come first in the output, before the aggregates.
            self.outputs.insert(self.group_count, (alias.to_string(), Output::Group(a, column)));
            self.group_count += 1;
        }
        self
    }

    /// Compute `agg`, returned as column `alias`. Without [`group_by`](Self::group_by)
    /// the whole selection is one group.
    pub fn aggregate(mut self, alias: &str, agg: Agg) -> Self {
        let arg = match agg {
            Agg::Count => None,
            Agg::CountOf(c) | Agg::CountDistinct(c) | Agg::Sum(c) | Agg::Avg(c) | Agg::Min(c) | Agg::Max(c) => Some(c),
        };
        if let Some(c) = arg {
            if self.resolve(c).is_none() {
                return self;
            }
        }
        self.outputs.push((alias.to_string(), Output::Measure(agg)));
        self
    }

    /// Keep groups whose aggregate `alias` compares to `value` (`HAVING`).
    pub fn having(mut self, alias: &str, cmp: Cmp, value: impl Into<Param>) -> Self {
        if !self.outputs.iter().any(|(a, o)| a == alias && matches!(o, Output::Measure(_))) {
            self.fail(format!("having: no aggregate named '{alias}'"));
        }
        self.having.push((alias.to_string(), cmp, value.into()));
        self
    }

    /// Sort by an output column (a group or an aggregate alias).
    pub fn order_by(mut self, alias: &str, ascending: bool) -> Self {
        self.order.push((OrderKey::Output(alias.to_string()), ascending));
        self
    }

    /// Sort by any column of the query's entities.
    pub fn order_by_col(mut self, c: Col, ascending: bool) -> Self {
        if let Some((a, column)) = self.resolve(c) {
            self.order.push((OrderKey::Column(a, column), ascending));
        }
        self
    }

    /// Drop duplicate rows (`SELECT DISTINCT`).
    pub fn distinct(mut self) -> Self {
        self.distinct = true;
        self
    }

    /// Return at most `n` rows.
    pub fn limit(mut self, n: u64) -> Self {
        self.limit = Some(n);
        self
    }

    /// Skip the first `n` rows.
    pub fn offset(mut self, n: u64) -> Self {
        self.offset = n;
        self
    }

    fn aggregated(&self) -> bool {
        !self.outputs.is_empty()
    }

    fn push_col(q: &mut Sql, alias: usize, column: &str) {
        q.push(QUALS[alias]).ident(column);
    }

    fn push_agg(&self, q: &mut Sql, agg: Agg) {
        // Columns were resolved when the aggregate was added.
        let target = |c: Col| {
            let alias = if c.table == T::table_name() {
                0
            } else {
                self.joins.iter().position(|j| j.table == c.table).map_or(0, |i| i + 1)
            };
            (alias, c.column.unwrap_or("?"))
        };
        let (func, c) = match agg {
            Agg::Count => {
                q.push("COUNT(*)");
                return;
            }
            Agg::CountOf(c) => ("COUNT(", c),
            Agg::CountDistinct(c) => ("COUNT(DISTINCT ", c),
            Agg::Sum(c) => ("SUM(", c),
            Agg::Avg(c) => ("AVG(", c),
            Agg::Min(c) => ("MIN(", c),
            Agg::Max(c) => ("MAX(", c),
        };
        let (a, column) = target(c);
        q.push(func);
        Self::push_col(q, a, column);
        q.push(")");
    }

    /// `FROM … JOIN … WHERE …`
    fn push_from_where(&self, q: &mut Sql) {
        q.push(" FROM ").ident(T::table_name()).push(" t0");
        for (i, j) in self.joins.iter().enumerate() {
            q.push(if j.kind == JoinKind::Inner { " INNER JOIN " } else { " LEFT JOIN " });
            q.ident(j.table).push(" ").push(&QUALS[i + 1][..2]).push(" ON ");
            Self::push_col(q, j.on.0, j.on.1);
            q.push(" = ");
            Self::push_col(q, i + 1, j.on.2);
        }
        for (i, f) in self.filters.iter().enumerate() {
            q.push(if i == 0 { " WHERE " } else { " AND " });
            push_frags(q, f);
        }
    }

    /// `GROUP BY … HAVING …`
    fn push_group_having(&self, q: &mut Sql) {
        let groups: Vec<_> = self
            .outputs
            .iter()
            .filter_map(|(_, o)| match o {
                Output::Group(a, c) => Some((*a, *c)),
                Output::Measure(_) => None,
            })
            .collect();
        for (i, (a, c)) in groups.iter().enumerate() {
            q.push(if i == 0 { " GROUP BY " } else { ", " });
            Self::push_col(q, *a, c);
        }
        for (i, (alias, cmp, value)) in self.having.iter().enumerate() {
            q.push(if i == 0 { " HAVING " } else { " AND " });
            if let Some((_, Output::Measure(agg))) = self.outputs.iter().find(|(a, _)| a == alias) {
                self.push_agg(q, *agg);
            }
            q.push(cmp.sql()).param(value.clone());
        }
    }

    fn push_order_window(&self, q: &mut Sql, d: &dyn SqlDialect) {
        for (i, (key, asc)) in self.order.iter().enumerate() {
            q.push(if i == 0 { " ORDER BY " } else { ", " });
            match key {
                OrderKey::Output(alias) => {
                    q.ident(alias);
                }
                OrderKey::Column(a, c) => Self::push_col(q, *a, c),
            }
            q.push(if *asc { " ASC" } else { " DESC" });
        }
        q.push(&d.window(self.limit, self.offset, !self.order.is_empty()));
    }

    /// The statement for the select list `cols` (`None` = the outputs).
    fn build(&self, entity_cols: &[(usize, &'static str, String)]) -> Sql {
        let d = self.src.sql_dialect();
        let mut q = Sql::new(d);
        q.push(if self.distinct { "SELECT DISTINCT " } else { "SELECT " });
        if entity_cols.is_empty() {
            for (i, (alias, o)) in self.outputs.iter().enumerate() {
                if i > 0 {
                    q.push(", ");
                }
                match o {
                    Output::Group(a, c) => Self::push_col(&mut q, *a, c),
                    Output::Measure(agg) => self.push_agg(&mut q, *agg),
                }
                q.push(" AS ").ident(alias);
            }
        } else {
            for (i, (a, c, label)) in entity_cols.iter().enumerate() {
                if i > 0 {
                    q.push(", ");
                }
                Self::push_col(&mut q, *a, c);
                q.push(" AS ").ident(label);
            }
        }
        self.push_from_where(&mut q);
        self.push_group_having(&mut q);
        self.push_order_window(&mut q, d);
        q
    }

    fn check(&mut self) -> Result<(), OrmError> {
        if let Some(e) = self.error.take() {
            return Err(e);
        }
        for (key, _) in &self.order {
            if let OrderKey::Output(alias) = key {
                if !self.outputs.iter().any(|(a, _)| a == alias) {
                    return Err(OrmError::Query(format!("order_by: no output column named '{alias}'")));
                }
            }
        }
        Ok(())
    }

    /// The SQL text and parameters this query would run (for logs and tests).
    pub fn to_sql(&self) -> (String, Vec<Param>) {
        let q = if self.aggregated() { self.build(&[]) } else { self.build(&Self::entity_cols::<T>(0, "")) };
        (q.text, q.params)
    }

    fn entity_cols<E: Entity>(alias: usize, prefix: &str) -> Vec<(usize, &'static str, String)> {
        E::columns().iter().map(|c| (alias, c.name, format!("{prefix}{}", c.name))).collect()
    }

    /// Run the query and return its rows: the groups and aggregates when
    /// there are any, else `T`'s columns.
    pub async fn fetch_rows(mut self) -> Result<Rows, OrmError> {
        self.check()?;
        let (sql, params) = self.to_sql();
        self.src.fetch_rows(&sql, &params).await
    }

    /// Run the query and read each row into `R` (columns by name, see
    /// [`Rows::to_structs`]).
    pub async fn fetch_as<R: DeserializeOwned>(self) -> Result<Vec<R>, OrmError> {
        self.fetch_rows().await?.to_structs()
    }

    /// The matching rows of `T` (no aggregates).
    pub async fn fetch_all(mut self) -> Result<Vec<T>, OrmError>
    where
        T: DeserializeOwned,
    {
        self.check()?;
        if self.aggregated() {
            return Err(OrmError::Query("fetch_all on an aggregate query: use fetch_as or fetch_rows".into()));
        }
        let q = self.build(&Self::entity_cols::<T>(0, ""));
        self.src.fetch_rows(&q.text, &q.params).await?.into_entities()
    }

    /// Each matching row of `T` with its joined `U` — `None` where a
    /// [`left_join`](Self::left_join) found no match.
    pub async fn fetch_joined<U: Entity + DeserializeOwned>(mut self) -> Result<Vec<(T, Option<U>)>, OrmError>
    where
        T: DeserializeOwned,
    {
        let alias = self.alias_of(U::table_name());
        self.check()?;
        let alias = alias.ok_or_else(|| OrmError::Query(format!("'{}' is not joined", U::table_name())))?;
        if self.aggregated() {
            return Err(OrmError::Query("fetch_joined on an aggregate query: use fetch_as or fetch_rows".into()));
        }
        let mut cols = Self::entity_cols::<T>(0, "a_");
        let split = cols.len();
        cols.extend(Self::entity_cols::<U>(alias, "b_"));
        let q = self.build(&cols);
        let rows = self.src.fetch_rows(&q.text, &q.params).await?;
        let left_names: Vec<String> = T::columns().iter().map(|c| c.name.to_string()).collect();
        let right_names: Vec<String> = U::columns().iter().map(|c| c.name.to_string()).collect();
        let mut out = Vec::with_capacity(rows.rows.len());
        for mut row in rows.rows {
            let right = row.split_off(split.min(row.len()));
            let t = Rows { columns: left_names.clone(), rows: vec![row] }.into_entities::<T>()?.pop();
            let u = if right.iter().all(crate::Value::is_null) {
                None
            } else {
                Rows { columns: right_names.clone(), rows: vec![right] }.into_entities::<U>()?.pop()
            };
            if let Some(t) = t {
                out.push((t, u));
            }
        }
        Ok(out)
    }

    /// How many rows (or groups) the query yields, ignoring limit/offset.
    pub async fn fetch_count(mut self) -> Result<u64, OrmError> {
        self.check()?;
        self.order.clear();
        self.limit = None;
        self.offset = 0;
        let d = self.src.sql_dialect();
        let mut q = Sql::new(d);
        if self.aggregated() || self.distinct {
            let inner = self.to_sql();
            q.push("SELECT COUNT(*) FROM (").push(&inner.0).push(") q");
            q.params = inner.1;
        } else {
            q.push("SELECT COUNT(*)");
            self.push_from_where(&mut q);
        }
        let rows = self.src.fetch_rows(&q.text, &q.params).await?;
        let n: Vec<u64> = rows.to_structs()?;
        Ok(n.first().copied().unwrap_or(0))
    }
}
