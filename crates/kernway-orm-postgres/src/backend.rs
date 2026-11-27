//! PostgreSQL's side of the shared SQL ORM layer (`kernway-orm-sql`): how a
//! [`Conn`] runs a statement, decodes rows, returns generated keys, batches,
//! walks cursors, calls routines and controls transactions. Everything else
//! (CRUD SQL, the query builder, repositories, transactions) is shared.

use crate::protocol::{cell_value, Conn, Portal, QueryResult};
use crate::types;
use crate::PostgresDialect;
use kernway_orm_core::{decode::cells_to_entity, driver::DriverCapabilities, entity::Entity, error::OrmError, SqlDialect};
use kernway_orm_sql::{CallArg, CallResult, OutKind, Outcome, Param, ServerCursor, SqlConn, Value};
use serde::de::DeserializeOwned;
use std::rc::Rc;

static DIALECT: PostgresDialect = PostgresDialect;
const BATCH_SAVEPOINT: &str = "kw_batch";

impl Conn {
    async fn execute_many_rows(&mut self, sql: &str, rows: &[Vec<Param>]) -> Result<u64, OrmError> {
        let stmt = self.prepare(sql, &rows[0]).await?;
        let own_tx = !self.in_transaction();
        if !own_tx {
            self.simple(&format!("SAVEPOINT {BATCH_SAVEPOINT}")).await?;
        }
        match self.execute_pipelined(&stmt, rows).await {
            Ok(n) => {
                if !own_tx {
                    self.simple(&format!("RELEASE SAVEPOINT {BATCH_SAVEPOINT}")).await?;
                }
                Ok(n)
            }
            Err((_, e @ OrmError::Connection(_))) => Err(e),
            Err((index, error)) => {
                // The whole flight was undone (one implicit transaction, or
                // back to the savepoint): redo the rows before the failure, so
                // the batch behaves as if run row by row.
                if !own_tx {
                    self.simple(&format!("ROLLBACK TO SAVEPOINT {BATCH_SAVEPOINT}")).await?;
                }
                if index > 0 {
                    if let Err((_, again)) = self.execute_pipelined(&stmt, &rows[..index]).await {
                        if !own_tx && !matches!(again, OrmError::Connection(_)) {
                            self.simple(&format!("ROLLBACK TO SAVEPOINT {BATCH_SAVEPOINT}")).await?;
                            self.simple(&format!("RELEASE SAVEPOINT {BATCH_SAVEPOINT}")).await?;
                        }
                        return Err(again);
                    }
                }
                if !own_tx {
                    self.simple(&format!("RELEASE SAVEPOINT {BATCH_SAVEPOINT}")).await?;
                }
                Err(error)
            }
        }
    }

    /// Portals end with their transaction: forget them (their statements are
    /// closed with the next request).
    fn forget_cursors(&mut self) {
        let names: Vec<String> = self.cursors.drain().map(|(_, p)| p.stmt.name.clone()).collect();
        for n in names {
            self.queue_close(&n);
        }
    }

    async fn end_cursor(&mut self, p: Portal, commit: bool) -> Result<(), OrmError> {
        let closed = async {
            self.close(b'P', &p.name).await?;
            self.close(b'S', &p.stmt.name).await
        }
        .await;
        if p.own_tx {
            let end = if commit && closed.is_ok() { "COMMIT" } else { "ROLLBACK" };
            let ended = self.simple(end).await;
            closed?;
            return ended;
        }
        closed
    }

    async fn run_query_rows(&mut self, sql: &str, params: &[Param]) -> Result<Vec<Vec<Value>>, OrmError> {
        Ok(self.query_params(sql, params).await?.1)
    }

    /// Run a routine call with every parameter's type left to the server, so
    /// overload resolution sees `unknown` (an `int` argument accepts 5,
    /// which a declared `bigint` would not).
    async fn run_routine(&mut self, sql: &str, params: &[Param]) -> Result<(bool, Vec<Vec<Value>>, u64), OrmError> {
        let stmt = self.prepare_typed(sql, &vec![0; params.len()], true).await?;
        let mut rows = Vec::new();
        let done = self
            .execute_prepared(&stmt, params, |cols, cells| {
                rows.push(cols.iter().zip(cells).map(|(c, cell)| cell_value(c, cell)).collect());
                Ok(())
            })
            .await?;
        Ok((!stmt.columns.is_empty(), rows, done.affected))
    }

    async fn commit_or_fail(&mut self) -> Result<(), OrmError> {
        let failed = self.transaction_failed();
        let r = self.query("COMMIT").await?;
        // COMMIT of a failed transaction rolls back and says so in its tag.
        if failed || matches!(r.first(), Some(QueryResult::Ok { tag, .. }) if tag == "ROLLBACK") {
            return Err(OrmError::Transaction("the transaction had failed: rolled back instead of committed".into()));
        }
        Ok(())
    }
}

/// `name` checked as a (possibly schema-qualified, possibly quoted) routine name.
fn routine_name(name: &str) -> Result<&str, OrmError> {
    let ok = !name.is_empty()
        && name.split('.').all(|part| {
            let p = part.strip_prefix('"').and_then(|p| p.strip_suffix('"'));
            match p {
                Some(q) => !q.is_empty() && !q.contains('"'),
                None => !part.is_empty() && part.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$'),
            }
        });
    if ok {
        Ok(name)
    } else {
        Err(OrmError::Query(format!("not a routine name: {name:?}")))
    }
}

/// `name(args)` with `$n` placeholders; OUT arguments are `NULL` (PostgreSQL
/// returns them as the row a `CALL` produces). Named arguments use `=>`.
fn routine_call(name: &str, args: &[CallArg], allow_out: bool) -> Result<(String, Vec<Param>), OrmError> {
    let mut sql = format!("{}(", routine_name(name)?);
    let mut params = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        let (named, inner) = match a {
            CallArg::Named(n, inner) => (Some(n.as_str()), inner.as_ref()),
            other => (None, other),
        };
        if let Some(n) = named {
            sql.push_str(&format!("{} => ", DIALECT.quote_identifier(n)));
        }
        match inner {
            CallArg::In(p) | CallArg::InOut(p) | CallArg::InOutAs(p, _) => {
                if !allow_out && !matches!(inner, CallArg::In(_)) {
                    return Err(OrmError::Query("PostgreSQL functions take IN arguments only; use call() for a procedure".into()));
                }
                params.push(p.clone());
                sql.push_str(&format!("${}", params.len()));
            }
            CallArg::Out(_) if allow_out => sql.push_str("NULL"),
            CallArg::Out(_) => return Err(OrmError::Query("PostgreSQL functions take IN arguments only; use call() for a procedure".into())),
            CallArg::Named(..) => return Err(OrmError::Query("nested named argument".into())),
        }
    }
    sql.push(')');
    Ok((sql, params))
}

impl SqlConn for Conn {
    fn dialect() -> &'static dyn SqlDialect {
        &DIALECT
    }

    async fn select<T: Entity + DeserializeOwned>(&mut self, sql: &str, params: &[Param]) -> Result<Vec<T>, OrmError> {
        let cols = T::columns();
        let mut out = Vec::new();
        self.run(sql, params, |_, cells| {
            out.push(cells_to_entity::<T>(cells, cols)?);
            Ok(())
        })
        .await?;
        Ok(out)
    }

    async fn select_count(&mut self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        let rows = self.run_query_rows(sql, params).await?;
        let v = rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Int(0));
        v.as_i64().map(|n| n.max(0) as u64).ok_or_else(|| OrmError::TypeConversion(format!("count is not a number: {v:?}")))
    }

    async fn execute(&mut self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        Ok(self.run(sql, params, |_, _| Ok(())).await?.1.affected)
    }

    async fn insert_returning_id(&mut self, insert_sql: &str, params: &[Param], pk_col: &str) -> Result<Param, OrmError> {
        let sql = format!("{insert_sql} RETURNING {}", DIALECT.quote_identifier(pk_col));
        let rows = self.run_query_rows(&sql, params).await?;
        match rows.into_iter().next().and_then(|r| r.into_iter().next()) {
            Some(Value::Int(i)) => Ok(Param::Int(i)),
            Some(Value::UInt(u)) => Ok(Param::UInt(u)),
            Some(Value::Null) | None => Err(OrmError::Query("INSERT … RETURNING gave no key".into())),
            Some(other) => Ok(Param::Str(other.to_text().unwrap_or_default())),
        }
    }

    async fn query_each<F>(&mut self, sql: &str, params: &[Param], mut each: F) -> Result<Outcome, OrmError>
    where
        F: FnMut(Vec<Value>) -> Result<(), OrmError>,
    {
        let (stmt, done) = self
            .run(sql, params, |cols, cells| each(cols.iter().zip(cells).map(|(c, cell)| cell_value(c, cell)).collect()))
            .await?;
        let columns: Vec<String> = stmt.columns.iter().map(|c| c.name.clone()).collect();
        let affected_rows = if columns.is_empty() { done.affected } else { 0 };
        Ok(Outcome { columns, affected_rows, last_insert_id: None })
    }

    async fn execute_many(&mut self, sql: &str, rows: &[Vec<Param>]) -> Result<u64, OrmError> {
        match rows {
            [] => Ok(0),
            [one] => SqlConn::execute(self, sql, one).await,
            _ => self.execute_many_rows(sql, rows).await,
        }
    }

    async fn cursor_open(&mut self, sql: &str, params: &[Param]) -> Result<ServerCursor, OrmError> {
        let id = self.next_cursor;
        self.next_cursor += 1;
        let own_tx = !self.in_transaction();
        if own_tx {
            // Outside a transaction block a portal would end with its first fetch.
            self.simple("BEGIN").await?;
        }
        let oids: Vec<u32> = params.iter().map(types::param_oid).collect();
        let opened = async {
            let stmt = Rc::new(self.parse_describe(format!("kwcs{id}"), sql, &oids).await?);
            if stmt.columns.is_empty() {
                self.close(b'S', &stmt.name).await?;
                return Err(OrmError::Query("cursor_open: the statement returns no rows".into()));
            }
            let portal = format!("kwcp{id}");
            if let Err(e) = self.bind_portal(&portal, &stmt, params).await {
                self.queue_close(&stmt.name);
                return Err(e);
            }
            Ok(Portal { name: portal, stmt, own_tx })
        }
        .await;
        match opened {
            Ok(p) => {
                let columns = p.stmt.columns.iter().map(|c| c.name.clone()).collect();
                self.cursors.insert(id, p);
                Ok(ServerCursor::new(id, columns))
            }
            Err(e) => {
                if own_tx && !matches!(e, OrmError::Connection(_)) {
                    self.simple("ROLLBACK").await?;
                }
                Err(e)
            }
        }
    }

    async fn cursor_fetch(&mut self, cursor: &mut ServerCursor, max_rows: usize) -> Result<Vec<Vec<Value>>, OrmError> {
        if cursor.is_done() {
            return Ok(Vec::new());
        }
        let Some(p) = self.cursors.get(&cursor.id()) else {
            cursor.finish();
            return Err(OrmError::Query("cursor is no longer open (its transaction ended)".into()));
        };
        let (portal, stmt) = (p.name.clone(), p.stmt.clone());
        let mut rows = Vec::new();
        let fetched = self
            .fetch_portal(&portal, &stmt.columns, max_rows, |cols, cells| {
                rows.push(cols.iter().zip(cells).map(|(c, cell)| cell_value(c, cell)).collect());
                Ok(())
            })
            .await;
        match fetched {
            Ok(done) if done.suspended => Ok(rows),
            Ok(_) => {
                cursor.finish();
                if let Some(p) = self.cursors.remove(&cursor.id()) {
                    self.end_cursor(p, true).await?;
                }
                Ok(rows)
            }
            Err(e) => {
                cursor.finish();
                if let Some(p) = self.cursors.remove(&cursor.id()) {
                    if !matches!(e, OrmError::Connection(_)) {
                        let _ = self.end_cursor(p, false).await;
                    }
                }
                Err(e)
            }
        }
    }

    async fn cursor_close(&mut self, cursor: ServerCursor) -> Result<(), OrmError> {
        match self.cursors.remove(&cursor.id()) {
            Some(p) => self.end_cursor(p, true).await,
            None => Ok(()),
        }
    }

    async fn call(&mut self, name: &str, args: &[CallArg]) -> Result<CallResult, OrmError> {
        let (call, params) = routine_call(name, args, true)?;
        self.take_notices();
        let (has_out, rows, affected) = self.run_routine(&format!("CALL {call}"), &params).await?;
        let mut out = CallResult { affected_rows: affected, ..CallResult::default() };
        if has_out {
            out.out = rows.into_iter().next().unwrap_or_default();
        }
        out.output = self.take_notices().into_iter().map(|n| n.message).collect();
        Ok(out)
    }

    async fn call_function(&mut self, name: &str, _returns: OutKind, args: &[CallArg]) -> Result<CallResult, OrmError> {
        let (call, params) = routine_call(name, args, false)?;
        self.take_notices();
        let (_, rows, _) = self.run_routine(&format!("SELECT {call}"), &params).await?;
        let value = rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null);
        let output = self.take_notices().into_iter().map(|n| n.message).collect();
        Ok(CallResult { value, output, ..CallResult::default() })
    }

    async fn execute_script(&mut self, script: &str) -> Result<(), OrmError> {
        self.simple(script).await
    }

    async fn begin(&mut self) -> Result<(), OrmError> {
        self.simple("BEGIN").await
    }

    async fn commit(&mut self) -> Result<(), OrmError> {
        self.forget_cursors();
        self.commit_or_fail().await
    }

    async fn rollback(&mut self) -> Result<(), OrmError> {
        self.forget_cursors();
        self.simple("ROLLBACK").await
    }

    fn capabilities() -> DriverCapabilities {
        DriverCapabilities { transactions: true, raw_query: true, full_text_search: true, json_columns: true, migrations: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routine_calls() {
        let (sql, p) = routine_call(
            "app.transfer",
            &[CallArg::In(Param::Int(1)), CallArg::Out(OutKind::Number), CallArg::named("note", Param::Str("x".into()))],
            true,
        )
        .unwrap();
        assert_eq!(sql, "app.transfer($1, NULL, \"note\" => $2)");
        assert_eq!(p, vec![Param::Int(1), Param::Str("x".into())]);
        assert!(routine_call("f; DROP TABLE x", &[], true).is_err());
        assert!(routine_call("f", &[CallArg::Out(OutKind::Number)], false).is_err());
        assert_eq!(routine_call("\"My Fn\"", &[], false).unwrap().0, "\"My Fn\"()");
    }
}
