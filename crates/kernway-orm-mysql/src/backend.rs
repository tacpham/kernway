//! MySQL's side of the shared SQL ORM layer (`kernway-orm-sql`): how a
//! [`Conn`] runs a statement, decodes rows, reports generated keys and
//! controls transactions. Everything else (CRUD SQL, the query builder,
//! repositories, transactions) is shared with the other backends.

use crate::protocol::{BindValue, Cell, ColMeta, Conn, Fetched, Pipelined, QueryResult};
use crate::MySqlDialect;
use kernway_orm_core::{decode::cells_to_entity, entity::Entity, error::OrmError, SqlDialect};
use kernway_orm_sql::{CallArg, CallResult, OutKind, Outcome, Param, Rows, ServerCursor, SqlConn, Value};
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
        // No object types: structured values travel as JSON.
        p @ (Param::Array(_) | Param::Object(_)) => BindValue::Str(p.to_json().to_string()),
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

fn named_unsupported() -> OrmError {
    OrmError::Unsupported("named arguments: MySQL has none; pass arguments by position".into())
}

/// A positional `CALL` argument: `OUT` slots are bound as NULL.
fn positional(a: &CallArg) -> Result<BindValue, OrmError> {
    match a {
        CallArg::In(p) | CallArg::InOut(p) | CallArg::InOutAs(p, _) => Ok(bind(p)),
        CallArg::Out(_) => Ok(BindValue::Null),
        CallArg::Named(..) => Err(named_unsupported()),
    }
}

fn has_delimiter_directive(script: &str) -> bool {
    script.lines().any(|l| l.trim_start().get(..10).is_some_and(|w| w.eq_ignore_ascii_case("DELIMITER ")))
}

/// Split a script the way the `mysql` client does: statements end at the
/// current delimiter (`;` until a `DELIMITER x` line changes it), outside
/// quotes (`'`, `"`, backticks) and comments (`-- `, `#`, `/* */`).
pub(crate) fn split_mysql_script(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut delim = ";".to_string();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut block_comment = false;
    for line in script.split_inclusive('\n') {
        if quote.is_none() && !block_comment && cur.trim().is_empty() {
            let t = line.trim();
            if t.len() > 10 && t[..10].eq_ignore_ascii_case("DELIMITER ") {
                delim = t[10..].trim().to_string();
                cur.clear();
                continue;
            }
        }
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if block_comment {
                cur.push(c);
                if c == '*' && chars.get(i + 1) == Some(&'/') {
                    cur.push('/');
                    i += 1;
                    block_comment = false;
                }
            } else if let Some(q) = quote {
                cur.push(c);
                if c == '\\' {
                    if let Some(n) = chars.get(i + 1) {
                        cur.push(*n);
                        i += 1;
                    }
                } else if c == q {
                    quote = None;
                }
            } else if c == '#' || (c == '-' && chars.get(i + 1) == Some(&'-') && chars.get(i + 2).is_none_or(|n| n.is_whitespace())) {
                // Comment to end of line.
                break;
            } else if c == '/' && chars.get(i + 1) == Some(&'*') {
                block_comment = true;
                cur.push_str("/*");
                i += 1;
            } else if matches!(c, '\'' | '"' | '`') {
                quote = Some(c);
                cur.push(c);
            } else if chars[i..].iter().collect::<String>().starts_with(&delim) {
                let stmt = cur.trim().to_string();
                if !stmt.is_empty() {
                    out.push(stmt);
                }
                cur.clear();
                i += delim.chars().count();
                continue;
            } else {
                cur.push(c);
            }
            i += 1;
        }
        if !line.ends_with('\n') || quote.is_some() || block_comment {
            continue;
        }
        cur.push('\n');
    }
    let stmt = cur.trim().to_string();
    if !stmt.is_empty() {
        out.push(stmt);
    }
    out
}

fn first_u64(cells: &[Cell<'_>]) -> Result<u64, OrmError> {
    Ok(cells.first().and_then(Cell::to_text).and_then(|t| t.parse().ok()).unwrap_or(0))
}

/// Savepoint guarding a pipelined `execute_many` inside a caller's transaction.
const BATCH_SAVEPOINT: &str = "kernway_execute_many";

impl Conn {
    /// Close every cursor still open (at the end of a transaction).
    async fn close_all_cursors(&mut self) -> Result<(), OrmError> {
        let open: Vec<_> = self.cursors.drain().map(|(_, c)| c).collect();
        for c in open {
            self.stmt_cursor_close(c).await?;
        }
        Ok(())
    }

    /// Undo a pipelined batch: back to the savepoint (caller's transaction)
    /// or roll back the batch's own transaction.
    async fn undo_batch(&mut self, own_tx: bool) -> Result<(), OrmError> {
        if own_tx {
            self.query("ROLLBACK").await?;
        } else {
            self.query(&format!("ROLLBACK TO SAVEPOINT {BATCH_SAVEPOINT}")).await?;
        }
        Ok(())
    }

    /// Keep a pipelined batch: commit its own transaction or release the
    /// savepoint.
    async fn keep_batch(&mut self, own_tx: bool) -> Result<(), OrmError> {
        if own_tx {
            self.query("COMMIT").await?;
        } else {
            self.query(&format!("RELEASE SAVEPOINT {BATCH_SAVEPOINT}")).await?;
        }
        Ok(())
    }

    async fn start_batch(&mut self, own_tx: bool) -> Result<(), OrmError> {
        if own_tx {
            self.query("START TRANSACTION").await?;
        } else {
            self.query(&format!("SAVEPOINT {BATCH_SAVEPOINT}")).await?;
        }
        Ok(())
    }

    /// `execute_many` statement by statement (statements with a result set,
    /// or after the server dropped the prepared statement).
    async fn execute_each_row(&mut self, sql: &str, rows: &[Vec<Param>]) -> Result<u64, OrmError> {
        let mut total = 0;
        for row in rows {
            total += SqlConn::execute(self, sql, row).await?;
        }
        Ok(total)
    }

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
        let params: Vec<BindValue> = args.iter().map(positional).collect::<Result<_, _>>()?;
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

    /// `SELECT name(?, …)`: MySQL functions take `IN` arguments only.
    async fn call_function(&mut self, name: &str, _returns: OutKind, args: &[CallArg]) -> Result<CallResult, OrmError> {
        if args.iter().any(|a| !matches!(a.inner(), CallArg::In(_))) {
            return Err(OrmError::Query("MySQL stored functions take IN arguments only".into()));
        }
        let call = crate::protocol::call_sql(name, args.len())?;
        let sql = format!("SELECT {}", &call["CALL ".len()..]);
        let params: Vec<Param> = args
            .iter()
            .map(|a| match a.inner() {
                CallArg::In(p) => Ok(p.clone()),
                _ => Err(named_unsupported()),
            })
            .collect::<Result<_, _>>()?;
        if args.iter().any(|a| matches!(a, CallArg::Named(..))) {
            return Err(named_unsupported());
        }
        let rows = self.query_rows(&sql, &params).await?;
        let value = rows.rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null);
        Ok(CallResult { value, ..CallResult::default() })
    }

    /// Without `DELIMITER` lines the server splits the script itself
    /// (multi-statement mode — it understands `BEGIN … END` bodies). With
    /// them (mysqldump / mysql-client scripts) it is split here, as the
    /// `mysql` client does, and run statement by statement.
    async fn execute_script(&mut self, script: &str) -> Result<(), OrmError> {
        if !has_delimiter_directive(script) {
            return Conn::execute_script(self, script).await;
        }
        for (i, stmt) in split_mysql_script(script).iter().enumerate() {
            self.query(stmt).await.map_err(|e| match e {
                OrmError::Query(m) => OrmError::Query(format!("statement {} of the script: {m}", i + 1)),
                other => other,
            })?;
        }
        Ok(())
    }

    async fn begin(&mut self) -> Result<(), OrmError> {
        self.query("START TRANSACTION").await.map(|_| ())
    }

    async fn commit(&mut self) -> Result<(), OrmError> {
        self.close_all_cursors().await?;
        self.query("COMMIT").await.map(|_| ())
    }

    async fn rollback(&mut self) -> Result<(), OrmError> {
        self.close_all_cursors().await?;
        self.query("ROLLBACK").await.map(|_| ())
    }

    /// Pipelined: the executes of up to 500 rows are written in one go and
    /// their replies read after, so a batch costs one round trip per 500
    /// rows. The outcome is the same as running the rows one by one: if row
    /// *k* fails, rows before it stay applied and the rest do not (the batch
    /// runs under its own transaction or a savepoint; after a failure it is
    /// rolled back and rows before *k* are run again).
    async fn execute_many(&mut self, sql: &str, rows: &[Vec<Param>]) -> Result<u64, OrmError> {
        match rows {
            [] => return Ok(0),
            [one] => return SqlConn::execute(self, sql, one).await,
            _ => {}
        }
        if !self.prepare_for_batch(sql).await? {
            return self.execute_each_row(sql, rows).await;
        }
        let bound: Vec<Vec<BindValue>> = rows.iter().map(|r| binds(r)).collect();
        let own_tx = !self.in_transaction();
        self.start_batch(own_tx).await?;
        let first = match self.execute_pipelined(sql, &bound).await {
            Ok(p) => p,
            Err(e) => {
                if !matches!(e, OrmError::Connection(_)) {
                    self.undo_batch(own_tx).await?;
                    if !own_tx {
                        self.keep_batch(false).await?; // release the savepoint
                    }
                }
                return Err(e);
            }
        };
        match first {
            Pipelined::Done(n) => {
                self.keep_batch(own_tx).await?;
                Ok(n)
            }
            Pipelined::Stale => {
                self.undo_batch(own_tx).await?;
                if !own_tx {
                    self.keep_batch(false).await?;
                }
                self.execute_each_row(sql, rows).await
            }
            Pipelined::Failed { index, error, .. } => {
                self.undo_batch(own_tx).await?;
                if index > 0 {
                    if own_tx {
                        self.start_batch(true).await?;
                    }
                    match self.execute_pipelined(sql, &bound[..index]).await? {
                        Pipelined::Done(_) => {}
                        Pipelined::Failed { error, .. } => {
                            self.undo_batch(own_tx).await?;
                            if !own_tx {
                                self.keep_batch(false).await?;
                            }
                            return Err(error);
                        }
                        Pipelined::Stale => {
                            self.undo_batch(own_tx).await?;
                            if !own_tx {
                                self.keep_batch(false).await?;
                            }
                            self.execute_each_row(sql, &rows[..index]).await?;
                            return Err(error);
                        }
                    }
                }
                self.keep_batch(own_tx).await?;
                Err(error)
            }
        }
    }

    /// `COM_STMT_EXECUTE` with a read-only cursor: the server keeps the
    /// result; each fetch is a `COM_STMT_FETCH`.
    async fn cursor_open(&mut self, sql: &str, params: &[Param]) -> Result<ServerCursor, OrmError> {
        let state = self
            .stmt_cursor_open(sql, &binds(params))
            .await?
            .ok_or_else(|| OrmError::Query("cursor_open: the statement returns no rows".into()))?;
        let id = self.next_cursor;
        self.next_cursor += 1;
        let columns = state.metas().iter().map(|m| m.name.clone()).collect();
        self.cursors.insert(id, state);
        Ok(ServerCursor::new(id, columns))
    }

    async fn cursor_fetch(&mut self, cursor: &mut ServerCursor, max_rows: usize) -> Result<Vec<Vec<Value>>, OrmError> {
        if cursor.is_done() {
            return Ok(Vec::new());
        }
        let mut state = self
            .cursors
            .remove(&cursor.id())
            .ok_or_else(|| OrmError::Query("cursor is closed (its transaction ended)".into()))?;
        let mut rows = Vec::new();
        let n = u32::try_from(max_rows).unwrap_or(u32::MAX);
        let r = self
            .stmt_cursor_fetch(&mut state, n, |metas, cells| {
                rows.push(row_values(metas, cells)?);
                Ok(())
            })
            .await;
        if state.is_done() {
            cursor.finish();
        } else {
            self.cursors.insert(cursor.id(), state);
        }
        r.map(|()| rows)
    }

    async fn cursor_close(&mut self, cursor: ServerCursor) -> Result<(), OrmError> {
        match self.cursors.remove(&cursor.id()) {
            Some(state) => self.stmt_cursor_close(state).await,
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::split_mysql_script;

    #[test]
    fn delimiter_scripts_split_like_the_mysql_client() {
        let script = "DROP PROCEDURE IF EXISTS p;
DELIMITER ;;
CREATE PROCEDURE p() BEGIN
  SELECT 'a;b'; -- c;omment
  SELECT \"x;;y\";
END ;;
DELIMITER ;
/* block; comment */ SELECT 1;
# hash; comment
SELECT `we;ird`;
";
        let parts = split_mysql_script(script);
        assert_eq!(parts.len(), 4, "{parts:#?}");
        assert_eq!(parts[0], "DROP PROCEDURE IF EXISTS p");
        assert!(parts[1].starts_with("CREATE PROCEDURE p() BEGIN") && parts[1].ends_with("END"), "{}", parts[1]);
        assert!(parts[1].contains("SELECT 'a;b';") && parts[1].contains("\"x;;y\""));
        assert!(parts[2].ends_with("SELECT 1"));
        assert_eq!(parts[3], "SELECT `we;ird`");
    }
}
