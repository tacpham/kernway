//! Asynchronous CRUD against a live [`Conn`], shared by the autocommit
//! repository and the transaction path.
//!
//! Parameterised statements use the **binary prepared-statement protocol**
//! (`conn.exec_prepared`), so values are bound server-side — no text escaping.
//! Statements with no parameters (DDL, `COUNT`, unfiltered `SELECT`) use the
//! plain text path. Everything runs directly on the per-core reactor.

use crate::protocol::{BindValue, Conn, QueryResult};
use crate::value::{
    id_to_binds, is_auto_pk, is_new_auto_entity, json_to_bind, pk_cols, pk_where_ph, row_to_entity,
};
use crate::MySqlDialect;
use kernway_orm_core::{entity::Entity, error::OrmError, ColumnDef, SqlDialect};
use serde::{de::DeserializeOwned, Serialize};

fn map_serde(e: serde_json::Error) -> OrmError {
    OrmError::TypeConversion(e.to_string())
}

fn col_list<T: Entity>() -> String {
    T::columns()
        .iter()
        .map(|c| format!("`{}`", c.name))
        .collect::<Vec<_>>()
        .join(", ")
}

fn pk_col_name<T: Entity>() -> &'static str {
    T::columns()
        .iter()
        .find(|c| c.primary_key)
        .map(|c| c.name)
        .unwrap_or("id")
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

fn rows_to_entities<T: Entity + DeserializeOwned>(result: QueryResult) -> Result<Vec<T>, OrmError> {
    match result {
        QueryResult::Rows { rows, .. } => rows.iter().map(|r| row_to_entity::<T>(r, T::columns())).collect(),
        QueryResult::Ok { .. } => Ok(Vec::new()),
    }
}

/// `CREATE TABLE IF NOT EXISTS` for `T` (no params → text path).
pub async fn ensure_table<T: Entity>(conn: &mut Conn) -> Result<(), OrmError> {
    let sql = MySqlDialect.create_table_sql(T::table_name(), T::columns());
    conn.query(&sql).await.map(|_| ())
}

/// Find by primary key, optionally taking a `FOR UPDATE` row lock.
pub async fn find_by_id<T>(conn: &mut Conn, id: &T::Id, lock: bool) -> Result<Option<T>, OrmError>
where
    T: Entity + DeserializeOwned,
    T::Id: Serialize,
{
    let binds = id_to_binds(id)?;
    let mut sql = format!(
        "SELECT {} FROM `{}` WHERE {}",
        col_list::<T>(),
        T::table_name(),
        pk_where_ph::<T>()
    );
    if lock {
        let suffix = MySqlDialect.lock_for_update_suffix();
        if !suffix.is_empty() {
            sql.push(' ');
            sql.push_str(suffix);
        }
    }
    Ok(rows_to_entities::<T>(conn.exec_prepared(&sql, &binds).await?)?
        .into_iter()
        .next())
}

/// Load every row (no params → text path).
pub async fn find_all<T>(conn: &mut Conn) -> Result<Vec<T>, OrmError>
where
    T: Entity + DeserializeOwned,
{
    let sql = format!("SELECT {} FROM `{}`", col_list::<T>(), T::table_name());
    rows_to_entities::<T>(conn.query(&sql).await?)
}

/// Total row count (no params → text path).
pub async fn count<T: Entity>(conn: &mut Conn) -> Result<u64, OrmError> {
    let sql = format!("SELECT COUNT(*) FROM `{}`", T::table_name());
    Ok(scalar_u64(conn.query(&sql).await?))
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

/// Existence check without materialising the entity.
pub async fn exists_by_id<T>(conn: &mut Conn, id: &T::Id) -> Result<bool, OrmError>
where
    T: Entity,
    T::Id: Serialize,
{
    let binds = id_to_binds(id)?;
    let sql = format!("SELECT 1 FROM `{}` WHERE {} LIMIT 1", T::table_name(), pk_where_ph::<T>());
    match conn.exec_prepared(&sql, &binds).await? {
        QueryResult::Rows { rows, .. } => Ok(!rows.is_empty()),
        QueryResult::Ok { .. } => Ok(false),
    }
}

/// Insert or update, returning the stored form (with any generated key).
pub async fn save<T>(conn: &mut Conn, entity: T) -> Result<T, OrmError>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize,
{
    if is_new_auto_entity(&entity)? {
        return do_insert(conn, &entity).await;
    }
    if is_auto_pk::<T>() || exists_by_id::<T>(conn, &entity.id()).await? {
        do_update(conn, &entity).await
    } else {
        do_insert(conn, &entity).await
    }
}

fn entity_object<T: Serialize>(entity: &T) -> Result<serde_json::Map<String, serde_json::Value>, OrmError> {
    serde_json::to_value(entity)
        .map_err(map_serde)?
        .as_object()
        .cloned()
        .ok_or_else(|| OrmError::TypeConversion("entity must serialize as a JSON object".into()))
}

fn bind_for<T: Entity>(obj: &serde_json::Map<String, serde_json::Value>, col: &ColumnDef) -> BindValue {
    json_to_bind(obj.get(col.field).unwrap_or(&serde_json::Value::Null), col)
}

async fn do_insert<T>(conn: &mut Conn, entity: &T) -> Result<T, OrmError>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize,
{
    let cols = T::columns();
    let insert_cols: Vec<&ColumnDef> = cols.iter().filter(|c| !(c.primary_key && c.auto)).collect();
    let obj = entity_object(entity)?;

    let names: Vec<String> = insert_cols.iter().map(|c| format!("`{}`", c.name)).collect();
    let binds: Vec<BindValue> = insert_cols.iter().map(|c| bind_for::<T>(&obj, c)).collect();
    let sql = format!(
        "INSERT INTO `{}` ({}) VALUES ({})",
        T::table_name(),
        names.join(", "),
        placeholders(insert_cols.len())
    );
    let result = conn.exec_prepared(&sql, &binds).await?;

    if is_auto_pk::<T>() {
        let last_id = match result {
            QueryResult::Ok { last_insert_id, .. } => last_insert_id,
            QueryResult::Rows { .. } => 0,
        };
        let sql = format!(
            "SELECT {} FROM `{}` WHERE `{}` = ?",
            col_list::<T>(),
            T::table_name(),
            pk_col_name::<T>()
        );
        rows_to_entities::<T>(conn.exec_prepared(&sql, &[BindValue::UInt(last_id)]).await?)?
            .into_iter()
            .next()
            .ok_or(OrmError::NotFound)
    } else {
        Ok(entity.clone())
    }
}

async fn do_update<T>(conn: &mut Conn, entity: &T) -> Result<T, OrmError>
where
    T: Entity + Serialize,
    T::Id: Serialize,
{
    let cols = T::columns();
    let set_cols: Vec<&ColumnDef> = cols.iter().filter(|c| !c.primary_key).collect();
    let obj = entity_object(entity)?;

    let assignments: Vec<String> = set_cols.iter().map(|c| format!("`{}` = ?", c.name)).collect();
    let mut binds: Vec<BindValue> = set_cols.iter().map(|c| bind_for::<T>(&obj, c)).collect();
    binds.extend(id_to_binds(&entity.id())?);
    let sql = format!(
        "UPDATE `{}` SET {} WHERE {}",
        T::table_name(),
        assignments.join(", "),
        pk_where_ph::<T>()
    );
    conn.exec_prepared(&sql, &binds).await?;
    Ok(entity.clone())
}

/// Delete by primary key.
pub async fn delete_by_id<T>(conn: &mut Conn, id: &T::Id) -> Result<(), OrmError>
where
    T: Entity,
    T::Id: Serialize,
{
    let binds = id_to_binds(id)?;
    let sql = format!("DELETE FROM `{}` WHERE {}", T::table_name(), pk_where_ph::<T>());
    conn.exec_prepared(&sql, &binds).await.map(|_| ())
}

// --- Batch / optimised paths ----------------------------------------------

/// Save many entities. When every entity is a fresh auto-increment row, this
/// issues **one** multi-row `INSERT` and re-reads the generated rows in a single
/// `SELECT`; otherwise it falls back to per-entity [`save`].
pub async fn save_all<T>(conn: &mut Conn, entities: Vec<T>) -> Result<Vec<T>, OrmError>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
{
    if entities.is_empty() {
        return Ok(Vec::new());
    }
    let mut all_new_auto = is_auto_pk::<T>();
    if all_new_auto {
        for e in &entities {
            if !is_new_auto_entity(e)? {
                all_new_auto = false;
                break;
            }
        }
    }

    if all_new_auto {
        insert_many(conn, &entities).await
    } else {
        let mut out = Vec::with_capacity(entities.len());
        for e in entities {
            out.push(save(conn, e).await?);
        }
        Ok(out)
    }
}

async fn insert_many<T>(conn: &mut Conn, entities: &[T]) -> Result<Vec<T>, OrmError>
where
    T: Entity + Serialize + DeserializeOwned,
{
    let cols = T::columns();
    let insert_cols: Vec<&ColumnDef> = cols.iter().filter(|c| !(c.primary_key && c.auto)).collect();
    let names: Vec<String> = insert_cols.iter().map(|c| format!("`{}`", c.name)).collect();

    let tuple = format!("({})", placeholders(insert_cols.len()));
    let mut tuples = Vec::with_capacity(entities.len());
    let mut binds: Vec<BindValue> = Vec::with_capacity(entities.len() * insert_cols.len());
    for entity in entities {
        let obj = entity_object(entity)?;
        for c in &insert_cols {
            binds.push(bind_for::<T>(&obj, c));
        }
        tuples.push(tuple.clone());
    }
    let sql = format!(
        "INSERT INTO `{}` ({}) VALUES {}",
        T::table_name(),
        names.join(", "),
        tuples.join(", ")
    );
    let first_id = match conn.exec_prepared(&sql, &binds).await? {
        QueryResult::Ok { last_insert_id, .. } => last_insert_id,
        QueryResult::Rows { .. } => 0,
    };

    // Consecutive auto-increment ids: first_id .. first_id + n - 1.
    let n = entities.len() as u64;
    let pk = pk_col_name::<T>();
    let sql = format!(
        "SELECT {} FROM `{}` WHERE `{}` BETWEEN ? AND ? ORDER BY `{}` ASC",
        col_list::<T>(),
        T::table_name(),
        pk,
        pk
    );
    rows_to_entities::<T>(
        conn.exec_prepared(&sql, &[BindValue::UInt(first_id), BindValue::UInt(first_id + n - 1)])
            .await?,
    )
}

/// Load many rows by id. Single-column keys use one `WHERE pk IN (?, …)`; a
/// composite key falls back to per-id lookups.
pub async fn find_all_by_ids<T>(conn: &mut Conn, ids: &[T::Id]) -> Result<Vec<T>, OrmError>
where
    T: Entity + DeserializeOwned,
    T::Id: Serialize,
{
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if pk_cols::<T>().len() > 1 {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(e) = find_by_id::<T>(conn, id, false).await? {
                out.push(e);
            }
        }
        return Ok(out);
    }
    let binds: Vec<BindValue> = ids
        .iter()
        .map(|id| id_to_binds(id).map(|mut v| v.remove(0)))
        .collect::<Result<_, _>>()?;
    let sql = format!(
        "SELECT {} FROM `{}` WHERE `{}` IN ({})",
        col_list::<T>(),
        T::table_name(),
        pk_col_name::<T>(),
        placeholders(ids.len())
    );
    rows_to_entities::<T>(conn.exec_prepared(&sql, &binds).await?)
}

/// Delete many rows by id. Single-column keys use one `WHERE pk IN (?, …)`; a
/// composite key falls back to per-id deletes.
pub async fn delete_all_by_ids<T>(conn: &mut Conn, ids: &[T::Id]) -> Result<(), OrmError>
where
    T: Entity,
    T::Id: Serialize,
{
    if ids.is_empty() {
        return Ok(());
    }
    if pk_cols::<T>().len() > 1 {
        for id in ids {
            delete_by_id::<T>(conn, id).await?;
        }
        return Ok(());
    }
    let binds: Vec<BindValue> = ids
        .iter()
        .map(|id| id_to_binds(id).map(|mut v| v.remove(0)))
        .collect::<Result<_, _>>()?;
    let sql = format!(
        "DELETE FROM `{}` WHERE `{}` IN ({})",
        T::table_name(),
        pk_col_name::<T>(),
        placeholders(ids.len())
    );
    conn.exec_prepared(&sql, &binds).await.map(|_| ())
}
