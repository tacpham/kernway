use crate::{query::SqliteQueryBuilder, SqliteDialect};
use kernway_orm_core::{
    entity::{ColumnType, Entity},
    error::OrmError,
    query::QueryBuilder,
    repository::Repository,
    BoxFuture, ColumnDef, SqlDialect,
};
use rusqlite::{params_from_iter, types::Value as SqlValue, Connection};
use serde::{de::DeserializeOwned, Serialize};
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

pub(crate) fn map_err(e: rusqlite::Error) -> OrmError {
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
    SqliteDialect.create_table_sql(T::table_name(), T::columns())
}

pub(crate) fn row_to_entity<T: DeserializeOwned>(
    row: &rusqlite::Row,
    cols: &[ColumnDef],
) -> rusqlite::Result<T> {
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

fn number_to_integer(n: &serde_json::Number, ctx: &str) -> Result<i64, OrmError> {
    if let Some(i) = n.as_i64() {
        Ok(i)
    } else if let Some(u) = n.as_u64() {
        i64::try_from(u).map_err(|_| {
            OrmError::TypeConversion(format!(
                "{ctx} value {u} exceeds SQLite INTEGER range (i64)"
            ))
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
            } else if let Some(f) = n
                .as_f64()
                .filter(|_| n.as_i64().is_none() && n.as_u64().is_none())
            {
                SqlValue::Real(f)
            } else {
                SqlValue::Integer(number_to_integer(&n, &format!("column '{}'", col.name))?)
            }
        }
        serde_json::Value::String(s) => SqlValue::Text(s),
        other => SqlValue::Text(serde_json::to_string(&other).unwrap_or_default()),
    })
}

fn scalar_to_sql(v: &serde_json::Value) -> Result<SqlValue, OrmError> {
    Ok(match v {
        serde_json::Value::Number(n) => SqlValue::Integer(number_to_integer(n, "id")?),
        serde_json::Value::String(s) => SqlValue::Text(s.clone()),
        serde_json::Value::Bool(b) => SqlValue::Integer(*b as i64),
        serde_json::Value::Null => SqlValue::Null,
        other => {
            return Err(OrmError::TypeConversion(format!(
                "id must be a scalar (number/string/bool), got {other}"
            )))
        }
    })
}

fn id_to_sql<Id: Serialize>(id: &Id) -> Result<SqlValue, OrmError> {
    scalar_to_sql(&serde_json::to_value(id).map_err(map_serde)?)
}

/// An id's SQL values — one for a scalar key, several for a composite (tuple) key.
fn id_to_sql_values<Id: Serialize>(id: &Id) -> Result<Vec<SqlValue>, OrmError> {
    match serde_json::to_value(id).map_err(map_serde)? {
        serde_json::Value::Array(parts) => parts.iter().map(scalar_to_sql).collect(),
        scalar => Ok(vec![scalar_to_sql(&scalar)?]),
    }
}

fn pk_col_name<T: Entity>() -> &'static str {
    T::columns()
        .iter()
        .find(|c| c.primary_key)
        .map(|c| c.name)
        .unwrap_or("id")
}

/// The primary-key column names — one for a scalar key, several for a composite.
fn pk_cols<T: Entity>() -> Vec<&'static str> {
    let cols: Vec<&'static str> = T::columns()
        .iter()
        .filter(|c| c.primary_key)
        .map(|c| c.name)
        .collect();
    if cols.is_empty() {
        vec!["id"]
    } else {
        cols
    }
}

/// A `col = ?n [AND col2 = ?n+1 ...]` predicate over the PK columns, with
/// placeholders numbered from `start`.
fn pk_where<T: Entity>(start: usize) -> String {
    pk_cols::<T>()
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{} = ?{}", c, start + i))
        .collect::<Vec<_>>()
        .join(" AND ")
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
    Ok(matches!(
        v,
        serde_json::Value::Number(ref n)
            if n.as_u64() == Some(0) || n.as_i64() == Some(0)
    ))
}

pub(crate) fn column_for_field<T: Entity>(field: &str) -> Option<&'static ColumnDef> {
    T::columns()
        .iter()
        .find(|c| c.field == field || c.name == field)
}

pub(crate) fn filter_value_for_field<T: Entity>(field: &str, value: &str) -> SqlValue {
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
/// Uses `Arc<Mutex<Connection>>` so query builders and driver-created
/// repositories can share the same underlying SQLite connection.
pub struct SqliteRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    conn: Arc<Mutex<Connection>>,
    _marker: PhantomData<T>,
}

impl<T> SqliteRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    fn init(conn: Connection) -> Result<Self, OrmError> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
            .map_err(|e| OrmError::Connection(e.to_string()))?;
        let repo = Self {
            conn: Arc::new(Mutex::new(conn)),
            _marker: PhantomData,
        };
        repo.create_table()?;
        Ok(repo)
    }

    /// Create a repository from a shared SQLite connection.
    ///
    /// This ensures the entity table exists before returning. It panics only if
    /// schema initialisation fails after the connection has already been opened.
    pub fn from_conn(conn: Arc<Mutex<Connection>>) -> Self {
        let repo = Self {
            conn,
            _marker: PhantomData,
        };
        repo.create_table()
            .expect("failed to initialise SQLite repository table");
        repo
    }

    fn from_shared_conn(conn: Arc<Mutex<Connection>>) -> Self {
        Self {
            conn,
            _marker: PhantomData,
        }
    }

    /// Open (or create) a SQLite database file and ensure `T`'s table exists.
    pub fn open(path: &str) -> Result<Self, OrmError> {
        Connection::open(path)
            .map_err(|e| OrmError::Connection(e.to_string()))
            .and_then(Self::init)
    }

    /// Open a private in-memory database — the usual choice for tests.
    ///
    /// The database lives as long as this repository and is not shared with any
    /// other connection.
    pub fn in_memory() -> Result<Self, OrmError> {
        Connection::open_in_memory()
            .map_err(|e| OrmError::Connection(e.to_string()))
            .and_then(Self::init)
    }

    /// Open a private in-memory database.
    pub fn open_in_memory() -> Result<Self, OrmError> {
        Self::in_memory()
    }

    fn create_table(&self) -> Result<(), OrmError> {
        self.conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?
            .execute_batch(&create_table_sql::<T>())
            .map_err(|e| OrmError::Query(e.to_string()))
    }

    /// Run raw SQL directly against the connection.
    ///
    /// An escape hatch for schema tweaks and test fixtures. It bypasses the
    /// `Repository` abstraction entirely, so nothing here is portable to
    /// another backend.
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
            "UPDATE {} SET {} WHERE {}",
            T::table_name(),
            set_clause.join(", "),
            pk_where::<T>(set_cols.len() + 1)
        );
        let mut vals: Vec<SqlValue> = set_cols
            .iter()
            .map(|c| json_to_sql(obj.get(c.field).cloned().unwrap_or(serde_json::Value::Null), c))
            .collect::<Result<_, _>>()?;
        vals.extend(id_to_sql_values(&entity.id())?);

        self.conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?
            .execute(&sql, params_from_iter(vals.iter()))
            .map_err(map_err)?;
        Ok(entity.clone())
    }

    fn find_by_id_sync(&self, id: &T::Id) -> Result<Option<T>, OrmError> {
        let cols = T::columns();
        let col_list: Vec<&str> = cols.iter().map(|c| c.name).collect();
        let sql = format!(
            "SELECT {} FROM {} WHERE {}",
            col_list.join(", "),
            T::table_name(),
            pk_where::<T>(1)
        );

        let conn = self
            .conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?;
        let mut stmt = conn.prepare(&sql).map_err(map_err)?;
        let id_vals = id_to_sql_values(id)?;
        let mut rows = stmt.query(params_from_iter(id_vals.iter())).map_err(map_err)?;
        match rows.next().map_err(map_err)? {
            Some(row) => Ok(Some(row_to_entity::<T>(row, cols).map_err(map_err)?)),
            None => Ok(None),
        }
    }

    fn find_all_sync(&self) -> Result<Vec<T>, OrmError> {
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

    fn find_all_by_ids_sync(&self, ids: &[T::Id]) -> Result<Vec<T>, OrmError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        // A composite key has no simple `IN (...)` form — fall back to per-id
        // lookups (each is a multi-column WHERE).
        if pk_cols::<T>().len() > 1 {
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(e) = self.find_by_id_sync(id)? {
                    out.push(e);
                }
            }
            return Ok(out);
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

    fn count_sync(&self) -> Result<u64, OrmError> {
        let sql = format!("SELECT COUNT(*) FROM {}", T::table_name());
        let count: i64 = self
            .conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?
            .query_row(&sql, [], |row| row.get(0))
            .map_err(map_err)?;
        Ok(count as u64)
    }

    fn exists_by_id_sync(&self, id: &T::Id) -> Result<bool, OrmError> {
        Ok(self.find_by_id_sync(id)?.is_some())
    }

    fn save_sync(&self, entity: T) -> Result<T, OrmError> {
        if is_new_auto_entity(&entity)? {
            return self.do_insert(&entity);
        }

        if is_auto_pk::<T>() || self.exists_by_id_sync(&entity.id())? {
            self.do_update(&entity)
        } else {
            self.do_insert(&entity)
        }
    }

    fn save_all_sync(&self, entities: Vec<T>) -> Result<Vec<T>, OrmError> {
        entities.into_iter().map(|entity| self.save_sync(entity)).collect()
    }

    fn delete_by_id_sync(&self, id: &T::Id) -> Result<(), OrmError> {
        let sql = format!(
            "DELETE FROM {} WHERE {}",
            T::table_name(),
            pk_where::<T>(1)
        );
        let id_vals = id_to_sql_values(id)?;
        self.conn
            .lock()
            .map_err(|e| OrmError::Transaction(format!("mutex poisoned: {e}")))?
            .execute(&sql, params_from_iter(id_vals.iter()))
            .map_err(map_err)?;
        Ok(())
    }

    fn delete_all_by_ids_sync(&self, ids: &[T::Id]) -> Result<(), OrmError> {
        for id in ids {
            self.delete_by_id_sync(id)?;
        }
        Ok(())
    }
}

impl<T> Repository<T> for SqliteRepository<T>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    fn find_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        let id = id.clone();
        let conn = Arc::clone(&self.conn);
        Box::pin(async move {
            rt_core::spawn_blocking(move || Self::from_shared_conn(conn).find_by_id_sync(&id))
                .await
                .ok_or_else(|| OrmError::Connection("blocking task panicked".to_string()))?
        })
    }

    fn find_all<'a>(&'a self) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        let conn = Arc::clone(&self.conn);
        Box::pin(async move {
            rt_core::spawn_blocking(move || Self::from_shared_conn(conn).find_all_sync())
                .await
                .ok_or_else(|| OrmError::Connection("blocking task panicked".to_string()))?
        })
    }

    fn find_all_by_ids<'a>(&'a self, ids: &'a [T::Id]) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        let ids = ids.to_vec();
        let conn = Arc::clone(&self.conn);
        Box::pin(async move {
            rt_core::spawn_blocking(move || Self::from_shared_conn(conn).find_all_by_ids_sync(&ids))
                .await
                .ok_or_else(|| OrmError::Connection("blocking task panicked".to_string()))?
        })
    }

    fn count<'a>(&'a self) -> BoxFuture<'a, Result<u64, OrmError>> {
        let conn = Arc::clone(&self.conn);
        Box::pin(async move {
            rt_core::spawn_blocking(move || Self::from_shared_conn(conn).count_sync())
                .await
                .ok_or_else(|| OrmError::Connection("blocking task panicked".to_string()))?
        })
    }

    fn exists_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<bool, OrmError>> {
        let id = id.clone();
        let conn = Arc::clone(&self.conn);
        Box::pin(async move {
            rt_core::spawn_blocking(move || Self::from_shared_conn(conn).exists_by_id_sync(&id))
                .await
                .ok_or_else(|| OrmError::Connection("blocking task panicked".to_string()))?
        })
    }

    fn save<'a>(&'a self, entity: T) -> BoxFuture<'a, Result<T, OrmError>> {
        let conn = Arc::clone(&self.conn);
        Box::pin(async move {
            rt_core::spawn_blocking(move || Self::from_shared_conn(conn).save_sync(entity))
                .await
                .ok_or_else(|| OrmError::Connection("blocking task panicked".to_string()))?
        })
    }

    fn save_all<'a>(&'a self, entities: Vec<T>) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        let conn = Arc::clone(&self.conn);
        Box::pin(async move {
            rt_core::spawn_blocking(move || Self::from_shared_conn(conn).save_all_sync(entities))
                .await
                .ok_or_else(|| OrmError::Connection("blocking task panicked".to_string()))?
        })
    }

    fn delete_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>> {
        let id = id.clone();
        let conn = Arc::clone(&self.conn);
        Box::pin(async move {
            rt_core::spawn_blocking(move || Self::from_shared_conn(conn).delete_by_id_sync(&id))
                .await
                .ok_or_else(|| OrmError::Connection("blocking task panicked".to_string()))?
        })
    }

    fn delete_all_by_ids<'a>(&'a self, ids: &'a [T::Id]) -> BoxFuture<'a, Result<(), OrmError>> {
        let ids = ids.to_vec();
        let conn = Arc::clone(&self.conn);
        Box::pin(async move {
            rt_core::spawn_blocking(move || Self::from_shared_conn(conn).delete_all_by_ids_sync(&ids))
                .await
                .ok_or_else(|| OrmError::Connection("blocking task panicked".to_string()))?
        })
    }

    fn query(&self) -> Box<dyn QueryBuilder<T>> {
        Box::new(SqliteQueryBuilder::<T>::new(Arc::clone(&self.conn)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernway_orm_core::repository::Repository;
    use kernway_orm_macro::entity;
    use serde::{Deserialize, Serialize};
    use std::future::Future;

    fn block_on<F: Future>(future: F) -> F::Output {
        rt_core::Executor::new().unwrap().block_on(future).unwrap()
    }

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
        let saved = block_on(repo.save(sample("alpha", 10))).unwrap();
        let found = block_on(repo.find_by_id(&saved.id)).unwrap().unwrap();

        assert_eq!(found, saved);
    }

    #[test]
    fn test_auto_increment_id() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        let first = block_on(repo.save(sample("a", 1))).unwrap();
        let second = block_on(repo.save(sample("b", 2))).unwrap();

        assert_eq!(first.id, 1);
        assert_eq!(second.id, 2);
    }

    #[test]
    fn test_update() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        let saved = block_on(repo.save(sample("alpha", 10))).unwrap();
        let updated = block_on(repo.save(Item {
            id: saved.id,
            name: "alpha-updated".to_string(),
            value: 99,
        }))
        .unwrap();

        let found = block_on(repo.find_by_id(&saved.id)).unwrap().unwrap();
        assert_eq!(updated.name, "alpha-updated");
        assert_eq!(found.value, 99);
    }

    #[test]
    fn test_delete() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        let saved = block_on(repo.save(sample("alpha", 10))).unwrap();

        block_on(repo.delete_by_id(&saved.id)).unwrap();

        assert!(block_on(repo.find_by_id(&saved.id)).unwrap().is_none());
    }

    #[test]
    fn test_count() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        block_on(repo.save(sample("a", 1))).unwrap();
        block_on(repo.save(sample("b", 2))).unwrap();

        assert_eq!(block_on(repo.count()).unwrap(), 2);
    }

    #[test]
    fn test_find_all() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        block_on(repo.save(sample("b", 2))).unwrap();
        block_on(repo.save(sample("a", 1))).unwrap();

        let mut items = block_on(repo.find_all()).unwrap();
        items.sort_by(|left, right| left.name.cmp(&right.name));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].name, "a");
        assert_eq!(items[1].name, "b");
    }

    #[test]
    fn test_query_filter_eq() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        block_on(repo.save(sample("alpha", 10))).unwrap();
        block_on(repo.save(sample("beta", 20))).unwrap();

        let items = block_on(repo.query().filter_eq("name", "alpha").fetch_all()).unwrap();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "alpha");
    }

    #[test]
    fn test_query_order_by() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        block_on(repo.save(sample("c", 3))).unwrap();
        block_on(repo.save(sample("a", 1))).unwrap();
        block_on(repo.save(sample("b", 2))).unwrap();

        let items = block_on(repo.query().order_by_asc("name").fetch_all()).unwrap();
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_query_fetch_page() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        for (name, value) in [("a", 1), ("b", 2), ("c", 3), ("d", 4), ("e", 5)] {
            block_on(repo.save(sample(name, value))).unwrap();
        }

        let page = block_on(repo.query().order_by_asc("id").fetch_page(1, 2)).unwrap();

        assert_eq!(page.total, 5);
        assert_eq!(page.total_pages, 3);
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0].id, 3);
        assert_eq!(page.items[1].id, 4);
    }

    #[test]
    fn test_query_rejects_unknown_fields() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        block_on(repo.save(sample("alpha", 10))).unwrap();

        let err = block_on(
            repo.query()
                .filter_eq("name); DROP TABLE items; --", "alpha")
                .fetch_all(),
        );
        assert!(err.is_err());
    }

    #[test]
    fn test_order_by_rejects_unknown_fields() {
        let repo = SqliteRepository::<Item>::in_memory().unwrap();
        block_on(repo.save(sample("alpha", 10))).unwrap();
        assert!(block_on(
            repo.query()
                .order_by_asc("nope; DROP TABLE items")
                .fetch_all()
        )
        .is_err());
    }

    /// Composite key `(warehouse, sku)` — two `#[id]` fields → `Id = (String, String)`,
    /// mapped to a table-level `PRIMARY KEY (warehouse, sku)` and multi-column WHERE.
    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[entity(table = "stock")]
    struct Stock {
        #[id()]
        warehouse: String,
        #[id()]
        sku: String,
        quantity: i32,
    }

    fn key(w: &str, s: &str) -> (String, String) {
        (w.to_string(), s.to_string())
    }

    #[test]
    fn test_composite_primary_key() {
        let repo = SqliteRepository::<Stock>::in_memory().unwrap();

        // Same sku in two warehouses → two distinct composite keys.
        block_on(repo.save(Stock { warehouse: "WH1".into(), sku: "SKU42".into(), quantity: 100 })).unwrap();
        block_on(repo.save(Stock { warehouse: "WH2".into(), sku: "SKU42".into(), quantity: 5 })).unwrap();
        assert_eq!(block_on(repo.count()).unwrap(), 2);

        // find_by_id takes the tuple.
        assert_eq!(block_on(repo.find_by_id(&key("WH1", "SKU42"))).unwrap().unwrap().quantity, 100);
        assert_eq!(block_on(repo.find_by_id(&key("WH2", "SKU42"))).unwrap().unwrap().quantity, 5);
        assert!(block_on(repo.find_by_id(&key("WH3", "SKU42"))).unwrap().is_none());

        // Re-saving the same composite key updates in place (do_update path).
        block_on(repo.save(Stock { warehouse: "WH1".into(), sku: "SKU42".into(), quantity: 250 })).unwrap();
        assert_eq!(block_on(repo.count()).unwrap(), 2, "update, not a second insert");
        assert_eq!(block_on(repo.find_by_id(&key("WH1", "SKU42"))).unwrap().unwrap().quantity, 250);

        // Deleting one composite key leaves the other.
        block_on(repo.delete_by_id(&key("WH1", "SKU42"))).unwrap();
        assert!(block_on(repo.find_by_id(&key("WH1", "SKU42"))).unwrap().is_none());
        assert!(block_on(repo.exists_by_id(&key("WH2", "SKU42"))).unwrap());
    }
}
