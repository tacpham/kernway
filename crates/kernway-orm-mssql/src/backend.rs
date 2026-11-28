//! SQL Server's side of the shared SQL ORM layer: statements through
//! `sp_executesql` with typed parameters, generated keys via
//! `SCOPE_IDENTITY()`, pipelined batches, API cursors, procedures with
//! `OUTPUT` parameters, `GO`-separated scripts, transactions.

use crate::protocol::{Column, Conn, Proc, Response, RpcParam, SP_CURSORCLOSE, SP_CURSORFETCH, SP_CURSOROPEN, SP_EXECUTESQL};
use crate::types::{self, Encoded, Target};
use crate::SqlServerDialect;
use kernway_orm_core::{decode::cells_to_entity, driver::DriverCapabilities, entity::Entity, error::OrmError, SqlDialect};
use kernway_orm_sql::{CallArg, CallResult, OutKind, Outcome, Param, ServerCursor, SqlConn, Value};
use serde::de::DeserializeOwned;
use std::rc::Rc;

static DIALECT: SqlServerDialect = SqlServerDialect;
/// Rows of a batch sent per request.
const BATCH_FLIGHT: usize = 200;
const BATCH_SAVEPOINT: &str = "kw_batch";

fn int_param(name: &str, v: Option<i32>, output: bool) -> RpcParam {
    let value = match v {
        Some(i) => [vec![4u8], i.to_le_bytes().to_vec()].concat(),
        None => vec![0],
    };
    RpcParam { name: name.into(), output, enc: Encoded { decl: "int".into(), type_info: vec![types::ty::INTN, 4], value } }
}

impl Conn {
    /// Parameter targets the server sees (`sp_describe_undeclared_parameters`),
    /// asked once per statement that binds a NULL (which must travel typed).
    async fn targets(&mut self, sql: &str, params: &[Param]) -> Vec<Target> {
        if !params.iter().any(|p| matches!(p, Param::Null)) {
            return vec![Target::Other; params.len()];
        }
        if let Some(t) = self.describe_cache.get(sql) {
            return (*t).clone();
        }
        let mut out = vec![Target::Other; params.len()];
        let enc = types::encode(&Param::Str(sql.to_string()), Target::Other, self.collation).expect("text encodes");
        let call = (Proc::Name("sp_describe_undeclared_parameters"), vec![RpcParam { name: "@tsql".into(), output: false, enc }]);
        self.arm_timeout();
        let mut found: Vec<(usize, String)> = Vec::new();
        if self.send_rpcs(&[call]).await.is_ok() {
            let r = self
                .read_response(|cols, vals| {
                    let get = |name: &str| cols.iter().position(|c| c.name == name).and_then(|i| vals[i].as_deref()).map(|b| types::value(&cols[cols.iter().position(|c| c.name == name).expect("found")].ti, Some(b)));
                    if let (Some(Value::Text(n)), Some(Value::Text(t))) = (get("name"), get("suggested_system_type_name")) {
                        if let Ok(i) = n.trim_start_matches("@P").parse::<usize>() {
                            found.push((i, t.to_ascii_lowercase()));
                        }
                    }
                    Ok(())
                })
                .await;
            if r.is_ok() {
                for (i, t) in found {
                    if (1..=params.len()).contains(&i) && (t.starts_with("varbinary") || t.starts_with("binary") || t.starts_with("image")) {
                        out[i - 1] = Target::Binary;
                    }
                }
            }
        }
        self.describe_cache.insert(sql, Rc::new(out.clone()));
        out
    }

    fn executesql_params(&self, sql: &str, params: &[Param], targets: &[Target]) -> Result<Vec<RpcParam>, OrmError> {
        let mut encoded = Vec::with_capacity(params.len());
        let mut decl = String::new();
        for (i, (p, t)) in params.iter().zip(targets).enumerate() {
            let e = types::encode(p, *t, self.collation)?;
            if i > 0 {
                decl.push(',');
            }
            decl.push_str(&format!("@P{} {}", i + 1, e.decl));
            encoded.push(RpcParam { name: format!("@P{}", i + 1), output: false, enc: e });
        }
        let mut out = vec![
            RpcParam { name: String::new(), output: false, enc: types::encode(&Param::Str(sql.to_string()), Target::Other, self.collation)? },
            RpcParam { name: String::new(), output: false, enc: types::encode(&Param::Str(decl), Target::Other, self.collation)? },
        ];
        out.extend(encoded);
        Ok(out)
    }

    /// Run `sql` with `params` (a batch without parameters; else
    /// `sp_executesql`), handing each row to `row`.
    pub async fn exec<F>(&mut self, sql: &str, params: &[Param], row: F) -> Result<Response, OrmError>
    where
        F: FnMut(&[Column], &[Option<Vec<u8>>]) -> Result<(), OrmError>,
    {
        if params.is_empty() {
            self.arm_timeout();
            self.send_batch(sql).await?;
            return self.read_response(row).await;
        }
        let targets = self.targets(sql, params).await;
        let rpc = self.executesql_params(sql, params, &targets)?;
        self.arm_timeout();
        self.send_rpcs(&[(Proc::Id(SP_EXECUTESQL), rpc)]).await?;
        self.read_response(row).await
    }

    /// Run `sql` with `params` and collect the first result set as typed values.
    pub async fn query_params(&mut self, sql: &str, params: &[Param]) -> Result<(Vec<Column>, Vec<Vec<Value>>, Response), OrmError> {
        let mut rows = Vec::new();
        let mut first: Option<Vec<Column>> = None;
        let resp = self
            .exec(sql, params, |cols, vals| {
                if first.is_none() {
                    first = Some(cols.to_vec());
                }
                if first.as_deref() == Some(cols) {
                    rows.push(cols.iter().zip(vals).map(|(c, v)| types::value(&c.ti, v.as_deref())).collect());
                }
                Ok(())
            })
            .await?;
        let cols = first.or_else(|| resp.results.first().cloned()).unwrap_or_default();
        Ok((cols, rows, resp))
    }

    /// Rows of the last result set (`INSERT …; SELECT SCOPE_IDENTITY()`).
    async fn last_set(&mut self, sql: &str, params: &[Param]) -> Result<Vec<Vec<Value>>, OrmError> {
        let mut sets: Vec<Vec<Vec<Value>>> = Vec::new();
        let mut last_cols: Option<Vec<Column>> = None;
        self.exec(sql, params, |cols, vals| {
            if last_cols.as_deref() != Some(cols) {
                last_cols = Some(cols.to_vec());
                sets.push(Vec::new());
            }
            sets.last_mut().expect("pushed").push(cols.iter().zip(vals).map(|(c, v)| types::value(&c.ti, v.as_deref())).collect());
            Ok(())
        })
        .await?;
        Ok(sets.pop().unwrap_or_default())
    }

    /// The columns of the unique constraint / index a violation names.
    async fn unique_columns(&mut self, e: OrmError) -> OrmError {
        let OrmError::UniqueViolation { field } = &e else { return e };
        if field.is_empty() {
            return e;
        }
        let sql = "SELECT c.name FROM sys.indexes i JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                   JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id WHERE i.name = @P1 ORDER BY ic.key_ordinal";
        match self.query_params(sql, &[Param::Str(field.clone())]).await {
            Ok((_, rows, _)) if !rows.is_empty() => {
                OrmError::UniqueViolation { field: rows.iter().filter_map(|r| r[0].as_str().map(String::from)).collect::<Vec<_>>().join(", ") }
            }
            _ => e,
        }
    }

    /// Pipelined `sp_executesql` calls for `rows`, in flights. Returns rows
    /// affected, or (index of the failing row, its error).
    async fn pipeline(&mut self, sql: &str, rows: &[Vec<Param>]) -> Result<u64, (usize, OrmError)> {
        let targets = self.targets(sql, &rows[0]).await;
        let mut total = 0;
        let mut done = 0;
        while done < rows.len() {
            let end = (done + BATCH_FLIGHT).min(rows.len());
            let mut calls = Vec::with_capacity(end - done);
            for r in &rows[done..end] {
                let t = if r.len() == targets.len() { targets.clone() } else { vec![Target::Other; r.len()] };
                calls.push((Proc::Id(SP_EXECUTESQL), self.executesql_params(sql, r, &t).map_err(|e| (done, e))?));
            }
            self.arm_timeout();
            self.send_rpcs(&calls).await.map_err(|e| (done, e))?;
            match self.read_response(|_, _| Ok(())).await {
                Ok(r) => total += r.affected(),
                Err(e) => return Err((done + self.error_proc.unwrap_or(0), e)),
            }
            done = end;
        }
        Ok(total)
    }

    async fn close_cursors(&mut self) {
        let handles: Vec<i32> = self.cursors.drain().map(|(_, h)| h).collect();
        for h in handles {
            let _ = self.rpc_discard(Proc::Id(SP_CURSORCLOSE), vec![int_param("", Some(h), false)]).await;
        }
    }

    async fn rpc_discard(&mut self, proc_: Proc<'_>, params: Vec<RpcParam>) -> Result<Response, OrmError> {
        self.arm_timeout();
        self.send_rpcs(&[(proc_, params)]).await?;
        self.read_response(|_, _| Ok(())).await
    }
}

/// The column an API cursor adds to every fetch (`ROWSTAT`, last, or a
/// column flagged hidden).
fn cursor_extra(i: usize, cols: &[Column], c: &Column) -> bool {
    c.hidden || (i + 1 == cols.len() && c.name.eq_ignore_ascii_case("rowstat"))
}

/// `name` checked as a (possibly schema-qualified, possibly bracketed) routine name.
fn routine_name(name: &str) -> Result<&str, OrmError> {
    let ok = !name.is_empty()
        && name.split('.').all(|part| {
            match part.strip_prefix('[').and_then(|p| p.strip_suffix(']')) {
                Some(q) => !q.is_empty() && !q.contains(']'),
                None => !part.is_empty() && part.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '#' || c == '@' || c == '$'),
            }
        });
    if ok {
        Ok(name)
    } else {
        Err(OrmError::Query(format!("not a routine name: {name:?}")))
    }
}

/// Split a script at `GO` lines (as sqlcmd and SSMS do; `GO n` repeats).
pub(crate) fn split_go(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in script.lines() {
        let t = line.trim();
        let mut words = t.split_whitespace();
        if words.next().is_some_and(|w| w.eq_ignore_ascii_case("go")) {
            let times = words.next().and_then(|n| n.parse::<usize>().ok()).unwrap_or(1);
            if words.next().is_none() && t.len() <= 12 {
                if !cur.trim().is_empty() {
                    for _ in 0..times {
                        out.push(cur.clone());
                    }
                }
                cur.clear();
                continue;
            }
        }
        cur.push_str(line);
        cur.push('\n');
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

impl SqlConn for Conn {
    fn dialect() -> &'static dyn SqlDialect {
        &DIALECT
    }

    async fn select<T: Entity + DeserializeOwned>(&mut self, sql: &str, params: &[Param]) -> Result<Vec<T>, OrmError> {
        let cols_def = T::columns();
        let mut out = Vec::new();
        self.exec(sql, params, |cols, vals| {
            let cells: Vec<_> = cols.iter().zip(vals).map(|(c, v)| types::cell(&c.ti, v.as_deref())).collect();
            out.push(cells_to_entity::<T>(&cells, cols_def)?);
            Ok(())
        })
        .await?;
        Ok(out)
    }

    async fn select_count(&mut self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        let (_, rows, _) = self.query_params(sql, params).await?;
        let v = rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Int(0));
        v.as_i64().map(|n| n.max(0) as u64).ok_or_else(|| OrmError::TypeConversion(format!("count is not a number: {v:?}")))
    }

    async fn execute(&mut self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        match self.exec(sql, params, |_, _| Ok(())).await {
            Ok(r) => Ok(r.affected()),
            Err(e) => Err(self.unique_columns(e).await),
        }
    }

    async fn insert_returning_id(&mut self, insert_sql: &str, params: &[Param], _pk_col: &str) -> Result<Param, OrmError> {
        let sql = format!("{insert_sql}; SELECT CAST(SCOPE_IDENTITY() AS BIGINT)");
        let rows = match self.last_set(&sql, params).await {
            Ok(r) => r,
            Err(e) => return Err(self.unique_columns(e).await),
        };
        match rows.into_iter().next().and_then(|r| r.into_iter().next()) {
            Some(Value::Int(i)) => Ok(Param::Int(i)),
            _ => Err(OrmError::Query("the INSERT generated no identity value".into())),
        }
    }

    async fn query_each<F>(&mut self, sql: &str, params: &[Param], mut each: F) -> Result<Outcome, OrmError>
    where
        F: FnMut(Vec<Value>) -> Result<(), OrmError>,
    {
        let mut first: Option<Vec<Column>> = None;
        let resp = self
            .exec(sql, params, |cols, vals| {
                if first.is_none() {
                    first = Some(cols.to_vec());
                }
                if first.as_deref() == Some(cols) {
                    each(cols.iter().zip(vals).map(|(c, v)| types::value(&c.ti, v.as_deref())).collect())?;
                }
                Ok(())
            })
            .await;
        let resp = match resp {
            Ok(r) => r,
            Err(e) => return Err(self.unique_columns(e).await),
        };
        let cols = first.or_else(|| resp.results.first().cloned()).unwrap_or_default();
        let columns: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        let column_info = cols.iter().map(|c| types::column_info(&c.name, &c.ti)).collect();
        let affected_rows = if columns.is_empty() { resp.affected() } else { 0 };
        Ok(Outcome { columns, column_info, affected_rows, last_insert_id: None })
    }

    async fn execute_many(&mut self, sql: &str, rows: &[Vec<Param>]) -> Result<u64, OrmError> {
        match rows {
            [] => return Ok(0),
            [one] => return SqlConn::execute(self, sql, one).await,
            _ => {}
        }
        let own_tx = !self.in_transaction();
        self.simple(&if own_tx { "BEGIN TRANSACTION".to_string() } else { format!("SAVE TRANSACTION {BATCH_SAVEPOINT}") }).await?;
        let undo = if own_tx { "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION".to_string() } else { format!("ROLLBACK TRANSACTION {BATCH_SAVEPOINT}") };
        match self.pipeline(sql, rows).await {
            Ok(n) => {
                if own_tx {
                    self.simple("COMMIT TRANSACTION").await?;
                }
                Ok(n)
            }
            Err((_, e @ OrmError::Connection(_))) => Err(e),
            Err((index, error)) => {
                // Later rows of the flight ran too: undo the flight, redo the
                // rows before the failure — as if run one by one.
                self.simple(&undo).await?;
                if index > 0 {
                    if own_tx {
                        self.simple("BEGIN TRANSACTION").await?;
                    } else {
                        self.simple(&format!("SAVE TRANSACTION {BATCH_SAVEPOINT}")).await?;
                    }
                    if let Err((_, again)) = self.pipeline(sql, &rows[..index]).await {
                        let _ = self.simple(&undo).await;
                        return Err(again);
                    }
                    if own_tx {
                        self.simple("COMMIT TRANSACTION").await?;
                    }
                }
                Err(self.unique_columns(error).await)
            }
        }
    }

    async fn cursor_open(&mut self, sql: &str, params: &[Param]) -> Result<ServerCursor, OrmError> {
        // FORWARD_ONLY | (PARAMETERIZED_STMT when bound); READ_ONLY.
        let scroll = 0x0004 | if params.is_empty() { 0 } else { 0x1000 };
        let mut rpc = vec![
            int_param("", None, true),
            RpcParam { name: String::new(), output: false, enc: types::encode(&Param::Str(sql.to_string()), Target::Other, self.collation)? },
            int_param("", Some(scroll), true),
            int_param("", Some(0x0001), true),
            int_param("", None, true),
        ];
        if !params.is_empty() {
            let targets = self.targets(sql, params).await;
            let mut ps = self.executesql_params(sql, params, &targets)?;
            ps.remove(0); // the statement is already the second argument
            rpc.extend(ps);
        }
        let resp = self.rpc_discard(Proc::Id(SP_CURSOROPEN), rpc).await?;
        let handle = match resp.return_values.first() {
            Some((_, _, Value::Int(h))) => *h as i32,
            _ => return Err(OrmError::Query("sp_cursoropen returned no cursor".into())),
        };
        let cols = resp.results.into_iter().find(|c| !c.is_empty()).unwrap_or_default();
        if cols.is_empty() || handle == 0 {
            let _ = self.rpc_discard(Proc::Id(SP_CURSORCLOSE), vec![int_param("", Some(handle), false)]).await;
            return Err(OrmError::Query("cursor_open: the statement returns no rows".into()));
        }
        let id = self.next_cursor;
        self.next_cursor += 1;
        self.cursors.insert(id, handle);
        Ok(ServerCursor::new(id, cols.iter().enumerate().filter(|(i, c)| !cursor_extra(*i, &cols, c)).map(|(_, c)| c.name.clone()).collect()))
    }

    async fn cursor_fetch(&mut self, cursor: &mut ServerCursor, max_rows: usize) -> Result<Vec<Vec<Value>>, OrmError> {
        if cursor.is_done() {
            return Ok(Vec::new());
        }
        let Some(&h) = self.cursors.get(&cursor.id()) else {
            cursor.finish();
            return Err(OrmError::Query("cursor is no longer open".into()));
        };
        let n = i32::try_from(max_rows.max(1)).unwrap_or(i32::MAX);
        // FETCH NEXT n rows.
        let rpc = vec![int_param("", Some(h), false), int_param("", Some(2), false), int_param("", Some(0), false), int_param("", Some(n), false)];
        self.arm_timeout();
        self.send_rpcs(&[(Proc::Id(SP_CURSORFETCH), rpc)]).await?;
        let mut rows = Vec::new();
        let r = self
            .read_response(|cols, vals| {
                rows.push(
                    cols.iter()
                        .zip(vals)
                        .enumerate()
                        .filter(|(i, (c, _))| !cursor_extra(*i, cols, c))
                        .map(|(_, (c, v))| types::value(&c.ti, v.as_deref()))
                        .collect::<Vec<Value>>(),
                );
                Ok(())
            })
            .await;
        if let Err(e) = r {
            cursor.finish();
            self.cursors.remove(&cursor.id());
            let _ = self.rpc_discard(Proc::Id(SP_CURSORCLOSE), vec![int_param("", Some(h), false)]).await;
            return Err(e);
        }
        if rows.len() < n as usize {
            cursor.finish();
            self.cursors.remove(&cursor.id());
            self.rpc_discard(Proc::Id(SP_CURSORCLOSE), vec![int_param("", Some(h), false)]).await?;
        }
        Ok(rows)
    }

    async fn cursor_close(&mut self, cursor: ServerCursor) -> Result<(), OrmError> {
        match self.cursors.remove(&cursor.id()) {
            Some(h) => self.rpc_discard(Proc::Id(SP_CURSORCLOSE), vec![int_param("", Some(h), false)]).await.map(|_| ()),
            None => Ok(()),
        }
    }

    async fn call(&mut self, name: &str, args: &[CallArg]) -> Result<CallResult, OrmError> {
        let name = routine_name(name)?;
        let mut params = Vec::with_capacity(args.len());
        for a in args {
            let (pname, inner) = match a {
                CallArg::Named(n, inner) => (format!("@{}", n.trim_start_matches('@')), inner.as_ref()),
                other => (String::new(), other),
            };
            let (output, enc) = match inner {
                CallArg::In(p) => (false, types::encode(p, Target::Other, self.collation)?),
                CallArg::InOut(p) | CallArg::InOutAs(p, _) => (true, types::encode(p, Target::Other, self.collation)?),
                CallArg::Out(k) => (true, types::out_null(*k, self.collation)),
                CallArg::Named(..) => return Err(OrmError::Query("nested named argument".into())),
            };
            params.push(RpcParam { name: pname, output, enc });
        }
        self.take_messages();
        self.arm_timeout();
        self.send_rpcs(&[(Proc::Name(name), params)]).await?;
        let mut sets: Vec<(Vec<Column>, Vec<Vec<Value>>)> = Vec::new();
        let resp = self
            .read_response(|cols, vals| {
                if sets.last().is_none_or(|(c, _)| c.as_slice() != cols) {
                    sets.push((cols.to_vec(), Vec::new()));
                }
                sets.last_mut().expect("pushed").1.push(cols.iter().zip(vals).map(|(c, v)| types::value(&c.ti, v.as_deref())).collect());
                Ok(())
            })
            .await?;
        let mut out = CallResult { affected_rows: resp.affected(), ..CallResult::default() };
        out.results = sets.into_iter().map(|(c, rows)| kernway_orm_sql::Rows { columns: c.into_iter().map(|c| c.name).collect(), rows }).collect();
        let mut rv = resp.return_values;
        rv.sort_by_key(|r| r.0);
        // OUT numbers are declared decimal(38,10): drop the padding zeros.
        let numeric_out: Vec<bool> = args
            .iter()
            .filter(|a| matches!(a.inner(), CallArg::Out(_) | CallArg::InOut(_) | CallArg::InOutAs(..)))
            .map(|a| matches!(a.inner(), CallArg::Out(OutKind::Number)))
            .collect();
        out.out = rv
            .into_iter()
            .enumerate()
            .map(|(i, r)| match r.2 {
                Value::Decimal(d) if numeric_out.get(i).copied().unwrap_or(false) && d.contains('.') => {
                    Value::Decimal(d.trim_end_matches('0').trim_end_matches('.').to_string())
                }
                v => v,
            })
            .collect();
        if let Some(s) = resp.return_status {
            out.value = Value::Int(i64::from(s));
        }
        out.output = self.take_messages().into_iter().map(|m| m.message).collect();
        Ok(out)
    }

    async fn call_function(&mut self, name: &str, _returns: OutKind, args: &[CallArg]) -> Result<CallResult, OrmError> {
        let name = routine_name(name)?;
        let mut params = Vec::new();
        for a in args {
            match a {
                CallArg::In(p) => params.push(p.clone()),
                CallArg::Named(..) => return Err(OrmError::Unsupported("named arguments: T-SQL functions take arguments by position".into())),
                _ => return Err(OrmError::Query("SQL Server functions take IN arguments only; use call() for a procedure".into())),
            }
        }
        let placeholders: Vec<String> = (1..=params.len()).map(|i| format!("@P{i}")).collect();
        let qualified = if name.contains('.') { name.to_string() } else { format!("dbo.{name}") };
        self.take_messages();
        let (_, rows, _) = self.query_params(&format!("SELECT {qualified}({})", placeholders.join(", ")), &params).await?;
        let value = rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null);
        Ok(CallResult { value, output: self.take_messages().into_iter().map(|m| m.message).collect(), ..CallResult::default() })
    }

    async fn execute_script(&mut self, script: &str) -> Result<(), OrmError> {
        for (i, batch) in split_go(script).iter().enumerate() {
            self.simple(batch).await.map_err(|e| match e {
                OrmError::Query(m) => OrmError::Query(format!("batch {} of the script: {m}", i + 1)),
                other => other,
            })?;
        }
        Ok(())
    }

    async fn begin(&mut self) -> Result<(), OrmError> {
        self.simple("BEGIN TRANSACTION").await.map(|_| ())
    }

    async fn commit(&mut self) -> Result<(), OrmError> {
        self.close_cursors().await;
        if !self.in_transaction() {
            return Err(OrmError::Transaction("no transaction to commit (an error rolled it back)".into()));
        }
        self.simple("COMMIT TRANSACTION").await.map(|_| ())
    }

    async fn rollback(&mut self) -> Result<(), OrmError> {
        self.close_cursors().await;
        self.simple("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await.map(|_| ())
    }

    fn capabilities() -> DriverCapabilities {
        DriverCapabilities { transactions: true, raw_query: true, full_text_search: false, json_columns: true, migrations: false }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_splits_scripts() {
        let s = "CREATE TABLE a (x int)\nGO\nINSERT a VALUES (1)\n  go 2\nSELECT 'GO here' AS goal\nGO\n";
        assert_eq!(split_go(s), ["CREATE TABLE a (x int)\n", "INSERT a VALUES (1)\n", "INSERT a VALUES (1)\n", "SELECT 'GO here' AS goal\n"]);
        assert!(routine_name("dbo.[my proc]").is_ok());
        assert!(routine_name("x; DROP TABLE t").is_err());
    }
}
