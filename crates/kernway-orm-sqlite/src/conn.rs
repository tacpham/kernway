//! SQLite's side of the shared SQL layer: a [`SqliteConn`] sends each
//! operation to its connection thread and awaits the answer.

use crate::config::{OpenMode, SqliteConfig};
use crate::oneshot;
use crate::worker::{self, map_err, Ctx, Job};
use crate::SqliteDialect;
use kernway_orm_core::{decode::{cells_to_entity, Cell}, driver::DriverCapabilities, entity::Entity, error::OrmError, SqlDialect};
use kernway_orm_sql::{CallArg, CallResult, OutKind, Outcome, Param, Rows, ServerCursor, SqlConn, Value};
use serde::de::DeserializeOwned;
use std::path::Path;
use rusqlite::InterruptHandle;
use std::sync::{mpsc, Arc};

static DIALECT: SqliteDialect = SqliteDialect;

/// Rows per step when streaming a result to the async side.
const BATCH: usize = 512;

/// Savepoint guarding `execute_many`.
const MANY_SAVEPOINT: &str = "kernway_execute_many";

/// One SQLite connection, owned by its own thread.
pub struct SqliteConn {
    jobs: Option<mpsc::Sender<Job>>,
    /// Stops the statement running on the thread (`sqlite3_interrupt`).
    interrupt: Arc<InterruptHandle>,
    read_only: bool,
}

/// Interrupts the job if its future is dropped before the answer came — a
/// timed-out or cancelled query stops at once instead of running to the end
/// and holding the connection.
struct CancelOnDrop<'a> {
    interrupt: &'a InterruptHandle,
    finished: bool,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.interrupt.interrupt();
        }
    }
}

impl SqliteConn {
    /// Open a connection (on a new thread) and set it up per `cfg`.
    pub async fn open(cfg: &SqliteConfig) -> Result<Self, OrmError> {
        let read_only = cfg.mode == OpenMode::ReadOnly;
        let (jobs, interrupt) = worker::spawn(cfg.clone()).await?;
        Ok(Self { jobs: Some(jobs), interrupt: Arc::new(interrupt), read_only })
    }

    /// Run `f` on the connection thread and await its result. A panic in
    /// SQLite code or a dead thread is an error, never a hang.
    pub(crate) async fn run<R: Send + 'static>(
        &mut self,
        f: impl for<'c> FnOnce(&mut Ctx<'c>) -> Result<R, OrmError> + Send + 'static,
    ) -> Result<R, OrmError> {
        let dead = || OrmError::Connection("the SQLite connection thread is gone".into());
        let jobs = self.jobs.as_ref().ok_or_else(dead)?;
        let (tx, rx) = oneshot::channel();
        let job: Job = Box::new(move |ctx| {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(ctx)))
                .unwrap_or_else(|_| Err(OrmError::Query("panic inside SQLite code".into())));
            tx.send(r);
        });
        if jobs.send(job).is_err() {
            self.jobs = None;
            return Err(dead());
        }
        let interrupt = Arc::clone(&self.interrupt);
        let mut guard = CancelOnDrop { interrupt: &interrupt, finished: false };
        let r = rx.await;
        guard.finished = true;
        r.unwrap_or_else(|| Err(dead()))
    }

    /// Stop whatever statement this connection is running now (it fails
    /// with "interrupted"). Safe from any thread.
    pub fn interrupt(&self) {
        self.interrupt.interrupt();
    }

    /// Copy the whole database into a new file at `path` (SQLite's online
    /// backup: consistent, while other connections keep working).
    pub async fn backup_to(&mut self, path: &Path) -> Result<(), OrmError> {
        let path = path.to_path_buf();
        self.run(move |ctx| ctx.conn.backup(rusqlite::DatabaseName::Main, &path, None).map_err(map_err)).await
    }

    /// Release every open cursor (at the end of a transaction).
    async fn close_cursors(&mut self) -> Result<(), OrmError> {
        self.run(|ctx| {
            ctx.cursors.clear();
            Ok(())
        })
        .await
    }

    async fn simple(&mut self, sql: &'static str) -> Result<(), OrmError> {
        self.run(move |ctx| ctx.conn.execute_batch(sql).map_err(map_err)).await
    }
}

/// A value as a decoder cell (for entity mapping).
fn cell(v: &Value) -> Cell<'_> {
    match v {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool(*b),
        Value::Int(i) => Cell::Int(*i),
        Value::UInt(u) => Cell::UInt(*u),
        Value::Float(f) => Cell::F64(*f),
        Value::Bytes(b) => Cell::Bytes(b),
        Value::Text(s) | Value::Decimal(s) | Value::Json(s) | Value::Date(s) | Value::Time(s) | Value::DateTime(s) | Value::Interval(s) => {
            Cell::Bytes(s.as_bytes())
        }
        other => Cell::Owned(format!("{other:?}")),
    }
}

impl SqlConn for SqliteConn {
    fn dialect() -> &'static dyn SqlDialect {
        &DIALECT
    }

    async fn select<T: Entity + DeserializeOwned>(&mut self, sql: &str, params: &[Param]) -> Result<Vec<T>, OrmError> {
        let rows = self.query_rows(sql, params).await?;
        let cols = T::columns();
        rows.rows
            .iter()
            .map(|r| {
                let cells: Vec<Cell<'_>> = r.iter().map(cell).collect();
                cells_to_entity::<T>(&cells, cols)
            })
            .collect()
    }

    async fn select_count(&mut self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        let rows = self.query_rows(sql, params).await?;
        Ok(rows.rows.first().and_then(|r| r.first()).and_then(Value::as_i64).map_or(0, |n| n.max(0) as u64))
    }

    async fn execute(&mut self, sql: &str, params: &[Param]) -> Result<u64, OrmError> {
        let (sql, params) = (sql.to_string(), params.to_vec());
        self.run(move |ctx| ctx.execute(&sql, &params).map(|(n, _)| n)).await
    }

    async fn insert_returning_id(&mut self, insert_sql: &str, params: &[Param], _pk_col: &str) -> Result<Param, OrmError> {
        let (sql, params) = (insert_sql.to_string(), params.to_vec());
        self.run(move |ctx| ctx.execute(&sql, &params).map(|(_, id)| Param::Int(id))).await
    }

    /// One multi-row `INSERT`: it holds the write lock for the whole
    /// statement, so the rows get consecutive rowids ending at the last one.
    async fn insert_batch_first_id(&mut self, insert_sql: &str, params: &[Param], rows: usize) -> Result<Option<u64>, OrmError> {
        let (sql, params) = (insert_sql.to_string(), params.to_vec());
        self.run(move |ctx| {
            let (n, last) = ctx.execute(&sql, &params)?;
            if n != rows as u64 {
                return Err(OrmError::Query(format!("batch INSERT stored {n} of {rows} rows")));
            }
            Ok(u64::try_from(last - rows as i64 + 1).ok())
        })
        .await
    }

    async fn query_rows(&mut self, sql: &str, params: &[Param]) -> Result<Rows, OrmError> {
        let (sql, params) = (sql.to_string(), params.to_vec());
        let (columns, rows) = self.run(move |ctx| ctx.query(&sql, &params)).await?;
        Ok(Rows { columns, rows })
    }

    /// Streamed: a cursor stepped 512 rows at a time, so a large
    /// result never sits in memory whole.
    async fn query_each<F>(&mut self, sql: &str, params: &[Param], mut each: F) -> Result<Outcome, OrmError>
    where
        F: FnMut(Vec<Value>) -> Result<(), OrmError>,
    {
        let (sql, params) = (sql.to_string(), params.to_vec());
        // A statement without rows just runs.
        let shape = self
            .run(move |ctx| {
                let cols = ctx.conn.prepare_cached(&sql).map_err(map_err)?.column_count();
                if cols == 0 {
                    let (n, id) = ctx.execute(&sql, &params)?;
                    return Ok(Err((n, id)));
                }
                ctx.open_cursor(&sql, &params).map(Ok)
            })
            .await?;
        let (id, columns) = match shape {
            Err((affected_rows, id)) => {
                return Ok(Outcome { columns: Vec::new(), affected_rows, last_insert_id: (affected_rows > 0).then_some(id as u64) })
            }
            Ok(c) => c,
        };
        loop {
            let (rows, done) = self.run(move |ctx| ctx.fetch(id, BATCH)).await?;
            for r in rows {
                if let Err(e) = each(r) {
                    if !done {
                        self.run(move |ctx| {
                            ctx.cursors.remove(&id);
                            Ok(())
                        })
                        .await?;
                    }
                    return Err(e);
                }
            }
            if done {
                return Ok(Outcome { columns, ..Outcome::default() });
            }
        }
    }

    /// As if the rows ran one by one — rows before a failing row stay
    /// applied, none after — but under one savepoint, so a file database
    /// commits once instead of once per row.
    async fn execute_many(&mut self, sql: &str, rows: &[Vec<Param>]) -> Result<u64, OrmError> {
        if rows.is_empty() {
            return Ok(0);
        }
        let (sql, rows) = (sql.to_string(), rows.to_vec());
        self.run(move |ctx| {
            ctx.conn.execute_batch(&format!("SAVEPOINT {MANY_SAVEPOINT}")).map_err(map_err)?;
            let mut total = 0;
            let mut failed = None;
            for row in &rows {
                match ctx.execute(&sql, row) {
                    Ok((n, _)) => total += n,
                    Err(e) => {
                        failed = Some(e);
                        break;
                    }
                }
            }
            // A failed statement is undone by SQLite on its own; releasing
            // keeps the rows before it.
            ctx.conn.execute_batch(&format!("RELEASE {MANY_SAVEPOINT}")).map_err(map_err)?;
            failed.map_or(Ok(total), Err)
        })
        .await
    }

    async fn cursor_open(&mut self, sql: &str, params: &[Param]) -> Result<ServerCursor, OrmError> {
        let (sql, params) = (sql.to_string(), params.to_vec());
        let (id, columns) = self.run(move |ctx| ctx.open_cursor(&sql, &params)).await?;
        Ok(ServerCursor::new(id, columns))
    }

    async fn cursor_fetch(&mut self, cursor: &mut ServerCursor, max_rows: usize) -> Result<Vec<Vec<Value>>, OrmError> {
        if cursor.is_done() {
            return Ok(Vec::new());
        }
        let id = cursor.id();
        let r = self.run(move |ctx| ctx.fetch(id, max_rows)).await;
        match r {
            Ok((rows, done)) => {
                if done {
                    cursor.finish();
                }
                Ok(rows)
            }
            Err(e) => {
                cursor.finish();
                Err(e)
            }
        }
    }

    async fn cursor_close(&mut self, cursor: ServerCursor) -> Result<(), OrmError> {
        let id = cursor.id();
        self.run(move |ctx| {
            ctx.cursors.remove(&id);
            Ok(())
        })
        .await
    }

    async fn execute_script(&mut self, script: &str) -> Result<(), OrmError> {
        let script = script.to_string();
        self.run(move |ctx| ctx.conn.execute_batch(&script).map_err(map_err)).await
    }

    async fn call(&mut self, _name: &str, _args: &[CallArg]) -> Result<CallResult, OrmError> {
        Err(OrmError::Unsupported(
            "SQLite has no stored procedures; register a Rust function with SqliteConfig::function and call it from SQL".into(),
        ))
    }

    /// `SELECT name(?, …)` — a built-in or a function registered with
    /// [`SqliteConfig::function`].
    async fn call_function(&mut self, name: &str, _returns: OutKind, args: &[CallArg]) -> Result<CallResult, OrmError> {
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || name.is_empty() {
            return Err(OrmError::Query(format!("not a function name: {name:?}")));
        }
        let params: Vec<Param> = args
            .iter()
            .map(|a| match a {
                CallArg::In(p) => Ok(p.clone()),
                _ => Err(OrmError::Unsupported("SQLite functions take positional IN arguments only".into())),
            })
            .collect::<Result<_, _>>()?;
        let marks = vec!["?"; params.len()].join(", ");
        let rows = self.query_rows(&format!("SELECT {name}({marks})"), &params).await?;
        let value = rows.rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null);
        Ok(CallResult { value, ..CallResult::default() })
    }

    /// `BEGIN IMMEDIATE`: the write lock is taken up front, so two
    /// read-then-write transactions queue (within the busy timeout) instead
    /// of one failing at its first write. A read-only database begins plain.
    async fn begin(&mut self) -> Result<(), OrmError> {
        let sql = if self.read_only { "BEGIN" } else { "BEGIN IMMEDIATE" };
        self.simple(sql).await
    }

    async fn commit(&mut self) -> Result<(), OrmError> {
        self.close_cursors().await?;
        self.simple("COMMIT").await
    }

    async fn rollback(&mut self) -> Result<(), OrmError> {
        self.close_cursors().await?;
        self.simple("ROLLBACK").await
    }

    fn capabilities() -> DriverCapabilities {
        DriverCapabilities { transactions: true, raw_query: true, full_text_search: true, json_columns: true, migrations: false }
    }
}
