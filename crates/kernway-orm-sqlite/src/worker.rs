//! The connection's own thread. SQLite is a library — every call does file
//! I/O and may wait on a lock — so a connection lives on a dedicated thread
//! and the async side only sends it work and awaits the reply. The reactor
//! never runs SQLite code.
//!
//! Open cursors are prepared statements kept on this thread between
//! requests, stepped a batch at a time: the result is never materialised.

use crate::config::{Location, OpenMode, SqliteConfig};
use crate::oneshot;
use kernway_orm_core::error::OrmError;
use kernway_orm_sql::{Param, Value};
use rusqlite::functions::FunctionFlags;
use rusqlite::types::{ToSqlOutput, Value as SqlValue, ValueRef};
use rusqlite::{Connection, InterruptHandle, OpenFlags, Statement};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;

/// What a job sees: the connection and its open cursors.
pub(crate) struct Ctx<'c> {
    pub(crate) conn: &'c Connection,
    pub(crate) cursors: HashMap<u64, Cursor<'c>>,
    pub(crate) next_cursor: u64,
}

/// A statement being stepped across requests.
pub(crate) struct Cursor<'c> {
    pub(crate) stmt: Statement<'c>,
    pub(crate) decl: Vec<Option<String>>,
}

pub(crate) type Job = Box<dyn for<'c> FnOnce(&mut Ctx<'c>) + Send>;

static NEXT_THREAD: AtomicU64 = AtomicU64::new(0);

/// Start a connection thread, open the database on it and set it up. The
/// returned sender submits jobs; dropping it ends the thread and closes the
/// connection.
pub(crate) async fn spawn(cfg: SqliteConfig) -> Result<(mpsc::Sender<Job>, InterruptHandle), OrmError> {
    let (tx, rx) = mpsc::channel::<Job>();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<InterruptHandle, OrmError>>();
    let name = format!("kernway-sqlite-{}", NEXT_THREAD.fetch_add(1, Ordering::Relaxed));
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            let conn = match open(&cfg) {
                Ok(c) => c,
                Err(e) => return ready_tx.send(Err(e)),
            };
            ready_tx.send(Ok(conn.get_interrupt_handle()));
            let mut ctx = Ctx { conn: &conn, cursors: HashMap::new(), next_cursor: 1 };
            while let Ok(job) = rx.recv() {
                job(&mut ctx);
            }
            // Statements first, then the connection.
            drop(ctx);
            drop(conn);
        })
        .map_err(|e| OrmError::Connection(format!("cannot start the SQLite thread: {e}")))?;
    match ready_rx.await {
        Some(Ok(interrupt)) => Ok((tx, interrupt)),
        Some(Err(e)) => Err(e),
        None => Err(OrmError::Connection("the SQLite thread died while opening".into())),
    }
}

fn open(cfg: &SqliteConfig) -> Result<Connection, OrmError> {
    let conn_err = |e: rusqlite::Error| OrmError::Connection(e.to_string());
    let mut flags = OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI;
    flags |= match cfg.mode {
        OpenMode::ReadWriteCreate => OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        OpenMode::ReadWrite => OpenFlags::SQLITE_OPEN_READ_WRITE,
        OpenMode::ReadOnly => OpenFlags::SQLITE_OPEN_READ_ONLY,
    };
    let mut conn = match &cfg.location {
        // A path is opened as a path, not a URI, so `?` / `#` in a file name are literal.
        Location::File(p) => Connection::open_with_flags(p, flags - OpenFlags::SQLITE_OPEN_URI),
        // memdb: one shared in-memory database for every connection that names it.
        Location::Memory(name) => Connection::open_with_flags(
            format!("file:/{name}?vfs=memdb"),
            flags | OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        ),
    }
    .map_err(conn_err)?;
    conn.busy_timeout(cfg.busy_timeout).map_err(conn_err)?;
    for sql in cfg.setup_sql() {
        // Pragmas may return a row (journal_mode); step through it.
        conn.prepare(&sql)
            .and_then(|mut s| {
                let mut rows = s.raw_query();
                while rows.next()?.is_some() {}
                Ok(())
            })
            .map_err(|e| OrmError::Connection(format!("{sql}: {e}")))?;
    }
    conn.set_prepared_statement_cache_capacity(64);
    if let (Location::Memory(_), Some(image)) = (&cfg.location, &cfg.image) {
        load_image(&mut conn, image)?;
    }
    install_hooks(&conn, cfg);
    for f in &cfg.functions {
        // rusqlite catches a panic in the body and reports it as an SQL error.
        let body = std::panic::AssertUnwindSafe(std::sync::Arc::clone(&f.body));
        let mut flags = FunctionFlags::SQLITE_UTF8;
        if f.deterministic {
            flags |= FunctionFlags::SQLITE_DETERMINISTIC;
        }
        conn.create_scalar_function(f.name.as_str(), f.args, flags, move |ctx| {
            let args: Vec<Value> = (0..ctx.len()).map(|i| value_of(ctx.get_raw(i), None)).collect();
            // Capture the wrapper whole (not its field) so it stays UnwindSafe.
            let wrapped = &body;
            match (wrapped.0)(&args) {
                Ok(v) => Ok(to_sql(&v)),
                Err(m) => Err(rusqlite::Error::UserFunctionError(m.into())),
            }
        })
        .map_err(|e| OrmError::Connection(format!("register function {}: {e}", f.name)))?;
    }
    Ok(conn)
}

/// Copy a serialized database into `conn` (a fresh in-memory database):
/// the bytes go to a temporary file on this thread and SQLite's backup
/// copies them page by page.
fn load_image(conn: &mut Connection, image: &[u8]) -> Result<(), OrmError> {
    let bad = |e: &dyn std::fmt::Display| OrmError::Connection(format!("load the database image: {e}"));
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tmp = std::env::temp_dir().join(format!("kernway-image-{}-{}.db", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    // Bytes 18–19 of the header are the file format versions: 2 marks a WAL
    // database, which an in-memory database cannot be. Load it as a
    // rollback-journal database (1) instead — the pages are the same.
    let mut image = image.to_vec();
    if image.len() >= 100 && image.starts_with(b"SQLite format 3\0") && image[18] == 2 && image[19] == 2 {
        image[18] = 1;
        image[19] = 1;
    }
    std::fs::write(&tmp, &image).map_err(|e| bad(&e))?;
    let r = Connection::open_with_flags(&tmp, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .and_then(|src| rusqlite::backup::Backup::new(&src, conn)?.run_to_completion(256, std::time::Duration::ZERO, None));
    let _ = std::fs::remove_file(&tmp);
    r.map_err(|e| bad(&e))
}

fn install_hooks(conn: &Connection, cfg: &SqliteConfig) {
    use crate::config::{Change, ChangeKind};
    use rusqlite::hooks::Action;
    if let Some(f) = cfg.hooks.change.clone() {
        conn.update_hook(Some(move |action: Action, db: &str, table: &str, rowid: i64| {
            let kind = match action {
                Action::SQLITE_INSERT => ChangeKind::Insert,
                Action::SQLITE_UPDATE => ChangeKind::Update,
                Action::SQLITE_DELETE => ChangeKind::Delete,
                _ => return,
            };
            f(&Change { kind, database: db.to_string(), table: table.to_string(), rowid });
        }));
    }
    if let Some(f) = cfg.hooks.commit.clone() {
        conn.commit_hook(Some(move || {
            f();
            false // never veto the commit
        }));
    }
    if let Some(f) = cfg.hooks.rollback.clone() {
        conn.rollback_hook(Some(move || f()));
    }
}

/// A SQL value as kernway sees it, refined by the column's declared type:
/// SQLite stores only NULL / integer / real / text / blob, so a `BOOLEAN`,
/// `DATE`, `JSON` … column is recognised by its declaration.
pub(crate) fn value_of(v: ValueRef<'_>, decl: Option<&str>) -> Value {
    let decl = decl.map(str::to_ascii_uppercase);
    let has = |w: &str| decl.as_deref().is_some_and(|d| d.contains(w));
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) if has("BOOL") => Value::Bool(i != 0),
        ValueRef::Integer(i) => Value::Int(i),
        ValueRef::Real(f) => Value::Float(f),
        ValueRef::Blob(b) => Value::Bytes(b.to_vec()),
        ValueRef::Text(t) => match String::from_utf8(t.to_vec()) {
            Err(e) => Value::Bytes(e.into_bytes()),
            Ok(s) if has("JSON") => Value::Json(s),
            Ok(s) if has("DATETIME") || has("TIMESTAMP") => Value::DateTime(s),
            Ok(s) if has("DATE") => Value::Date(s),
            Ok(s) if has("TIME") => Value::Time(s),
            Ok(s) if has("DECIMAL") || has("NUMERIC") => Value::Decimal(s),
            Ok(s) => Value::Text(s),
        },
    }
}

/// A kernway value as an SQLite value (for function results).
fn to_sql(v: &Value) -> ToSqlOutput<'static> {
    ToSqlOutput::Owned(match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Value::Int(i) => SqlValue::Integer(*i),
        Value::UInt(u) => i64::try_from(*u).map_or(SqlValue::Real(*u as f64), SqlValue::Integer),
        Value::Float(f) => SqlValue::Real(*f),
        Value::Bytes(b) => SqlValue::Blob(b.clone()),
        Value::Text(s) | Value::Decimal(s) | Value::Json(s) | Value::Date(s) | Value::Time(s) | Value::DateTime(s) | Value::Interval(s) => {
            SqlValue::Text(s.clone())
        }
        other => SqlValue::Text(format!("{other:?}")),
    })
}

/// A bound parameter. SQLite integers are signed 64-bit: a larger `u64` is
/// refused rather than silently rounded.
fn param_value(p: &Param) -> Result<SqlValue, OrmError> {
    Ok(match p {
        Param::Null => SqlValue::Null,
        Param::Int(i) => SqlValue::Integer(*i),
        Param::UInt(u) => SqlValue::Integer(
            i64::try_from(*u).map_err(|_| OrmError::TypeConversion(format!("{u} does not fit SQLite's 64-bit signed integer")))?,
        ),
        Param::Float(f) => SqlValue::Real(*f),
        Param::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Param::Str(s) => SqlValue::Text(s.clone()),
        Param::Bytes(b) => SqlValue::Blob(b.clone()),
        // No object types: structured values travel as JSON text (usable with
        // SQLite's json_* functions).
        p @ (Param::Array(_) | Param::Object(_)) => SqlValue::Text(p.to_json().to_string()),
    })
}

/// Bind `params` by position, checking the count first.
pub(crate) fn bind(stmt: &mut Statement<'_>, params: &[Param]) -> Result<(), OrmError> {
    let want = stmt.parameter_count();
    if want != params.len() {
        return Err(OrmError::Query(format!("the statement has {want} parameter(s), {} given", params.len())));
    }
    for (i, p) in params.iter().enumerate() {
        stmt.raw_bind_parameter(i + 1, param_value(p)?).map_err(map_err)?;
    }
    Ok(())
}

/// SQLite's error as an [`OrmError`]: constraint kinds by extended code.
pub(crate) fn map_err(e: rusqlite::Error) -> OrmError {
    use rusqlite::ffi;
    if let rusqlite::Error::SqliteFailure(fe, msg) = &e {
        let msg = msg.clone().unwrap_or_else(|| fe.to_string());
        return match fe.extended_code {
            ffi::SQLITE_CONSTRAINT_UNIQUE | ffi::SQLITE_CONSTRAINT_PRIMARYKEY => {
                // "UNIQUE constraint failed: table.col[, table.col2]"
                let cols = msg.rsplit(": ").next().unwrap_or("");
                let first = cols.split(',').next().unwrap_or("").trim();
                let field = first.rsplit('.').next().unwrap_or(first).to_string();
                OrmError::UniqueViolation { field }
            }
            ffi::SQLITE_CONSTRAINT_FOREIGNKEY => OrmError::ForeignKeyViolation,
            _ => OrmError::Query(msg),
        };
    }
    OrmError::Query(e.to_string())
}

impl Ctx<'_> {
    /// Run `sql` once, stepping through any rows; rows changed and the last
    /// inserted rowid.
    pub(crate) fn execute(&self, sql: &str, params: &[Param]) -> Result<(u64, i64), OrmError> {
        let mut stmt = self.conn.prepare_cached(sql).map_err(map_err)?;
        bind(&mut stmt, params)?;
        let before = self.total_changes()?;
        let mut rows = stmt.raw_query();
        while rows.next().map_err(map_err)?.is_some() {}
        drop(rows);
        // `changes()` keeps the count of the last INSERT/UPDATE/DELETE; only
        // report it when this statement changed something.
        let changed = if self.total_changes()? != before { self.conn.changes() } else { 0 };
        Ok((changed, self.conn.last_insert_rowid()))
    }

    /// Open a BLOB for incremental I/O. Table and column are names handed to
    /// SQLite as such, never SQL text.
    pub(crate) fn blob(&self, table: &str, column: &str, rowid: i64, read_only: bool) -> Result<rusqlite::blob::Blob<'_>, OrmError> {
        self.conn.blob_open(rusqlite::DatabaseName::Main, table, column, rowid, read_only).map_err(map_err)
    }

    /// Rows changed on this connection so far (SQL `total_changes()`).
    fn total_changes(&self) -> Result<i64, OrmError> {
        self.conn.prepare_cached("SELECT total_changes()").and_then(|mut s| s.query_row([], |r| r.get(0))).map_err(map_err)
    }

    /// Every row of `sql`, typed by column.
    pub(crate) fn query(&self, sql: &str, params: &[Param]) -> Result<(Vec<String>, Vec<Vec<Value>>), OrmError> {
        let mut stmt = self.conn.prepare_cached(sql).map_err(map_err)?;
        bind(&mut stmt, params)?;
        let (names, decl) = shape(&stmt);
        let mut out = Vec::new();
        let mut rows = stmt.raw_query();
        while let Some(r) = rows.next().map_err(map_err)? {
            out.push(row_values(r, &decl)?);
        }
        Ok((names, out))
    }

    /// Prepare and bind a statement as a cursor; its id and column names.
    pub(crate) fn open_cursor(&mut self, sql: &str, params: &[Param]) -> Result<(u64, Vec<String>), OrmError> {
        let mut stmt = self.conn.prepare(sql).map_err(map_err)?;
        if stmt.column_count() == 0 {
            return Err(OrmError::Query("cursor_open: the statement returns no rows".into()));
        }
        bind(&mut stmt, params)?;
        let (names, decl) = shape(&stmt);
        let id = self.next_cursor;
        self.next_cursor += 1;
        self.cursors.insert(id, Cursor { stmt, decl });
        Ok((id, names))
    }

    /// Step a cursor up to `max` rows; `true` once it is exhausted (it is
    /// then released).
    pub(crate) fn fetch(&mut self, id: u64, max: usize) -> Result<(Vec<Vec<Value>>, bool), OrmError> {
        let Some(cur) = self.cursors.get_mut(&id) else {
            return Err(OrmError::Query("cursor is closed (its transaction ended)".into()));
        };
        let mut out = Vec::new();
        let mut done = false;
        let mut failed = None;
        {
            // `raw_query` neither resets nor rebinds: each batch continues
            // where the previous one stopped.
            let mut rows = cur.stmt.raw_query();
            while out.len() < max.max(1) {
                match rows.next() {
                    Ok(Some(r)) => match row_values(r, &cur.decl) {
                        Ok(v) => out.push(v),
                        Err(e) => {
                            failed = Some(e);
                            break;
                        }
                    },
                    Ok(None) => {
                        done = true;
                        break;
                    }
                    Err(e) => {
                        failed = Some(map_err(e));
                        break;
                    }
                }
            }
            if !done && failed.is_none() {
                // Dropping `Rows` would reset the statement to its first row.
                std::mem::forget(rows);
            }
        }
        if done || failed.is_some() {
            self.cursors.remove(&id);
        }
        match failed {
            Some(e) => Err(e),
            None => Ok((out, done)),
        }
    }
}

fn shape(stmt: &Statement<'_>) -> (Vec<String>, Vec<Option<String>>) {
    stmt.columns().iter().map(|c| (c.name().to_string(), c.decl_type().map(str::to_string))).unzip()
}

fn row_values(r: &rusqlite::Row<'_>, decl: &[Option<String>]) -> Result<Vec<Value>, OrmError> {
    (0..decl.len()).map(|i| Ok(value_of(r.get_ref(i).map_err(map_err)?, decl[i].as_deref()))).collect()
}
