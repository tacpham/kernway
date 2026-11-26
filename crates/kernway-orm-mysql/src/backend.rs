//! MySQL's side of the shared SQL ORM layer (`kernway-orm-sql`): how a
//! [`Conn`] runs a statement, decodes rows, reports generated keys and
//! controls transactions. Everything else (CRUD SQL, the query builder,
//! repositories, transactions) is shared with the other backends.

use crate::protocol::{BindValue, Cell, ColMeta, Conn, Fetched, QueryResult};
use crate::MySqlDialect;
use kernway_orm_core::{decode::cells_to_entity, entity::Entity, error::OrmError, SqlDialect};
use kernway_orm_sql::{CallArg, CallResult, Outcome, Param, Rows, SqlConn, Value};
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

/// A decoded cell as a [`Value`] typed by its column. The text protocol
/// sends every value as text, the binary protocol numbers natively; both end
/// up the same.
pub(crate) fn to_value(meta: &ColMeta, cell: &Cell<'_>) -> Value {
    let text = || cell.to_text().unwrap_or_default();
    match cell {
        Cell::Null => return Value::Null,
        Cell::Bool(b) => return Value::Bool(*b),
        _ => {}
    }
    match meta.col_type {
        // TINY, SHORT, LONG, LONGLONG, INT24, YEAR
        0x01 | 0x02 | 0x03 | 0x08 | 0x09 | 0x0d => match cell {
            Cell::Int(i) => Value::Int(*i),
            Cell::UInt(u) if meta.unsigned => Value::UInt(*u),
            Cell::UInt(u) => i64::try_from(*u).map_or(Value::UInt(*u), Value::Int),
            _ => {
                let t = text();
                if meta.unsigned {
                    t.parse().map_or(Value::Text(t), Value::UInt)
                } else {
                    t.parse().map_or(Value::Text(t), Value::Int)
                }
            }
        },
        // FLOAT, DOUBLE — a FLOAT keeps its shortest decimal form (0.1, not 0.10000000149).
        0x04 | 0x05 => match cell {
            Cell::F64(f) => Value::Float(*f),
            _ => {
                let t = text();
                t.parse().map_or(Value::Text(t), Value::Float)
            }
        },
        0x00 | 0xf6 => Value::Decimal(text()),                // DECIMAL, NEWDECIMAL
        0x0a | 0x0e => Value::Date(text()),                   // DATE, NEWDATE
        0x07 | 0x0c | 0x11 | 0x12 => Value::DateTime(text()), // TIMESTAMP, DATETIME (+v2)
        0x0b | 0x13 => Value::Time(text()),                   // TIME (+v2)
        0xf5 => Value::Json(text()),                          // JSON
        0x10 | 0xff | 0xf2 => Value::Bytes(bytes(cell)),      // BIT, GEOMETRY, VECTOR
        _ if meta.binary => Value::Bytes(bytes(cell)),        // BINARY, VARBINARY, BLOB
        _ if meta.maybe_text => match String::from_utf8(bytes(cell)) {
            Ok(s) => Value::Text(s),
            Err(e) => Value::Bytes(e.into_bytes()),
        },
        _ => Value::Text(text()),                             // CHAR, VARCHAR, TEXT, ENUM, SET
    }
}

fn bytes(cell: &Cell<'_>) -> Vec<u8> {
    match cell {
        Cell::Bytes(b) => b.to_vec(),
        other => other.to_text().unwrap_or_default().into_bytes(),
    }
}

fn row_values(metas: &[ColMeta], cells: &[Cell<'_>]) -> Result<Vec<Value>, OrmError> {
    Ok(metas.iter().zip(cells).map(|(m, c)| to_value(m, c)).collect())
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

    async fn query_each<F>(&mut self, sql: &str, params: &[Param], mut each: F) -> Result<Outcome, OrmError>
    where
        F: FnMut(Vec<Value>) -> Result<(), OrmError>,
    {
        let mut row = |metas: &[ColMeta], cells: &[Cell<'_>]| each(row_values(metas, cells)?);
        let results = self.run_meta(sql, &binds(params), &mut row, false).await?;
        Ok(match results.into_iter().next() {
            Some(Fetched::Rows { columns, .. }) => Outcome { columns, ..Outcome::default() },
            Some(Fetched::Ok { affected_rows, last_insert_id }) => Outcome {
                columns: Vec::new(),
                affected_rows,
                last_insert_id: (last_insert_id != 0).then_some(last_insert_id),
            },
            None => Outcome::default(),
        })
    }

    /// A prepared `CALL`: `OUT` arguments are bound as NULL placeholders and
    /// come back in the result set the server flags as `OUT` parameters.
    async fn call(&mut self, name: &str, args: &[CallArg]) -> Result<CallResult, OrmError> {
        let sql = crate::protocol::call_sql(name, args.len())?;
        let params: Vec<BindValue> = args
            .iter()
            .map(|a| match a {
                CallArg::In(p) | CallArg::InOut(p) => bind(p),
                CallArg::Out(_) => BindValue::Null,
            })
            .collect();
        let mut row = row_values;
        // `CALL` is always prepared here (even without arguments) so `OUT`
        // values come back.
        let results = self.exec_prepared_all(&sql, &params, &mut row).await?;
        let mut out = CallResult::default();
        for r in results {
            match r {
                Fetched::Rows { out_params: true, rows, .. } => out.out = rows.into_iter().next().unwrap_or_default(),
                Fetched::Rows { columns, rows, .. } => out.results.push(Rows { columns, rows }),
                Fetched::Ok { affected_rows, .. } => out.affected_rows = affected_rows,
            }
        }
        Ok(out)
    }

    async fn execute_script(&mut self, script: &str) -> Result<(), OrmError> {
        Conn::execute_script(self, script).await
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
