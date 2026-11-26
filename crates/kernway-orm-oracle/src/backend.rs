//! Oracle's side of the shared SQL ORM layer (`kernway-orm-sql`): how a
//! [`Conn`] runs a statement, decodes rows, returns generated keys, and keeps
//! statements outside a transaction committed (Oracle has no autocommit).

use crate::dialect::OracleDialect;
use crate::exec::{Bind, OutType};
use crate::value::Value;
use crate::Conn;
use kernway_orm_core::{
    decode::{cells_to_entity, Cell},
    entity::{ColumnDef, ColumnType, Entity},
    error::OrmError,
    SqlDialect,
};
use kernway_orm_sql::{Outcome, Param, Rows, SqlConn};
use serde::de::DeserializeOwned;

static DIALECT: OracleDialect = OracleDialect;

fn bind(p: &Param) -> Bind {
    Bind::In(match p {
        Param::Null => Value::Null,
        Param::Int(i) => Value::Number(i.to_string()),
        Param::UInt(u) => Value::Number(u.to_string()),
        Param::Float(f) => Value::Float(*f),
        Param::Bool(b) => Value::Bool(*b),
        Param::Str(s) if s.is_empty() => Value::Null,
        Param::Str(s) => Value::Str(s.clone()),
        Param::Bytes(b) if b.is_empty() => Value::Null,
        Param::Bytes(b) => Value::Raw(b.clone()),
    })
}

fn binds(params: &[Param]) -> Vec<Bind> {
    params.iter().map(bind).collect()
}

/// A fetched value as a decoder cell for `col`. Oracle stores `''` as NULL,
/// so a NULL in a non-optional text column reads back as an empty string.
fn cell<'a>(v: &'a Value, col: Option<&ColumnDef>) -> Cell<'a> {
    match v {
        Value::Null => match col {
            Some(c) if !c.nullable && matches!(c.col_type, ColumnType::Text | ColumnType::Unknown) => Cell::Bytes(b""),
            Some(c) if !c.nullable && c.col_type == ColumnType::Binary => Cell::Bytes(b""),
            _ => Cell::Null,
        },
        Value::Str(s) | Value::Number(s) => Cell::Bytes(s.as_bytes()),
        Value::Float(f) => Cell::F64(*f),
        Value::Date(d) | Value::Timestamp(d) => Cell::Owned(d.to_string()),
        Value::Raw(b) => Cell::Bytes(b),
        Value::Bool(b) => Cell::Bool(*b),
        Value::Cursor(_) => Cell::Null,
    }
}

impl Conn {
    async fn run(&mut self, sql: &str, binds: &[Bind]) -> Result<crate::Executed, OrmError> {
        if self.in_tx {
            self.execute(sql, binds).await
        } else {
            self.execute_autocommit(sql, binds).await
        }
    }

    /// The identity sequence of `table`.`column` (for a generated key that
    /// must be known before an INSERT whose values cannot go through PL/SQL).
    async fn identity_sequence(&mut self, table: &str, column: &str) -> Result<String, OrmError> {
        let r = self
            .execute(
                "SELECT sequence_name FROM user_tab_identity_cols WHERE table_name = :t AND column_name = :c",
                &[Bind::from(table), Bind::from(column)],
            )
            .await?;
        r.rows
            .first()
            .and_then(|row| row.first())
            .and_then(Value::to_text)
            .ok_or_else(|| OrmError::Query(format!("no identity column {table}.{column}")))
    }
}

/// An Oracle value as a shared typed value. (Interim: the Oracle phase
/// refines NUMBER / temporal typing and streams fetch rounds.)
pub(crate) fn to_shared(v: Value) -> kernway_orm_sql::Value {
    use kernway_orm_sql::Value as V;
    match v {
        Value::Null => V::Null,
        Value::Str(s) => V::Text(s),
        Value::Number(n) => n.parse::<i64>().map_or(V::Decimal(n), V::Int),
        Value::Float(f) => V::Float(f),
        Value::Date(d) | Value::Timestamp(d) => V::DateTime(d.to_string()),
        Value::Raw(b) => V::Bytes(b),
        Value::Bool(b) => V::Bool(b),
        Value::Cursor(rs) => V::Rows(Box::new(Rows {
            columns: rs.columns.into_iter().map(|c| c.name).collect(),
            rows: rs.rows.into_iter().map(|r| r.into_iter().map(to_shared).collect()).collect(),
        })),
    }
}

/// Whether a bind is too long for PL/SQL (a long SQL bind instead).
fn is_long(p: &Param, max: u32) -> bool {
    match p {
        Param::Str(s) => s.len() as u64 * 4 > u64::from(max),
        Param::Bytes(b) => b.len() as u64 > u64::from(max),
        _ => false,
    }
}

impl SqlConn for Conn {
    fn dialect() -> &'static dyn SqlDialect {
        &DIALECT
    }

    async fn select<T: Entity + DeserializeOwned>(&mut self, sql: &str, params: &[Param]) -> Result<Vec<T>, OrmError> {
        let r = self.execute(sql, &binds(params)).await?;
        let cols = T::columns();
        r.rows
            .iter()
            .map(|row| {
                let cells: Vec<Cell<'_>> = row.iter().enumerate().map(|(i, v)| cell(v, cols.get(i))).collect();
                cells_to_entity::<T>(&cells, cols)
            })
            .collect()
    }

    async fn select_count(&mut self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        let r = self.execute(sql, &binds(params)).await?;
        Ok(r.rows.first().and_then(|row| row.first()).and_then(Value::as_i64).unwrap_or(0) as u64)
    }

    async fn execute(&mut self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        Ok(self.run(sql, &binds(params)).await?.row_count)
    }

    async fn insert_returning_id(&mut self, insert_sql: &str, params: &[Param], pk_col: &str) -> Result<Param, OrmError> {
        let quoted = DIALECT.quote_identifier(pk_col);
        if !params.iter().any(|p| is_long(p, self.max_string_size)) {
            // One round trip: the INSERT returns its key from a PL/SQL block.
            let sql = format!("BEGIN {insert_sql} RETURNING {quoted} INTO :kw_rid; END;");
            let mut b = binds(params);
            b.push(Bind::Out(OutType::Number));
            let r = self.run(&sql, &b).await?;
            let id = r.out.last().and_then(Value::as_i64).ok_or_else(|| OrmError::Query("no generated key returned".into()))?;
            return Ok(Param::Int(id));
        }
        // Long values cannot be PL/SQL binds: draw the key from the identity
        // sequence first, then insert it explicitly (the identity is
        // GENERATED BY DEFAULT, so an explicit key is accepted).
        let table = insert_sql
            .strip_prefix("INSERT INTO ")
            .and_then(|r| r.split_once(' '))
            .map(|(t, _)| t.trim_matches('"').to_string())
            .ok_or_else(|| OrmError::Query("unexpected INSERT shape".into()))?;
        let seq = self.identity_sequence(&table, pk_col).await?;
        let r = self.execute(&format!("SELECT \"{seq}\".NEXTVAL FROM dual"), &[]).await?;
        let id = r.rows.first().and_then(|row| row.first()).and_then(Value::as_i64).ok_or_else(|| OrmError::Query("no sequence value".into()))?;
        let (head, values) = insert_sql
            .split_once(") VALUES (")
            .ok_or_else(|| OrmError::Query("unexpected INSERT shape".into()))?;
        let sql = format!("{head}, {quoted}) VALUES ({values}", values = values.trim_end_matches(')').to_string() + &format!(", :{})", params.len() + 1));
        let mut b = binds(params);
        b.push(Bind::from(id));
        self.run(&sql, &b).await?;
        Ok(Param::Int(id))
    }

    async fn query_each<F>(&mut self, sql: &str, params: &[Param], mut each: F) -> Result<Outcome, OrmError>
    where
        F: FnMut(Vec<kernway_orm_sql::Value>) -> Result<(), OrmError>,
    {
        let r = self.run(sql, &binds(params)).await?;
        let columns: Vec<String> = r.columns.iter().map(|c| c.name.clone()).collect();
        let affected_rows = if columns.is_empty() { r.row_count } else { 0 };
        for row in r.rows {
            each(row.into_iter().map(to_shared).collect())?;
        }
        Ok(Outcome { columns, affected_rows, last_insert_id: None })
    }

    async fn begin(&mut self) -> Result<(), OrmError> {
        // Oracle starts a transaction implicitly; stop committing each call.
        self.in_tx = true;
        Ok(())
    }

    async fn commit(&mut self) -> Result<(), OrmError> {
        self.in_tx = false;
        Conn::commit(self).await
    }

    async fn rollback(&mut self) -> Result<(), OrmError> {
        self.in_tx = false;
        Conn::rollback(self).await
    }
}
