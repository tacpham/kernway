//! SQLite implementation of kernway-orm-core's Repository<T> trait.
//!
//! Row marshaling uses serde_json as an intermediary layer:
//!   Entity → serde_json::Value → rusqlite params  (write)
//!   rusqlite row → serde_json::Value → Entity      (read)
//!
//! This avoids needing a custom FromRow derive and works with any
//! struct that implements Serialize + DeserializeOwned.
//!
//! # Example
//! ```rust,ignore
//! let repo = SqliteRepository::<Todo>::open("todos.db")?;
//! let todo = repo.save(Todo { id: 0, title: "Buy milk".into(), done: false })?;
//! println!("Saved with id {}", todo.id);
//! ```

use kernway_orm_core::{
    entity::{ColumnType, Entity},
    error::OrmError,
    page::Page,
    query::QueryBuilder,
    repository::Repository,
    ColumnDef,
};
use rusqlite::{params_from_iter, types::Value as SqlValue, Connection};
use serde::{de::DeserializeOwned, Serialize};
use std::sync::{Arc, Mutex};

fn map_err(e: rusqlite::Error) -> OrmError {
    match &e {
        rusqlite::Error::SqliteFailure(fe, msg) => {
            if let Some(message) = msg.as_deref() {
                if message.contains("FOREIGN KEY constraint failed") {
                    return OrmError::ForeignKeyViolation;
                }
            }
            if fe.code == rusqlite::ErrorCode::ConstraintViolation {
                let field = msg
                    .as_deref()
                    .and_then(|m| m.split('.').next_back())
                    .unwrap_or("unknown")
                    .to_string();
                return OrmError::UniqueViolation { field };
            }
            OrmError::Query(e.to_string())
        }
        rusqlite::Error::QueryReturnedNoRows => OrmError::NotFound,
        _ => OrmError::Query(e.to_string()),
    }
}

fn map_serde(e: serde_json::Error) -> OrmError {
    OrmError::TypeConversion(e.to_string())
}

fn create_table_sql<T: Entity>() -> String {
    let mut parts = Vec::new();
    for col in T::columns() {
        let sql_type = match col.col_type {
            ColumnType::BigInt | ColumnType::Integer | ColumnType::Boolean => "INTEGER",
            ColumnType::Float => "REAL",
            ColumnType::Text | ColumnType::Timestamp | ColumnType::Json | ColumnType::Unknown => {
                "TEXT"
            }
        };

        let mut def = format!("{} {}", col.name, sql_type);
        if col.primary_key {
            def.push_str(" PRIMARY KEY");
            if col.auto {
                def.push_str(" AUTOINCREMENT");
            }
        } else {
            if !col.nullable {
                def.push_str(" NOT NULL");
            }
            if col.unique {
                def.push_str(" UNIQUE");
            }
        }
        parts.push(def);
    }

    format!(
        "CREATE TABLE IF NOT EXISTS {} (\n  {}\n)",
        T::table_name(),
        parts.join(",\n  ")
    )
}

fn row_to_entity<T: DeserializeOwned>(row: &rusqlite::Row, cols: &[ColumnDef]) -> rusqlite::Result<T> {
    let mut map = serde_json::Map::new();
    for (i, col) in cols.iter().enumerate() {
        let v: SqlValue = row.get(i)?;
        map.insert(col.field.to_string(), sql_to_json(v, col));
    }

    serde_json::from_value(serde_json::Value::Object(map)).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn sql_to_json(v: SqlValue, col: &ColumnDef) -> serde_json::Value {
    match v {
        SqlValue::Null => serde_json::Value::Null,
        SqlValue::Integer(n) => {
            if col.col_type == ColumnType::Boolean {
                serde_json::Value::Bool(n != 0)
            } else {
                serde_json::json!(n)
            }
        }
        SqlValue::Real(f) => serde_json::json!(f),
        SqlValue::Text(s) => {
            if col.col_type == ColumnType::Json {
                serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s))
            } else {
                serde_json::Value::String(s)
            }
        }
        SqlValue::Blob(bytes) => serde_json::Value::String(format!("<blob:{}>", bytes.len())),
    }
}

/// Convert a JSON number to a SQLite INTEGER, erroring (never silently
/// coercing to 0 or wrapping to a negative) when the value can't be represented
/// as i64 — SQLite's only integer type. `ctx` names the source for the message.
fn number_to_integer(n: &serde_json::Number, ctx: &str) -> Result<i64, OrmError> {
    if let Some(i) = n.as_i64() {
        Ok(i)
    } else if let Some(u) = n.as_u64() {
        i64::try_from(u).map_err(|_| {
            OrmError::TypeConversion(format!("{ctx} value {u} exceeds SQLite INTEGER range (i64)"))
        })
    } else {
        Err(OrmError::TypeConversion(format!(
            "{ctx} is not an integer: {n}"
        )))
    }
}

fn json_to_sql(v: serde_json::Value, col: &ColumnDef) -> Result<SqlValue, OrmError> {
    Ok(match v {
        serde_json::Value::Null => SqlValue::Null,
        serde_json::Value::Bool(b) => SqlValue::Integer(b as i64),
        serde_json::Value::Number(n) => {
            if col.col_type == ColumnType::Float {
                SqlValue::Real(n.as_f64().ok_or_else(|| {
                    OrmError::TypeConversion(format!("column '{}' expects a float", col.name))
                })?)
            } else if let Some(f) = n.as_f64().filter(|_| n.as_i64().is_none() && n.as_u64().is_none()) {
                // Non-float column carrying a fractional number — store as REAL
                // rather than truncate silently.
                SqlValue::Real(f)
            } else {
                SqlValue::Integer(number_to_integer(&n, &format!("column '{}'", col.name))?)
            }
        }
        serde_json::Value::String(s) => SqlValue::Text(s),
        other => SqlValue::Text(serde_json::to_string(&other).unwrap_or_default()),
    })
}

fn id_to_sql<Id: Serialize>(id: &Id) -> Result<SqlValue, OrmError> {
    let v = serde_json::to_value(id).map_err(map_serde)?;
    Ok(match v {
        serde_json::Value::Number(n) => SqlValue::Integer(number_to_integer(&n, "id")?),
        serde_json::Value::String(s) => SqlValue::Text(s),
        serde_json::Value::Bool(b) => SqlValue::Integer(b as i64),
        serde_json::Value::Null => SqlValue::Null,
        other => {
            return Err(OrmError::TypeConversion(format!(
                "id must be a scalar (number/string/bool), got {other}"
            )))
        }
    })
}

fn pk_col_name<T: Entity>() -> &'static str {
    T::columns()
        .iter()
        .find(|c| c.primary_key)
        .map(|c| c.name)
        .unwrap_or("id")
}

fn is_auto_pk<T: Entity>() -> bool {
    T::columns().iter().any(|c| c.primary_key && c.auto)
}

fn is_new_auto_entity<T>(entity: &T) -> Result<bool, OrmError>
where
    T: Entity + Serialize,
    T::Id: Serialize,
{
    if !is_auto_pk::<T>() {
        return Ok(false);
    }

    let v = serde_json::to_value(entity.id()).map_err(map_serde)?;
    Ok(matches!(v, serde_json::Value::Number(ref n) if n.as_u64() == Some(0) || n.as_i64() == Some(0)))
}

fn column_for_field<T: Entity>(field: &str) -> Option<&'static ColumnDef> {
    T::columns().iter().find(|c| c.field == field || c.name == field)
}

fn filter_value_for_field<T: Entity>(field: &str, value: &str) -> SqlValue {
    let Some(col) = column_for_field::<T>(field) else {
        return SqlValue::Text(value.to_string());
    };

    match col.col_type {
        ColumnType::Boolean => match value {
            "true" | "1" => SqlValue::Integer(1),
            "false" | "0" => SqlValue::Integer(0),
            _ => SqlValue::Text(value.to_string()),
        },
        ColumnType::Integer | ColumnType::BigInt => value
            .parse::<i64>()
            .map(SqlValue::Integer)
            .or_else(|_| value.parse::<u64>().map(|v| SqlValue::Integer(v as i64)))
            .unwrap_or_else(|_| SqlValue::Text(value.to_string())),
        ColumnType::Float => value
            .parse::<f64>()
            .map(SqlValue::Real)
            .unwrap_or_else(|_| SqlValue::Text(value.to_string())),
        _ => SqlValue::Text(value.to_string()),
    }
}

/// Thread-safe SQLite repository.
///
/// Uses `Arc<Mutex<Connection>>` so `SqliteQueryBuilder` can share the connection.
pub struct SqliteRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    conn: Arc<Mutex<Connection>>,
    _marker: std::marker::PhantomData<T>,
}

// Send/Sync derive automatically from `Arc<Mutex<Connection>>` (Send+Sync) and
// hold exactly when `T` is Send/Sync. No `unsafe impl` — an explicit one would
// mask unsoundness if a non-thread-safe field were added later.

impl<T> SqliteRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned + Default + PartialEq,
{
    fn init(conn: Connection) -> Result<Self, OrmError> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| OrmError::Connection(e.to_string()))?;
        let repo = Self {
            conn: Arc::new(Mutex::new(conn)),
            _marker: std::marker::PhantomData,
        };
        repo.create_table()?;
        Ok(repo)
    }

    pub fn open(path: &str) -> Result<Self, OrmError> {
        Connection::open(path)
            .map_err(|e| OrmError::Connection(e.to_string()))
            .and_then(Self::init)
    }

    pub fn in_memory() -> Result<Self, OrmError> {
        Connection::open_in_memory()
            .map_err(|e| OrmError::Connection(e.to_string()))
            .and_then(Self::init)
    }

    fn create_table(&self) -> Result<(), OrmError> {
        self.conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?
            .execute_batch(&create_table_sql::<T>())
            .map_err(|e| OrmError::Query(e.to_string()))
    }

    pub fn execute_raw(&self, sql: &str) -> Result<(), OrmError> {
        self.conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?
            .execute_batch(sql)
            .map_err(|e| OrmError::Query(e.to_string()))
    }

    fn do_insert(&self, entity: &T) -> Result<T, OrmError> {
        let cols = T::columns();
        let insert_cols: Vec<&ColumnDef> = cols.iter().filter(|c| !(c.primary_key && c.auto)).collect();
        let json = serde_json::to_value(entity).map_err(map_serde)?;
        let obj = json
            .as_object()
            .ok_or_else(|| OrmError::TypeConversion("entity must serialize as object".into()))?;

        let col_names: Vec<String> = insert_cols.iter().map(|c| c.name.to_string()).collect();
        let placeholders: Vec<String> = (1..=insert_cols.len()).map(|i| format!("?{}", i)).collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            T::table_name(),
            col_names.join(", "),
            placeholders.join(", ")
        );
        let vals: Vec<SqlValue> = insert_cols
            .iter()
            .map(|c| json_to_sql(obj.get(c.field).cloned().unwrap_or(serde_json::Value::Null), c))
            .collect::<Result<_, _>>()?;

        let conn = self
            .conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        conn.execute(&sql, params_from_iter(vals.iter())).map_err(map_err)?;
        let last_id = conn.last_insert_rowid();
        drop(conn);

        if is_auto_pk::<T>() {
            let pk = pk_col_name::<T>();
            let col_list: Vec<&str> = cols.iter().map(|c| c.name).collect();
            let select_sql = format!(
                "SELECT {} FROM {} WHERE {} = ?1",
                col_list.join(", "),
                T::table_name(),
                pk
            );
            let conn = self
                .conn
                .lock()
                .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
            let mut stmt = conn.prepare(&select_sql).map_err(map_err)?;
            let mut rows = stmt.query(rusqlite::params![last_id]).map_err(map_err)?;
            let row = rows.next().map_err(map_err)?.ok_or(OrmError::NotFound)?;
            row_to_entity::<T>(row, cols).map_err(map_err)
        } else {
            Ok(entity.clone())
        }
    }

    fn do_update(&self, entity: &T) -> Result<T, OrmError> {
        let cols = T::columns();
        let set_cols: Vec<&ColumnDef> = cols.iter().filter(|c| !c.primary_key).collect();
        let json = serde_json::to_value(entity).map_err(map_serde)?;
        let obj = json
            .as_object()
            .ok_or_else(|| OrmError::TypeConversion("entity must serialize as object".into()))?;

        let set_clause: Vec<String> = set_cols
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{} = ?{}", c.name, i + 1))
            .collect();
        let sql = format!(
            "UPDATE {} SET {} WHERE {} = ?{}",
            T::table_name(),
            set_clause.join(", "),
            pk_col_name::<T>(),
            set_cols.len() + 1
        );
        let mut vals: Vec<SqlValue> = set_cols
            .iter()
            .map(|c| json_to_sql(obj.get(c.field).cloned().unwrap_or(serde_json::Value::Null), c))
            .collect::<Result<_, _>>()?;
        vals.push(id_to_sql(entity.id())?);

        self.conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?
            .execute(&sql, params_from_iter(vals.iter()))
            .map_err(map_err)?;
        Ok(entity.clone())
    }
}

impl<T> Repository<T> for SqliteRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned + Default + PartialEq,
{
    fn find_by_id(&self, id: &T::Id) -> Result<Option<T>, OrmError> {
        let cols = T::columns();
        let col_list: Vec<&str> = cols.iter().map(|c| c.name).collect();
        let sql = format!(
            "SELECT {} FROM {} WHERE {} = ?1",
            col_list.join(", "),
            T::table_name(),
            pk_col_name::<T>()
        );

        let conn = self
            .conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        let mut stmt = conn.prepare(&sql).map_err(map_err)?;
        let mut rows = stmt.query(rusqlite::params![id_to_sql(id)?]).map_err(map_err)?;
        match rows.next().map_err(map_err)? {
            Some(row) => Ok(Some(row_to_entity::<T>(row, cols).map_err(map_err)?)),
            None => Ok(None),
        }
    }

    fn find_all(&self) -> Result<Vec<T>, OrmError> {
        let cols = T::columns();
        let col_list: Vec<&str> = cols.iter().map(|c| c.name).collect();
        let sql = format!("SELECT {} FROM {}", col_list.join(", "), T::table_name());

        let conn = self
            .conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        let mut stmt = conn.prepare(&sql).map_err(map_err)?;
        let rows = stmt.query_map([], |row| row_to_entity::<T>(row, cols)).map_err(map_err)?;
        rows.map(|row| row.map_err(map_err)).collect()
    }

    fn find_all_by_ids(&self, ids: &[T::Id]) -> Result<Vec<T>, OrmError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let cols = T::columns();
        let col_list: Vec<&str> = cols.iter().map(|c| c.name).collect();
        let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{}", i)).collect();
        let sql = format!(
            "SELECT {} FROM {} WHERE {} IN ({})",
            col_list.join(", "),
            T::table_name(),
            pk_col_name::<T>(),
            placeholders.join(", ")
        );
        let sql_ids: Vec<SqlValue> = ids.iter().map(id_to_sql).collect::<Result<_, _>>()?;

        let conn = self
            .conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        let mut stmt = conn.prepare(&sql).map_err(map_err)?;
        let rows = stmt
            .query_map(params_from_iter(sql_ids.iter()), |row| row_to_entity::<T>(row, cols))
            .map_err(map_err)?;
        rows.map(|row| row.map_err(map_err)).collect()
    }

    fn count(&self) -> Result<u64, OrmError> {
        let sql = format!("SELECT COUNT(*) FROM {}", T::table_name());
        let count: i64 = self
            .conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?
            .query_row(&sql, [], |row| row.get(0))
            .map_err(map_err)?;
        Ok(count as u64)
    }

    fn exists_by_id(&self, id: &T::Id) -> Result<bool, OrmError> {
        Ok(self.find_by_id(id)?.is_some())
    }

    fn save(&self, entity: T) -> Result<T, OrmError> {
        if is_new_auto_entity(&entity)? {
            return self.do_insert(&entity);
        }

        if is_auto_pk::<T>() || self.exists_by_id(entity.id())? {
            self.do_update(&entity)
        } else {
            self.do_insert(&entity)
        }
    }

    fn save_all(&self, entities: Vec<T>) -> Result<Vec<T>, OrmError> {
        entities.into_iter().map(|entity| self.save(entity)).collect()
    }

    fn delete_by_id(&self, id: &T::Id) -> Result<(), OrmError> {
        let sql = format!("DELETE FROM {} WHERE {} = ?1", T::table_name(), pk_col_name::<T>());
        self.conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?
            .execute(&sql, rusqlite::params![id_to_sql(id)?])
            .map_err(map_err)?;
        Ok(())
    }

    fn delete_all_by_ids(&self, ids: &[T::Id]) -> Result<(), OrmError> {
        for id in ids {
            self.delete_by_id(id)?;
        }
        Ok(())
    }

    fn query(&self) -> Box<dyn QueryBuilder<T>> {
        Box::new(SqliteQueryBuilder::<T>::new(Arc::clone(&self.conn)))
    }
}

// SECURITY: `col` is ALWAYS a `&'static str` taken from `T::columns()` metadata —
// never a caller-supplied string. This makes SQL identifier injection structurally
// impossible: an unknown field is rejected (recorded as an error) instead of being
// interpolated raw. Values still go through `?N` bind parameters.
#[derive(Clone)]
enum Filter {
    Eq { col: &'static str, val: SqlValue },
    Ne { col: &'static str, val: SqlValue },
    Gt { col: &'static str, val: SqlValue },
    Lt { col: &'static str, val: SqlValue },
    Like { col: &'static str, pat: String },
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

pub struct SqliteQueryBuilder<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    conn: Arc<Mutex<Connection>>,
    filters: Vec<Filter>,
    order: Vec<Sort>,
    lim: Option<u64>,
    off: u64,
    /// First unknown-field error encountered while building the query, surfaced
    /// at the terminal operation (fetch_*). Keeps the builder API infallible
    /// while still rejecting invalid identifiers.
    error: Option<OrmError>,
    _marker: std::marker::PhantomData<T>,
}

// Send/Sync are derived automatically: `Arc<Mutex<Connection>>` is Send+Sync
// (Connection is Send) and the builder is only Send/Sync when `T` is. No
// `unsafe impl` — a hand-written one would silently become unsound if a
// non-Send/Sync field were ever added.

impl<T> SqliteQueryBuilder<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self {
            conn,
            filters: Vec::new(),
            order: Vec::new(),
            lim: None,
            off: 0,
            error: None,
            _marker: std::marker::PhantomData,
        }
    }

    /// Resolve a caller-supplied field name to a validated `&'static str` column
    /// name from entity metadata. Unknown fields are rejected (recorded in
    /// `self.error`) instead of being interpolated into SQL. See the `Filter`
    /// security note.
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
            let idx = params.len() + 1;
            match filter {
                Filter::Eq { col, val } => {
                    parts.push(format!("{} = ?{}", col, idx));
                    params.push(val.clone());
                }
                Filter::Ne { col, val } => {
                    parts.push(format!("{} != ?{}", col, idx));
                    params.push(val.clone());
                }
                Filter::Gt { col, val } => {
                    parts.push(format!("{} > ?{}", col, idx));
                    params.push(val.clone());
                }
                Filter::Lt { col, val } => {
                    parts.push(format!("{} < ?{}", col, idx));
                    params.push(val.clone());
                }
                Filter::Like { col, pat } => {
                    parts.push(format!("{} LIKE ?{}", col, idx));
                    params.push(SqlValue::Text(pat.clone()));
                }
            }
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
        let mut sql = format!("SELECT {} FROM {}{}", col_list.join(", "), T::table_name(), where_clause);

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
        (format!("SELECT COUNT(*) FROM {}{}", T::table_name(), where_clause), params)
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

    fn filter_like(mut self: Box<Self>, field: &'static str, pattern: &str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            self.filters.push(Filter::Like {
                col,
                pat: pattern.to_string(),
            });
        }
        self
    }

    fn order_by_asc(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            self.order.push(Sort { col, dir: SortDir::Asc });
        }
        self
    }

    fn order_by_desc(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        if let Some(col) = self.resolve_col(field) {
            self.order.push(Sort { col, dir: SortDir::Desc });
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

    fn fetch_all(mut self: Box<Self>) -> Result<Vec<T>, OrmError> {
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

    fn fetch_one(mut self: Box<Self>) -> Result<Option<T>, OrmError> {
        self.lim = Some(1);
        let mut items = self.fetch_all()?;
        Ok(items.pop())
    }

    fn fetch_count(mut self: Box<Self>) -> Result<u64, OrmError> {
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

    fn fetch_page(mut self: Box<Self>, page: u64, size: u64) -> Result<Page<T>, OrmError> {
        if let Some(err) = self.error.take() {
            return Err(err);
        }
        let (count_sql, count_params) = self.build_count_query();
        let total: i64 = self
            .conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?
            .query_row(&count_sql, params_from_iter(count_params.iter()), |row| row.get(0))
            .map_err(map_err)?;

        if size == 0 {
            return Ok(Page::new(Vec::new(), total as u64, page, size));
        }

        self.off = page.saturating_mul(size);
        self.lim = Some(size);
        let items = self.fetch_all()?;
        Ok(Page::new(items, total as u64, page, size))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernway_orm_core::repository::Repository;
    use kernway_orm_macro::entity;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[entity(table = "items")]
    struct Item {
        #[id(strategy = "auto")]
        id: u64,
        name: String,
        value: i32,
    }

    fn sample(name: &str, value: i32) -> Item {
        Item {
            id: 0,
            name: name.to_string(),
            value,
        }
    }

    #[test]
    fn test_in_memory_insert_and_find() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        let saved = repo.save(sample("alpha", 10)).unwrap();
        let found = repo.find_by_id(&saved.id).unwrap().unwrap();

        assert_eq!(found, saved);
    }

    #[test]
    fn test_auto_increment_id() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        let first = repo.save(sample("a", 1)).unwrap();
        let second = repo.save(sample("b", 2)).unwrap();

        assert_eq!(first.id, 1);
        assert_eq!(second.id, 2);
    }

    #[test]
    fn test_update() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        let saved = repo.save(sample("alpha", 10)).unwrap();
        let updated = repo
            .save(Item {
                id: saved.id,
                name: "alpha-updated".to_string(),
                value: 99,
            })
            .unwrap();

        let found = repo.find_by_id(&saved.id).unwrap().unwrap();
        assert_eq!(updated.name, "alpha-updated");
        assert_eq!(found.value, 99);
    }

    #[test]
    fn test_delete() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        let saved = repo.save(sample("alpha", 10)).unwrap();

        repo.delete_by_id(&saved.id).unwrap();

        assert!(repo.find_by_id(&saved.id).unwrap().is_none());
    }

    #[test]
    fn test_count() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        repo.save(sample("a", 1)).unwrap();
        repo.save(sample("b", 2)).unwrap();

        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn test_find_all() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        repo.save(sample("b", 2)).unwrap();
        repo.save(sample("a", 1)).unwrap();

        let mut items = repo.find_all().unwrap();
        items.sort_by(|left, right| left.name.cmp(&right.name));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "a");
        assert_eq!(items[1].name, "b");
    }

    #[test]
    fn test_query_filter_eq() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        repo.save(sample("alpha", 10)).unwrap();
        repo.save(sample("beta", 20)).unwrap();

        let items = repo.query().filter_eq("name", "alpha").fetch_all().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "alpha");
    }

    #[test]
    fn test_query_order_by() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        repo.save(sample("c", 3)).unwrap();
        repo.save(sample("a", 1)).unwrap();
        repo.save(sample("b", 2)).unwrap();

        let items = repo.query().order_by_asc("name").fetch_all().unwrap();
        let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_query_fetch_page() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        for (name, value) in [("a", 1), ("b", 2), ("c", 3), ("d", 4), ("e", 5)] {
            repo.save(sample(name, value)).unwrap();
        }

        let page = repo.query().order_by_asc("id").fetch_page(1, 2).unwrap();
        assert_eq!(page.total, 5);
        assert_eq!(page.total_pages, 3);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].id, 3);
        assert_eq!(page.items[1].id, 4);
    }

    // --- M1.0 correctness regressions ------------------------------------

    #[test]
    fn unknown_filter_field_is_rejected_not_injected() {
        // An unrecognised field name must surface an error at the terminal op,
        // never be interpolated into SQL (identifier-injection guard).
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        repo.save(sample("alpha", 10)).unwrap();

        let result = repo
            .query()
            .filter_eq("name = 'x' OR 1=1 --", "irrelevant")
            .fetch_all();
        assert!(result.is_err(), "unknown field must error, got {result:?}");
    }

    #[test]
    fn unknown_order_field_is_rejected() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        repo.save(sample("alpha", 10)).unwrap();
        assert!(repo.query().order_by_asc("nope; DROP TABLE items").fetch_all().is_err());
    }

    #[test]
    fn u64_id_exceeding_i64_errors_not_wraps() {
        // number_to_integer must reject u64 values that don't fit i64 instead of
        // silently wrapping to a negative rowid.
        let big = serde_json::Number::from(u64::MAX);
        assert!(number_to_integer(&big, "id").is_err());
        let ok = serde_json::Number::from(42u64);
        assert_eq!(number_to_integer(&ok, "id").unwrap(), 42);
    }
}
