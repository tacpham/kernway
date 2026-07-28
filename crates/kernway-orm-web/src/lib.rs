//! Turn an HTTP query string into a **validated** ORM query and a paginated
//! response — the "list endpoint" plumbing every web framework ships.
//!
//! A client asks for a filtered/sorted/paged slice through query params; this
//! crate parses them (two accepted syntaxes), **validates every field against a
//! whitelist** (so a caller can't filter or sort by an arbitrary column), builds
//! a [`Spec`] + ordering + page window, and runs it through any
//! [`Repository`] to a [`Page<T>`] envelope.
//!
//! ## Two syntaxes (both supported)
//!
//! - **JSON:API** — `?filter[role]=ADMIN&filter[age][gt]=18&sort=-age&page[number]=0&page[size]=20`
//! - **Compact DSL** — `?filter=role:eq:ADMIN,age:gt:18&sort=-age&page=0&size=20`
//!
//! Operators: `eq` (the default), `ne`, `gt`, `lt`, `gte`, `lte`, `like`, `in`
//! (comma-separated values). Filter terms combine with `AND`; for `OR` build a
//! [`Spec`] directly. A leading `-` on a sort field means descending.
//!
//! ## Whitelist (why validation matters)
//!
//! [`QueryConfig`] lists the fields that may be filtered and sorted. It doubles
//! as the fix for a Rust detail: [`QueryBuilder`] takes `&'static str` field
//! names, so a runtime field from the URL is resolved *through* the whitelist to
//! the `&'static str` it names — an unknown field is a validation error, never a
//! query.
//!
//! ```no_run
//! use kernway_orm_web::{QueryConfig, QuerySpec};
//! # use kernway_core::fields::QueryParams;
//! const CFG: QueryConfig = QueryConfig {
//!     filterable: &["role", "age"],
//!     sortable: &["age", "name"],
//!     default_size: 20,
//!     max_size: 100,
//! };
//! # fn handle(params: &QueryParams) -> Result<(), kernway_orm_web::QueryError> {
//! let qs = QuerySpec::parse(params, &CFG)?;
//! // let page = qs.fetch_page(&*repo).await?;   // -> Page<User>
//! # let _ = qs; Ok(())
//! # }
//! ```

use kernway_core::fields::QueryParams;
use kernway_orm_core::{
    entity::Entity, error::OrmError, page::Page, query::QueryBuilder, repository::Repository,
    spec::Spec, BoxFuture,
};

/// A sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    /// Ascending.
    Asc,
    /// Descending (a `-` prefix in the URL).
    Desc,
}

/// What a caller is allowed to filter and sort by, plus page-size bounds.
///
/// Designed to be a `const` so an endpoint declares it once.
pub struct QueryConfig {
    /// Fields permitted in `filter[...]`.
    pub filterable: &'static [&'static str],
    /// Fields permitted in `sort`.
    pub sortable: &'static [&'static str],
    /// Page size when the request does not ask for one.
    pub default_size: u64,
    /// The largest page size a request may ask for.
    pub max_size: u64,
}

impl QueryConfig {
    fn filter_field(&self, name: &str) -> Result<&'static str, QueryError> {
        self.filterable
            .iter()
            .copied()
            .find(|f| *f == name)
            .ok_or_else(|| QueryError::UnknownFilterField(name.to_string()))
    }

    fn sort_field(&self, name: &str) -> Result<&'static str, QueryError> {
        self.sortable
            .iter()
            .copied()
            .find(|f| *f == name)
            .ok_or_else(|| QueryError::UnknownSortField(name.to_string()))
    }
}

/// Why a query string was rejected. Maps naturally onto a `400 Bad Request`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// A `filter[...]` named a field not in the whitelist.
    UnknownFilterField(String),
    /// `sort` named a field not in the whitelist.
    UnknownSortField(String),
    /// An unrecognised operator (not eq/ne/gt/lt/gte/lte/like/in).
    UnknownOperator(String),
    /// A filter term was malformed (e.g. missing a value).
    MalformedFilter(String),
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QueryError::UnknownFilterField(s) => write!(f, "cannot filter by unknown field '{s}'"),
            QueryError::UnknownSortField(s) => write!(f, "cannot sort by unknown field '{s}'"),
            QueryError::UnknownOperator(s) => write!(f, "unknown filter operator '{s}'"),
            QueryError::MalformedFilter(s) => write!(f, "malformed filter term '{s}'"),
        }
    }
}

impl std::error::Error for QueryError {}

/// A parsed, validated query: an optional predicate, an ordering, and a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuerySpec {
    /// The combined filter predicate (`None` when no filters were given).
    pub spec: Option<Spec>,
    /// Sort fields in priority order.
    pub sort: Vec<(&'static str, SortDir)>,
    /// Page index, 0-based.
    pub page: u64,
    /// Page size.
    pub size: u64,
}

impl QuerySpec {
    /// Parse and validate query params against `config`. Accepts both the
    /// JSON:API and compact syntaxes.
    pub fn parse(params: &QueryParams, config: &QueryConfig) -> Result<Self, QueryError> {
        let mut conditions: Vec<Spec> = Vec::new();

        // Compact: filter=field:op:value,field2:value2
        if let Some(compact) = params.get("filter") {
            for term in compact.split(',').filter(|t| !t.is_empty()) {
                conditions.push(parse_compact_term(term, config)?);
            }
        }

        // JSON:API: filter[field] / filter[field][op]
        for (name, value) in params.iter() {
            if let Some(inner) = name.strip_prefix("filter[") {
                conditions.push(parse_jsonapi_filter(inner, value, config)?);
            }
        }

        // Sort (shared): sort=-age,name
        let mut sort = Vec::new();
        if let Some(s) = params.get("sort") {
            for token in s.split(',').filter(|t| !t.is_empty()) {
                let (dir, name) = match token.strip_prefix('-') {
                    Some(rest) => (SortDir::Desc, rest),
                    None => (SortDir::Asc, token),
                };
                sort.push((config.sort_field(name)?, dir));
            }
        }

        // Page: page[number]/page[size] (JSON:API) or page/size (compact).
        let page = params
            .get("page[number]")
            .or_else(|| params.get("page"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let size = params
            .get("page[size]")
            .or_else(|| params.get("size"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(config.default_size)
            .clamp(1, config.max_size);

        let spec = conditions.into_iter().reduce(|acc, c| acc.and(c));
        Ok(QuerySpec { spec, sort, page, size })
    }

    /// Apply this query's filter and ordering onto a query builder.
    pub fn apply<T: Entity>(&self, qb: Box<dyn QueryBuilder<T>>) -> Box<dyn QueryBuilder<T>> {
        let mut qb = qb;
        if let Some(spec) = &self.spec {
            qb = qb.filter_spec(spec.clone());
        }
        for (field, dir) in &self.sort {
            qb = match dir {
                SortDir::Asc => qb.order_by_asc(field),
                SortDir::Desc => qb.order_by_desc(field),
            };
        }
        qb
    }

    /// Run the query through `repo` and return the requested page.
    pub fn fetch_page<T>(&self, repo: &dyn Repository<T>) -> BoxFuture<'static, Result<Page<T>, OrmError>>
    where
        T: Entity,
    {
        self.apply(repo.query()).fetch_page(self.page, self.size)
    }
}

/// Parse a compact term `field[:op]:value` (op defaults to `eq`).
fn parse_compact_term(term: &str, config: &QueryConfig) -> Result<Spec, QueryError> {
    let parts: Vec<&str> = term.splitn(3, ':').collect();
    let (name, op, value) = match parts.as_slice() {
        [name, value] => (*name, "eq", *value),
        [name, op, value] => (*name, *op, *value),
        _ => return Err(QueryError::MalformedFilter(term.to_string())),
    };
    let field = config.filter_field(name)?;
    build_spec(field, op, value)
}

/// Parse a JSON:API filter key body `field]` or `field][op]` with its value.
fn parse_jsonapi_filter(inner: &str, value: &str, config: &QueryConfig) -> Result<Spec, QueryError> {
    let body = inner.trim_end_matches(']');
    let mut parts = body.split("][");
    let name = parts.next().unwrap_or("");
    let op = parts.next().unwrap_or("eq");
    let field = config.filter_field(name)?;
    build_spec(field, op, value)
}

/// Build one [`Spec`] leaf from a resolved field, operator, and value.
fn build_spec(field: &'static str, op: &str, value: &str) -> Result<Spec, QueryError> {
    Ok(match op {
        "eq" => Spec::eq(field, value),
        "ne" => Spec::ne(field, value),
        "gt" => Spec::gt(field, value),
        "lt" => Spec::lt(field, value),
        "gte" => Spec::gte(field, value),
        "lte" => Spec::lte(field, value),
        "like" => Spec::like(field, value),
        "in" => Spec::in_(field, value.split(',').map(|s| s.to_string())),
        other => return Err(QueryError::UnknownOperator(other.to_string())),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernway_orm_core::repository::Repository;
    use kernway_orm_macro::entity;
    use kernway_orm_memory::InMemoryRepository;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[entity(table = "users")]
    struct User {
        #[id(strategy = "auto")]
        id: u64,
        name: String,
        role: String,
        age: i64,
    }

    const CFG: QueryConfig = QueryConfig {
        filterable: &["role", "age"],
        sortable: &["age", "name"],
        default_size: 20,
        max_size: 100,
    };

    fn q(pairs: &[(&str, &str)]) -> QueryParams {
        let mut p = QueryParams::new();
        for (k, v) in pairs {
            p.append(k, v);
        }
        p
    }

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        rt_core::Executor::new().unwrap().block_on(f).unwrap()
    }

    fn seed() -> Box<dyn Repository<User>> {
        let repo: Box<dyn Repository<User>> = Box::new(InMemoryRepository::<User>::new());
        for (name, role, age) in [("a", "ADMIN", 30i64), ("b", "ADMIN", 15), ("c", "USER", 40), ("d", "ADMIN", 50)] {
            block_on(repo.save(User { id: 0, name: name.into(), role: role.into(), age })).unwrap();
        }
        repo
    }

    #[test]
    fn parses_jsonapi_syntax_and_runs() {
        let qs = QuerySpec::parse(
            &q(&[
                ("filter[role]", "ADMIN"),
                ("filter[age][gt]", "18"),
                ("sort", "-age"),
                ("page[number]", "0"),
                ("page[size]", "2"),
            ]),
            &CFG,
        )
        .unwrap();
        assert_eq!(qs.sort, vec![("age", SortDir::Desc)]);
        assert_eq!((qs.page, qs.size), (0, 2));

        let page = block_on(qs.fetch_page(&*seed())).unwrap();
        let names: Vec<&str> = page.items.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, vec!["d", "a"], "ADMIN & age>18, sorted -age");
        assert_eq!(page.total, 2);
    }

    #[test]
    fn parses_compact_syntax_and_runs() {
        let qs = QuerySpec::parse(
            &q(&[("filter", "role:eq:ADMIN,age:gt:18"), ("sort", "-age"), ("size", "10")]),
            &CFG,
        )
        .unwrap();
        let page = block_on(qs.fetch_page(&*seed())).unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.size, 10);
    }

    #[test]
    fn in_operator_and_default_size() {
        let qs = QuerySpec::parse(&q(&[("filter[role][in]", "ADMIN,USER")]), &CFG).unwrap();
        assert_eq!(qs.size, 20); // default
        assert_eq!(block_on(qs.fetch_page(&*seed())).unwrap().total, 4);
    }

    #[test]
    fn rejects_fields_outside_the_whitelist() {
        assert_eq!(
            QuerySpec::parse(&q(&[("filter[password]", "x")]), &CFG),
            Err(QueryError::UnknownFilterField("password".into()))
        );
        assert_eq!(
            QuerySpec::parse(&q(&[("sort", "ssn")]), &CFG),
            Err(QueryError::UnknownSortField("ssn".into()))
        );
        assert!(matches!(
            QuerySpec::parse(&q(&[("filter[age][xx]", "1")]), &CFG),
            Err(QueryError::UnknownOperator(_))
        ));
    }

    #[test]
    fn page_size_is_clamped_to_max() {
        let qs = QuerySpec::parse(&q(&[("page[size]", "9999")]), &CFG).unwrap();
        assert_eq!(qs.size, 100); // max_size
    }

    #[test]
    fn page_serialises_to_a_json_envelope() {
        let qs = QuerySpec::parse(&q(&[]), &CFG).unwrap();
        let page = block_on(qs.fetch_page(&*seed())).unwrap();
        let json = serde_json::to_value(&page).unwrap();
        assert!(json.get("items").is_some());
        assert_eq!(json["total"], 4);
        assert_eq!(json["size"], 20);
        assert_eq!(json["total_pages"], 1);
    }
}
