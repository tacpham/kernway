//! MySQL's side of the shared SQL ORM layer (`kernway-orm-sql`): how a
//! [`Conn`] runs a statement, decodes rows, reports generated keys and
//! controls transactions. Everything else (CRUD SQL, the query builder,
//! repositories, transactions) is shared with the other backends.

use crate::protocol::{BindValue, Cell, Conn, QueryResult};
use crate::MySqlDialect;
use kernway_orm_core::{decode::cells_to_entity, entity::Entity, error::OrmError, SqlDialect};
use kernway_orm_sql::{Param, SqlConn};
use serde::de::DeserializeOwned;

static DIALECT: MySqlDialect = MySqlDialect;

fn bind(p: &Param) -> BindValue {
    match p {
        Param::Null => BindValue::Null,
        Param::Int(i) => BindValue::Int(*i),
        Param::UInt(u) => BindValue::UInt(*u),
        Param::Float(f) => BindValue::Float(*f),
        Param::Bool(b) => BindValue::Int(i64::from(*b)),
        Param::Str(s) => BindValue::Str(s.clone()),
        Param::Bytes(b) => BindValue::Bytes(b.clone()),
    }
}

fn binds(params: &[Param]) -> Vec<BindValue> {
    params.iter().map(bind).collect()
}

fn first_u64(cells: &[Cell<'_>]) -> Result<u64, OrmError> {
    Ok(cells.first().and_then(Cell::to_text).and_then(|t| t.parse().ok()).unwrap_or(0))
}

impl Conn {
    /// Run `sql` (text protocol without parameters, prepared with them).
    async fn run(&mut self, sql: &str, params: &[Param]) -> Result<QueryResult, OrmError> {
        if params.is_empty() {
            self.query(sql).await
        } else {
            self.exec_prepared(sql, &binds(params)).await
        }
    }
}

impl SqlConn for Conn {
    fn dialect() -> &'static dyn SqlDialect {
        &DIALECT
    }

    async fn select<T: Entity + DeserializeOwned>(&mut self, sql: &str, params: &[Param]) -> Result<Vec<T>, OrmError> {
        let cols = T::columns();
        let decode = |cells: &[Cell<'_>]| cells_to_entity::<T>(cells, cols);
        let fetched = if params.is_empty() {
            self.query_with(sql, decode).await?
        } else {
            self.exec_prepared_with(sql, &binds(params), decode).await?
        };
        Ok(fetched.into_rows())
    }

    async fn select_count(&mut self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        let fetched = if params.is_empty() {
            self.query_with(sql, first_u64).await?
        } else {
            self.exec_prepared_with(sql, &binds(params), first_u64).await?
        };
        Ok(fetched.into_rows().into_iter().next().unwrap_or(0))
    }

    async fn execute(&mut self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        Ok(match self.run(sql, params).await? {
            QueryResult::Ok { affected_rows, .. } => affected_rows,
            QueryResult::Rows { .. } => 0,
        })
    }

    async fn insert_returning_id(&mut self, insert_sql: &str, params: &[Param], _pk_col: &str) -> Result<Param, OrmError> {
        match self.run(insert_sql, params).await? {
            QueryResult::Ok { last_insert_id, .. } => Ok(Param::UInt(last_insert_id)),
            QueryResult::Rows { .. } => Err(OrmError::Query("INSERT returned rows".into())),
        }
    }

    /// A multi-row INSERT reports the first generated key; InnoDB hands the
    /// rows of one statement consecutive keys.
    async fn insert_batch_first_id(&mut self, insert_sql: &str, params: &[Param], _rows: usize) -> Result<Option<u64>, OrmError> {
        match self.run(insert_sql, params).await? {
            QueryResult::Ok { last_insert_id, .. } => Ok(Some(last_insert_id)),
            QueryResult::Rows { .. } => Ok(None),
        }
    }

    async fn begin(&mut self) -> Result<(), OrmError> {
        self.query("START TRANSACTION").await.map(|_| ())
    }

    async fn commit(&mut self) -> Result<(), OrmError> {
        self.query("COMMIT").await.map(|_| ())
    }

    async fn rollback(&mut self) -> Result<(), OrmError> {
        self.query("ROLLBACK").await.map(|_| ())
    }
}
