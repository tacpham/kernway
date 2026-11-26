//! The connection's own thread. SQLite is a library — every call does file
//! I/O and may wait on a lock — so a connection lives on a dedicated thread
//! and the async side only sends it work and awaits the reply. The reactor
//! never runs SQLite code.
//!
//! Open cursors are prepared statements kept on this thread between
//! requests, stepped a batch at a time: the result is never materialised.

use crate::config::{Location, OpenMode, SqliteConfig};
use crate::ffi;
use crate::oneshot;
use crate::raw::{self, Db, Interrupt, Raw, Stmt};
use kernway_orm_core::error::OrmError;
use kernway_orm_sql::{Param, Value};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

/// The least time opening a connection waits for a lock during setup.
const SETUP_BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Prepared statements kept per connection.
const STATEMENT_CACHE: usize = 64;

/// What a job sees: the connection and its open cursors.
pub(crate) struct Ctx<'c> {
    pub(crate) conn: &'c Db,
    pub(crate) cursors: HashMap<u64, Cursor>,
    pub(crate) next_cursor: u64,
    /// Change-capture sessions: `(session, armed)` — armed until the
    /// transaction it was started for begins.
    pub(crate) sessions: HashMap<u64, (raw::Session, bool)>,
    cache: RefCell<StmtCache>,
}

/// A statement being stepped across requests.
pub(crate) struct Cursor {
    pub(crate) stmt: Stmt,
    pub(crate) decl: Vec<Option<String>>,
}

/// Prepared statements by SQL text, oldest evicted first.
#[derive(Default)]
struct StmtCache {
    by_sql: HashMap<String, Stmt>,
    order: VecDeque<String>,
}

pub(crate) type Job = Box<dyn for<'c> FnOnce(&mut Ctx<'c>) + Send>;

static NEXT_THREAD: AtomicU64 = AtomicU64::new(0);

/// Start a connection thread, open the database on it and set it up. The
/// returned sender submits jobs; dropping it ends the thread and closes the
/// connection.
pub(crate) async fn spawn(cfg: SqliteConfig) -> Result<(mpsc::Sender<Job>, Arc<Interrupt>), OrmError> {
    let (tx, rx) = mpsc::channel::<Job>();
    let (ready_tx, ready_rx) = oneshot::channel::<Result<Arc<Interrupt>, OrmError>>();
    let name = format!("kernway-sqlite-{}", NEXT_THREAD.fetch_add(1, Ordering::Relaxed));
    std::thread::Builder::new()
        .name(name)
        .spawn(move || {
            let conn = match open(&cfg) {
                Ok(c) => c,
                Err(e) => return ready_tx.send(Err(e)),
            };
            ready_tx.send(Ok(conn.interrupt_handle()));
            let mut ctx = Ctx { conn: &conn, cursors: HashMap::new(), next_cursor: 1, sessions: HashMap::new(), cache: RefCell::default() };
            while let Ok(job) = rx.recv() {
                job(&mut ctx);
            }
            // Statements and sessions first, then the connection.
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

fn open(cfg: &SqliteConfig) -> Result<Db, OrmError> {
    let conn_err = |e: raw::Error| OrmError::Connection(e.to_string());
    let mut flags = ffi::SQLITE_OPEN_NOMUTEX;
    flags |= match cfg.mode {
        OpenMode::ReadWriteCreate => ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE,
        OpenMode::ReadWrite => ffi::SQLITE_OPEN_READWRITE,
        OpenMode::ReadOnly => ffi::SQLITE_OPEN_READONLY,
    };
    let mut conn = match &cfg.location {
        // A path is opened as a path, not a URI, so `?` / `#` in a file name are literal.
        Location::File(p) => {
            let path = p.to_str().ok_or_else(|| OrmError::Connection(format!("{} is not valid UTF-8", p.display())))?;
            Db::open(path, flags)
        }
        // memdb: one shared in-memory database for every connection that names it.
        Location::Memory(name) => Db::open(
            &format!("file:/{name}?vfs=memdb"),
            flags | ffi::SQLITE_OPEN_URI | ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE,
        ),
    }
    .map_err(conn_err)?;
    // Setup may meet a lock another connection holds for a moment (WAL
    // recovery on first open, a journal-mode change): wait for it at least
    // a few seconds on this thread whatever the configured busy timeout,
    // which applies from the first statement on.
    conn.busy_timeout(cfg.busy_timeout.max(SETUP_BUSY_TIMEOUT).as_millis()).map_err(conn_err)?;
    for sql in cfg.setup_sql() {
        if let Some(want) = sql.strip_prefix("PRAGMA journal_mode = ") {
            set_journal_mode(&conn, want).map_err(|e| OrmError::Connection(format!("{sql}: {e}")))?;
            continue;
        }
        let run = || -> Result<(), raw::Error> {
            let mut stmt = conn.prepare(&sql, false)?;
            while stmt.step()? {}
            Ok(())
        };
        run().map_err(|e| OrmError::Connection(format!("{sql}: {e}")))?;
    }
    if let (Location::Memory(_), Some(image)) = (&cfg.location, &cfg.image) {
        load_image(&conn, image)?;
    }
    conn.busy_timeout(cfg.busy_timeout.as_millis()).map_err(conn_err)?;
    install_hooks(&mut conn, cfg);
    for (path, entry) in &cfg.extensions {
        // SAFETY: `SqliteConfig::extension` is unsafe; its caller vouched for
        // the library. Loading is enabled only around this call, so SQL's own
        // load_extension() stays refused.
        unsafe { conn.load_extension(path, entry.as_deref()) }
            .map_err(|e| OrmError::Connection(format!("load extension {}: {e}", path.display())))?;
    }
    for f in &cfg.functions {
        conn.create_function(&f.name, f.args, f.deterministic, Arc::clone(&f.body))
            .map_err(|e| OrmError::Connection(format!("register function {}: {e}", f.name)))?;
    }
    Ok(conn)
}

/// Switch the journal mode to `want`. It is often already set (a file opened
/// before), which needs no lock. Switching needs the database to itself, and
/// SQLite answers "locked" at once rather than waiting — several connections
/// opening a fresh file together all try — so retry until the mode is in
/// place (by us or by one of them) or the setup time is up.
fn set_journal_mode(conn: &Db, want: &str) -> Result<(), raw::Error> {
    let current = || -> Result<String, raw::Error> {
        let mut stmt = conn.prepare("PRAGMA journal_mode", false)?;
        Ok(match stmt.step()?.then(|| stmt.column(0)) {
            Some(Raw::Text(m)) => String::from_utf8_lossy(&m).into_owned(),
            _ => String::new(),
        })
    };
    let deadline = std::time::Instant::now() + SETUP_BUSY_TIMEOUT;
    loop {
        if current()?.eq_ignore_ascii_case(want) {
            return Ok(());
        }
        let attempt = (|| {
            let mut stmt = conn.prepare(&format!("PRAGMA journal_mode = {want}"), false)?;
            while stmt.step()? {}
            Ok::<(), raw::Error>(())
        })();
        let busy = matches!(&attempt, Err(e) if matches!(e.code & 0xff, ffi::SQLITE_BUSY | ffi::SQLITE_LOCKED));
        if let Err(e) = attempt {
            if !busy {
                return Err(e);
            }
        }
        if std::time::Instant::now() >= deadline {
            // Still not switched: SQLite kept refusing (or keeps another mode
            // for this kind of database) — say which mode it stayed in.
            let now = current()?;
            if now.eq_ignore_ascii_case(want) {
                return Ok(());
            }
            return Err(raw::Error { code: ffi::SQLITE_BUSY, message: format!("database is locked; journal mode stayed {now}") });
        }
        if busy {
            std::thread::sleep(std::time::Duration::from_millis(5));
        } else {
            // Accepted but not applied (a mode this database cannot use):
            // nothing will change by retrying.
            let now = current()?;
            if now.eq_ignore_ascii_case(want) {
                return Ok(());
            }
            return Err(raw::Error { code: ffi::SQLITE_ERROR, message: format!("journal mode {want} not available here; it is {now}") });
        }
    }
}

/// Copy a serialized database into `conn` (a fresh in-memory database):
/// deserialized into a private connection, then copied page by page with
/// SQLite's backup into the shared one.
fn load_image(conn: &Db, image: &[u8]) -> Result<(), OrmError> {
    let bad = |e: raw::Error| OrmError::Connection(format!("load the database image: {e}"));
    // Bytes 18–19 of the header are the file format versions: 2 marks a WAL
    // database, which an in-memory database cannot be. Load it as a
    // rollback-journal database (1) instead — the pages are the same.
    let mut image = image.to_vec();
    if image.len() >= 100 && image.starts_with(b"SQLite format 3\0") && image[18] == 2 && image[19] == 2 {
        image[18] = 1;
        image[19] = 1;
    }
    let private = Db::open(":memory:", ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE).map_err(bad)?;
    private.deserialize(&image).map_err(bad)?;
    conn.restore_from(&private).map_err(bad)
}

fn install_hooks(conn: &mut Db, cfg: &SqliteConfig) {
    use crate::config::{Change, ChangeKind};
    if let Some(f) = cfg.hooks.change.clone() {
        conn.update_hook(Box::new(move |op, db, table, rowid| {
            let kind = match op {
                ffi::SQLITE_INSERT => ChangeKind::Insert,
                ffi::SQLITE_UPDATE => ChangeKind::Update,
                ffi::SQLITE_DELETE => ChangeKind::Delete,
                _ => return,
            };
            f(&Change { kind, database: db.to_string(), table: table.to_string(), rowid });
        }));
    }
    if let Some(f) = cfg.hooks.commit.clone() {
        conn.commit_hook(Box::new(move || f()));
    }
    if let Some(f) = cfg.hooks.rollback.clone() {
        conn.rollback_hook(Box::new(move || f()));
    }
}

/// A SQL value as kernway sees it, refined by the column's declared type:
/// SQLite stores only NULL / integer / real / text / blob, so a `BOOLEAN`,
/// `DATE`, `JSON` … column is recognised by its declaration.
pub(crate) fn value_of(v: Raw, decl: Option<&str>) -> Value {
    let decl = decl.map(str::to_ascii_uppercase);
    let has = |w: &str| decl.as_deref().is_some_and(|d| d.contains(w));
    match v {
        Raw::Null => Value::Null,
        Raw::Int(i) if has("BOOL") => Value::Bool(i != 0),
        Raw::Int(i) => Value::Int(i),
        Raw::Real(f) => Value::Float(f),
        Raw::Blob(b) => Value::Bytes(b),
        Raw::Text(t) => match String::from_utf8(t) {
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

/// A bound parameter. SQLite integers are signed 64-bit: a larger `u64` is
/// refused rather than silently rounded.
fn param_value(p: &Param) -> Result<Raw, OrmError> {
    Ok(match p {
        Param::Null => Raw::Null,
        Param::Int(i) => Raw::Int(*i),
        Param::UInt(u) => Raw::Int(
            i64::try_from(*u).map_err(|_| OrmError::TypeConversion(format!("{u} does not fit SQLite's 64-bit signed integer")))?,
        ),
        Param::Float(f) => Raw::Real(*f),
        Param::Bool(b) => Raw::Int(i64::from(*b)),
        Param::Str(s) => Raw::Text(s.as_bytes().to_vec()),
        Param::Bytes(b) => Raw::Blob(b.clone()),
        // No object types: structured values travel as JSON text (usable with
        // SQLite's json_* functions).
        p @ (Param::Array(_) | Param::Object(_)) => Raw::Text(p.to_json().to_string().into_bytes()),
    })
}

/// Bind `params` by position, checking the count first.
pub(crate) fn bind(stmt: &mut Stmt, params: &[Param]) -> Result<(), OrmError> {
    let want = stmt.parameter_count();
    if want != params.len() {
        return Err(OrmError::Query(format!("the statement has {want} parameter(s), {} given", params.len())));
    }
    for (i, p) in params.iter().enumerate() {
        stmt.bind(i + 1, &param_value(p)?).map_err(map_err)?;
    }
    Ok(())
}

/// SQLite's error as an [`OrmError`]: constraint kinds by extended code.
pub(crate) fn map_err(e: raw::Error) -> OrmError {
    match e.code {
        ffi::SQLITE_CONSTRAINT_UNIQUE | ffi::SQLITE_CONSTRAINT_PRIMARYKEY => {
            // "UNIQUE constraint failed: table.col[, table.col2]"
            let cols = e.message.rsplit(": ").next().unwrap_or("");
            let first = cols.split(',').next().unwrap_or("").trim();
            let field = first.rsplit('.').next().unwrap_or(first).to_string();
            OrmError::UniqueViolation { field }
        }
        ffi::SQLITE_CONSTRAINT_FOREIGNKEY => OrmError::ForeignKeyViolation,
        _ => OrmError::Query(e.message),
    }
}

impl Ctx<'_> {
    /// Run `f` on the cached statement for `sql` (prepared on first use),
    /// then reset it and clear its bindings for the next caller.
    fn with_cached<R>(&self, sql: &str, f: impl FnOnce(&mut Stmt) -> Result<R, OrmError>) -> Result<R, OrmError> {
        let cached = self.cache.borrow_mut().by_sql.remove(sql);
        let mut stmt = match cached {
            Some(s) => s,
            None => self.conn.prepare(sql, true).map_err(map_err)?,
        };
        let r = f(&mut stmt);
        stmt.reset();
        stmt.clear_bindings();
        let mut cache = self.cache.borrow_mut();
        if !cache.order.iter().any(|k| k == sql) {
            cache.order.push_back(sql.to_string());
        }
        cache.by_sql.insert(sql.to_string(), stmt);
        while cache.order.len() > STATEMENT_CACHE {
            if let Some(old) = cache.order.pop_front() {
                cache.by_sql.remove(&old);
            }
        }
        r
    }

    /// Run `sql` once, stepping through any rows; rows changed and the last
    /// inserted rowid.
    pub(crate) fn execute(&self, sql: &str, params: &[Param]) -> Result<(u64, i64), OrmError> {
        let before = self.conn.total_changes();
        self.with_cached(sql, |stmt| {
            bind(stmt, params)?;
            while stmt.step().map_err(map_err)? {}
            Ok(())
        })?;
        // `changes()` keeps the count of the last INSERT/UPDATE/DELETE; only
        // report it when this statement changed something.
        let changed = if self.conn.total_changes() != before { self.conn.changes() } else { 0 };
        Ok((changed, self.conn.last_insert_rowid()))
    }

    /// The number of result columns of `sql` (0 for a statement without rows).
    pub(crate) fn column_count(&self, sql: &str) -> Result<usize, OrmError> {
        self.with_cached(sql, |stmt| Ok(stmt.column_count()))
    }

    /// Run a script of several statements.
    pub(crate) fn exec(&self, sql: &str) -> Result<(), OrmError> {
        self.conn.exec(sql).map_err(map_err)
    }

    /// Open a BLOB for incremental I/O. Table and column are names handed to
    /// SQLite as such, never SQL text.
    pub(crate) fn blob(&self, table: &str, column: &str, rowid: i64, read_only: bool) -> Result<raw::Blob, OrmError> {
        self.conn.blob(table, column, rowid, !read_only).map_err(map_err)
    }

    /// Start recording changes to `tables` (all tables when empty) for the
    /// transaction about to begin on this connection.
    pub(crate) fn start_session(&mut self, tables: &[String]) -> Result<u64, OrmError> {
        let session = self.conn.session(tables).map_err(map_err)?;
        let id = self.next_cursor;
        self.next_cursor += 1;
        self.sessions.insert(id, (session, true));
        Ok(id)
    }

    /// A transaction begins: the armed sessions now belong to it; any other
    /// session is a leftover of an abandoned capture and is dropped.
    pub(crate) fn sessions_for_new_transaction(&mut self) {
        self.sessions.retain(|_, (_, armed)| *armed);
        for (_, armed) in self.sessions.values_mut() {
            *armed = false;
        }
    }

    /// Stop a session and return what it recorded.
    pub(crate) fn finish_session(&mut self, id: u64, format: crate::ChangeFormat) -> Result<Vec<u8>, OrmError> {
        let (mut session, _) = self.sessions.remove(&id).ok_or_else(|| OrmError::Query("change session is gone".into()))?;
        session.changes(format == crate::ChangeFormat::Patchset).map_err(map_err)
    }

    /// Apply a changeset; the number of conflicts met.
    pub(crate) fn apply(&self, bytes: &[u8], policy: crate::OnConflict) -> Result<u64, OrmError> {
        let conflicts = std::cell::Cell::new(0u64);
        let handler = |kind: std::os::raw::c_int| {
            conflicts.set(conflicts.get() + 1);
            crate::changeset::resolve(policy, kind)
        };
        self.conn.apply_changeset(bytes, &handler).map_err(|e| match map_err(e) {
            OrmError::Query(m) if policy == crate::OnConflict::Abort => OrmError::Query(format!("changeset conflict, nothing applied: {m}")),
            other => other,
        })?;
        Ok(conflicts.get())
    }

    /// Every row of `sql`, typed by column.
    pub(crate) fn query(&self, sql: &str, params: &[Param]) -> Result<(Vec<String>, Vec<Vec<Value>>), OrmError> {
        self.with_cached(sql, |stmt| {
            bind(stmt, params)?;
            let (names, decl) = shape(stmt);
            let mut out = Vec::new();
            while stmt.step().map_err(map_err)? {
                out.push(row_values(stmt, &decl));
            }
            Ok((names, out))
        })
    }

    /// Prepare and bind a statement as a cursor; its id and column names.
    pub(crate) fn open_cursor(&mut self, sql: &str, params: &[Param]) -> Result<(u64, Vec<String>), OrmError> {
        let mut stmt = self.conn.prepare(sql, false).map_err(map_err)?;
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
    /// then released). The statement stays where it stopped between batches.
    pub(crate) fn fetch(&mut self, id: u64, max: usize) -> Result<(Vec<Vec<Value>>, bool), OrmError> {
        let Some(cur) = self.cursors.get_mut(&id) else {
            return Err(OrmError::Query("cursor is closed (its transaction ended)".into()));
        };
        let mut out = Vec::new();
        let mut done = false;
        let mut failed = None;
        while out.len() < max.max(1) {
            match cur.stmt.step() {
                Ok(true) => out.push(row_values(&cur.stmt, &cur.decl)),
                Ok(false) => {
                    done = true;
                    break;
                }
                Err(e) => {
                    failed = Some(map_err(e));
                    break;
                }
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

fn shape(stmt: &Stmt) -> (Vec<String>, Vec<Option<String>>) {
    (0..stmt.column_count()).map(|i| (stmt.column_name(i), stmt.column_decltype(i))).unzip()
}

fn row_values(stmt: &Stmt, decl: &[Option<String>]) -> Vec<Value> {
    (0..decl.len()).map(|i| value_of(stmt.column(i), decl[i].as_deref())).collect()
}
