//! CRUD against one connection, shared by the autocommit repository and the
//! transaction scope. SQL comes from the backend's dialect; every value is a
//! bound parameter.

use crate::param::{entity_object, id_to_params, is_auto_pk, is_new_auto_entity, json_to_param, pk_cols, Param};
use crate::SqlConn;
use kernway_orm_core::{entity::Entity, error::OrmError, ColumnDef, SqlDialect};
use serde::{de::DeserializeOwned, Serialize};

/// SQL text with its parameters; placeholders are numbered as they are added.
pub struct Sql {
    /// The statement text.
    pub text: String,
    /// The bound values, in placeholder order.
    pub params: Vec<Param>,
    dialect: &'static dyn SqlDialect,
}

impl Sql {
    /// Start a statement for `dialect`.
    pub fn new(dialect: &'static dyn SqlDialect) -> Self {
        Self { text: String::new(), params: Vec::new(), dialect }
    }
    /// Append raw SQL.
    pub fn push(&mut self, s: &str) -> &mut Self {
        self.text.push_str(s);
        self
    }
    /// Append a quoted identifier.
    pub fn ident(&mut self, name: &str) -> &mut Self {
        let q = self.dialect.quote_identifier(name);
        self.text.push_str(&q);
        self
    }
    /// The dialect the statement is written in.
    pub fn dialect(&self) -> &'static dyn SqlDialect {
        self.dialect
    }
    /// Append a placeholder bound to `p`.
    pub fn param(&mut self, p: Param) -> &mut Self {
        self.params.push(p);
        let ph = self.dialect.placeholder(self.params.len());
        self.text.push_str(&ph);
        self
    }
    /// Append `a, b, c` placeholders for `ps`.
    pub fn param_list(&mut self, ps: impl IntoIterator<Item = Param>) -> &mut Self {
        for (i, p) in ps.into_iter().enumerate() {
            if i > 0 {
                self.push(", ");
            }
            self.param(p);
        }
        self
    }
}

/// `"a", "b", …` for the entity's columns.
pub(crate) fn col_list<T: Entity>(d: &dyn SqlDialect) -> String {
    T::columns().iter().map(|c| d.quote_identifier(c.name)).collect::<Vec<_>>().join(", ")
}

fn select_from<T: Entity>(d: &'static dyn SqlDialect) -> Sql {
    let mut q = Sql::new(d);
    q.push("SELECT ").push(&col_list::<T>(d)).push(" FROM ").ident(T::table_name());
    q
}

/// `SELECT … FROM t` with the dialect's locking table hint, if it has one.
fn select_from_locked<T: Entity>(d: &'static dyn SqlDialect) -> Sql {
    let mut q = select_from::<T>(d);
    let hint = d.lock_table_hint();
    if !hint.is_empty() {
        q.push(" ").push(hint);
    }
    q
}

/// ` WHERE pk1 = ? AND pk2 = ?` for a key.
fn where_pk<T: Entity>(q: &mut Sql, id: Vec<Param>) {
    q.push(" WHERE ");
    for (i, (col, p)) in pk_cols::<T>().into_iter().zip(id).enumerate() {
        if i > 0 {
            q.push(" AND ");
        }
        q.ident(col).push(" = ").param(p);
    }
}

fn dialect<C: SqlConn>() -> &'static dyn SqlDialect {
    C::dialect()
}

/// `CREATE TABLE IF NOT EXISTS` for `T`.
pub async fn ensure_table<T: Entity, C: SqlConn>(c: &mut C) -> Result<(), OrmError> {
    let sql = dialect::<C>().create_table_sql(T::table_name(), T::columns());
    c.execute(&sql, &[]).await.map(|_| ())
}

/// Find by primary key, optionally taking a row lock (`FOR UPDATE`).
pub async fn find_by_id<T, C>(c: &mut C, id: &T::Id, lock: bool) -> Result<Option<T>, OrmError>
where
    T: Entity + DeserializeOwned,
    T::Id: Serialize,
    C: SqlConn,
{
    let d = dialect::<C>();
    let mut q = if lock { select_from_locked::<T>(d) } else { select_from::<T>(d) };
    where_pk::<T>(&mut q, id_to_params(id)?);
    if lock && !d.lock_for_update_suffix().is_empty() {
        q.push(" ").push(d.lock_for_update_suffix());
    }
    Ok(c.select::<T>(&q.text, &q.params).await?.into_iter().next())
}

/// Every row.
pub async fn find_all<T: Entity + DeserializeOwned, C: SqlConn>(c: &mut C) -> Result<Vec<T>, OrmError> {
    let q = select_from::<T>(dialect::<C>());
    c.select::<T>(&q.text, &q.params).await
}

/// Total row count.
pub async fn count<T: Entity, C: SqlConn>(c: &mut C) -> Result<u64, OrmError> {
    let mut q = Sql::new(dialect::<C>());
    q.push("SELECT COUNT(*) FROM ").ident(T::table_name());
    c.select_count(&q.text, &q.params).await
}

/// Whether a row with this key exists.
pub async fn exists_by_id<T, C>(c: &mut C, id: &T::Id) -> Result<bool, OrmError>
where
    T: Entity,
    T::Id: Serialize,
    C: SqlConn,
{
    let mut q = Sql::new(dialect::<C>());
    q.push("SELECT COUNT(*) FROM ").ident(T::table_name());
    where_pk::<T>(&mut q, id_to_params(id)?);
    Ok(c.select_count(&q.text, &q.params).await? > 0)
}

/// Insert or update, returning the stored form (with any generated key).
pub async fn save<T, C>(c: &mut C, entity: T) -> Result<T, OrmError>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize,
    C: SqlConn,
{
    if is_new_auto_entity(&entity)? {
        return insert(c, &entity).await;
    }
    if is_auto_pk::<T>() || exists_by_id::<T, C>(c, &entity.id()).await? {
        update(c, &entity).await
    } else {
        insert(c, &entity).await
    }
}

fn insert_cols<T: Entity>() -> Vec<&'static ColumnDef> {
    T::columns().iter().filter(|c| !(c.primary_key && c.auto)).collect()
}

fn insert_head<T: Entity>(d: &'static dyn SqlDialect) -> Sql {
    let cols = insert_cols::<T>();
    let mut q = Sql::new(d);
    q.push("INSERT INTO ").ident(T::table_name()).push(" (");
    for (i, c) in cols.iter().enumerate() {
        if i > 0 {
            q.push(", ");
        }
        q.ident(c.name);
    }
    q.push(") VALUES ");
    q
}

fn row_params<T: Serialize>(entity: &T, cols: &[&ColumnDef]) -> Result<Vec<Param>, OrmError> {
    let obj = entity_object(entity)?;
    Ok(cols
        .iter()
        .map(|c| json_to_param(obj.get(c.field).unwrap_or(&serde_json::Value::Null), c))
        .collect())
}

async fn insert<T, C>(c: &mut C, entity: &T) -> Result<T, OrmError>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize,
    C: SqlConn,
{
    let d = dialect::<C>();
    let mut q = insert_head::<T>(d);
    q.push("(").param_list(row_params(entity, &insert_cols::<T>())?).push(")");
    if !is_auto_pk::<T>() {
        c.execute(&q.text, &q.params).await?;
        return Ok(entity.clone());
    }
    let pk = pk_cols::<T>()[0];
    let id = c.insert_returning_id(&q.text, &q.params, pk).await?;
    let mut sel = select_from::<T>(d);
    where_pk::<T>(&mut sel, vec![id]);
    c.select::<T>(&sel.text, &sel.params).await?.into_iter().next().ok_or(OrmError::NotFound)
}

async fn update<T, C>(c: &mut C, entity: &T) -> Result<T, OrmError>
where
    T: Entity + Serialize,
    T::Id: Serialize,
    C: SqlConn,
{
    let set_cols: Vec<&ColumnDef> = T::columns().iter().filter(|c| !c.primary_key).collect();
    let values = row_params(entity, &set_cols)?;
    let mut q = Sql::new(dialect::<C>());
    q.push("UPDATE ").ident(T::table_name()).push(" SET ");
    for (i, (col, p)) in set_cols.iter().zip(values).enumerate() {
        if i > 0 {
            q.push(", ");
        }
        q.ident(col.name).push(" = ").param(p);
    }
    where_pk::<T>(&mut q, id_to_params(&entity.id())?);
    c.execute(&q.text, &q.params).await?;
    Ok(entity.clone())
}

/// Delete by primary key.
pub async fn delete_by_id<T, C>(c: &mut C, id: &T::Id) -> Result<(), OrmError>
where
    T: Entity,
    T::Id: Serialize,
    C: SqlConn,
{
    let mut q = Sql::new(dialect::<C>());
    q.push("DELETE FROM ").ident(T::table_name());
    where_pk::<T>(&mut q, id_to_params(id)?);
    c.execute(&q.text, &q.params).await.map(|_| ())
}

/// Save many. Fresh rows with generated keys go in one multi-row `INSERT`
/// when the backend can report consecutive keys; otherwise row by row.
pub async fn save_all<T, C>(c: &mut C, entities: Vec<T>) -> Result<Vec<T>, OrmError>
where
    T: Entity + Serialize + DeserializeOwned,
    T::Id: Serialize + DeserializeOwned,
    C: SqlConn,
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
    let mut out = Vec::with_capacity(entities.len());
    let mut rest = entities;
    if all_new_auto && rest.len() > 1 {
        let d = dialect::<C>();
        let cols = insert_cols::<T>();
        // As many rows per INSERT as the backend's parameter limit allows.
        let per = (d.max_bind_params() / cols.len().max(1)).max(1);
        while rest.len() > 1 {
            let n = rest.len().min(per);
            let mut q = insert_head::<T>(d);
            for (i, e) in rest[..n].iter().enumerate() {
                if i > 0 {
                    q.push(", ");
                }
                q.push("(").param_list(row_params(e, &cols)?).push(")");
            }
            let Some(first) = c.insert_batch_first_id(&q.text, &q.params, n).await? else { break };
            let pk = pk_cols::<T>()[0];
            let mut sel = select_from::<T>(d);
            sel.push(" WHERE ")
                .ident(pk)
                .push(" BETWEEN ")
                .param(Param::UInt(first))
                .push(" AND ")
                .param(Param::UInt(first + n as u64 - 1))
                .push(" ORDER BY ")
                .ident(pk)
                .push(" ASC");
            out.extend(c.select::<T>(&sel.text, &sel.params).await?);
            rest.drain(..n);
        }
    }
    for e in rest {
        out.push(save(c, e).await?);
    }
    Ok(out)
}

/// How many keys one `IN (…)` statement may carry on this backend.
fn in_chunk(d: &dyn SqlDialect) -> usize {
    d.max_in_list().min(d.max_bind_params()).max(1)
}

/// Load many rows by key (one `IN (…)` for a single-column key).
pub async fn find_all_by_ids<T, C>(c: &mut C, ids: &[T::Id]) -> Result<Vec<T>, OrmError>
where
    T: Entity + DeserializeOwned,
    T::Id: Serialize,
    C: SqlConn,
{
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    if pk_cols::<T>().len() > 1 {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(e) = find_by_id::<T, C>(c, id, false).await? {
                out.push(e);
            }
        }
        return Ok(out);
    }
    let params: Vec<Param> = ids
        .iter()
        .map(|id| id_to_params(id).map(|mut v| v.remove(0)))
        .collect::<Result<_, _>>()?;
    let d = dialect::<C>();
    let mut out = Vec::with_capacity(params.len());
    for chunk in params.chunks(in_chunk(d)) {
        let mut q = select_from::<T>(d);
        q.push(" WHERE ").ident(pk_cols::<T>()[0]).push(" IN (").param_list(chunk.iter().cloned()).push(")");
        out.extend(c.select::<T>(&q.text, &q.params).await?);
    }
    Ok(out)
}

/// Delete many rows by key (one `IN (…)` for a single-column key).
pub async fn delete_all_by_ids<T, C>(c: &mut C, ids: &[T::Id]) -> Result<(), OrmError>
where
    T: Entity,
    T::Id: Serialize,
    C: SqlConn,
{
    if ids.is_empty() {
        return Ok(());
    }
    if pk_cols::<T>().len() > 1 {
        for id in ids {
            delete_by_id::<T, C>(c, id).await?;
        }
        return Ok(());
    }
    let params: Vec<Param> = ids
        .iter()
        .map(|id| id_to_params(id).map(|mut v| v.remove(0)))
        .collect::<Result<_, _>>()?;
    let d = dialect::<C>();
    for chunk in params.chunks(in_chunk(d)) {
        let mut q = Sql::new(d);
        q.push("DELETE FROM ").ident(T::table_name()).push(" WHERE ").ident(pk_cols::<T>()[0]).push(" IN (").param_list(chunk.iter().cloned()).push(")");
        c.execute(&q.text, &q.params).await?;
    }
    Ok(())
}
