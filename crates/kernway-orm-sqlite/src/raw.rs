//! Safe wrappers over SQLite's C API ([`crate::ffi`]): a connection, its
//! prepared statements, SQL functions written in Rust, hooks, incremental
//! BLOBs, backups, serialization, extensions and change sessions.
//!
//! Every `unsafe` block of the driver is in this file. The invariants they
//! rely on:
//!
//! - A [`Db`] owns its `sqlite3*` and is used from one thread (the
//!   connection's worker); it is closed once, in `Drop`, with
//!   `sqlite3_close_v2`, which waits for statements, BLOBs and sessions still
//!   open to be released ("zombie" mode) — so dropping them after the `Db` is
//!   memory-safe too.
//! - Statements, BLOBs, backups, sessions and changeset iterators are owned
//!   wrappers that release their handle exactly once in `Drop`.
//! - Pointers SQLite returns (text, blobs, names) are copied into Rust memory
//!   before the next call on the same object can invalidate them.
//! - Callbacks registered with SQLite (functions, hooks, conflict handlers)
//!   never unwind into C: a panic is caught and reported as an SQL error (or
//!   ignored for notifications).
//! - The only cross-thread call is `sqlite3_interrupt`, documented
//!   thread-safe; [`Interrupt`] clears its pointer under a lock before the
//!   connection closes.

use crate::ffi;
use kernway_orm_sql::Value;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::{Arc, Mutex};

/// An SQLite error: the extended result code and its message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Error {
    pub(crate) code: c_int,
    pub(crate) message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

impl Error {
    fn misuse(message: impl Into<String>) -> Self {
        Self { code: ffi::SQLITE_MISUSE, message: message.into() }
    }

    /// The generic message of a result code.
    fn from_code(code: c_int) -> Self {
        // SAFETY: sqlite3_errstr returns a static string for any code.
        let message = unsafe { text(ffi::sqlite3_errstr(code)) }.unwrap_or_default();
        Self { code, message }
    }
}

/// Copy a NUL-terminated C string (`None` for NULL).
///
/// # Safety
/// `p` is NULL or points to a NUL-terminated string valid for this call.
unsafe fn text(p: *const c_char) -> Option<String> {
    if p.is_null() {
        None
    } else {
        // SAFETY: per the contract above.
        Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
    }
}

fn cstring(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| Error::misuse(format!("{s:?} contains a NUL byte")))
}

fn path_cstring(p: &Path) -> Result<CString> {
    let s = p.to_str().ok_or_else(|| Error::misuse(format!("{} is not valid UTF-8", p.display())))?;
    cstring(s)
}

/// Copy `n` bytes at `p` (empty for NULL or `n <= 0`).
///
/// # Safety
/// `p` is NULL or valid for `n` bytes.
unsafe fn bytes(p: *const c_void, n: c_int) -> Vec<u8> {
    if p.is_null() || n <= 0 {
        Vec::new()
    } else {
        // SAFETY: per the contract above.
        unsafe { std::slice::from_raw_parts(p.cast::<u8>(), n as usize) }.to_vec()
    }
}

/// The SQLite engine version compiled in.
pub(crate) fn version() -> String {
    // SAFETY: a static string.
    unsafe { text(ffi::sqlite3_libversion()) }.unwrap_or_default()
}

/// A value as SQLite stores it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Raw {
    Null,
    Int(i64),
    Real(f64),
    Text(Vec<u8>),
    Blob(Vec<u8>),
}

// ---------------------------------------------------------------------------
// Connection

/// Interrupts the statement running on a connection, from any thread.
pub(crate) struct Interrupt {
    db: Mutex<*mut ffi::sqlite3>,
}

// SAFETY: the pointer is only used for sqlite3_interrupt, which SQLite
// documents as safe to call from any thread while the connection is open;
// `Db::drop` nulls it under the lock before closing.
unsafe impl Send for Interrupt {}
// SAFETY: as above; access is serialised by the mutex.
unsafe impl Sync for Interrupt {}

impl Interrupt {
    pub(crate) fn interrupt(&self) {
        let db = self.db.lock().unwrap_or_else(|e| e.into_inner());
        if !db.is_null() {
            // SAFETY: open (non-null) connection, see above.
            unsafe { ffi::sqlite3_interrupt(*db) };
        }
    }
}

type ChangeFn = Box<dyn FnMut(c_int, &str, &str, i64)>;
type CommitFn = Box<dyn FnMut()>;

/// One SQLite connection.
pub(crate) struct Db {
    db: NonNull<ffi::sqlite3>,
    interrupt: Arc<Interrupt>,
    // Hook closures, freed after the connection closes.
    on_change: Option<Box<ChangeFn>>,
    on_commit: Option<Box<CommitFn>>,
    on_rollback: Option<Box<CommitFn>>,
}

impl Drop for Db {
    fn drop(&mut self) {
        *self.interrupt.db.lock().unwrap_or_else(|e| e.into_inner()) = ptr::null_mut();
        // SAFETY: closed exactly once; v2 defers the close until statements,
        // BLOBs, backups and sessions still alive are released.
        unsafe { ffi::sqlite3_close_v2(self.db.as_ptr()) };
        // The hook boxes outlive every call into them: SQLite cannot call a
        // hook of a closed connection.
    }
}

impl Db {
    /// Open `filename` (a path, or a `file:` URI when `flags` has
    /// `SQLITE_OPEN_URI`) with extended result codes.
    pub(crate) fn open(filename: &str, flags: c_int) -> Result<Db> {
        let name = cstring(filename)?;
        let mut db = ptr::null_mut();
        // SAFETY: valid C string and out-pointer.
        let rc = unsafe { ffi::sqlite3_open_v2(name.as_ptr(), &mut db, flags | ffi::SQLITE_OPEN_EXRESCODE, ptr::null()) };
        let Some(handle) = NonNull::new(db) else {
            return Err(Error::from_code(rc)); // out of memory
        };
        let conn = Db {
            db: handle,
            interrupt: Arc::new(Interrupt { db: Mutex::new(db) }),
            on_change: None,
            on_commit: None,
            on_rollback: None,
        };
        if rc != ffi::SQLITE_OK {
            // The handle exists even on failure; it carries the message and
            // must be closed (by dropping `conn`).
            return Err(conn.error(rc));
        }
        Ok(conn)
    }

    fn ptr(&self) -> *mut ffi::sqlite3 {
        self.db.as_ptr()
    }

    /// The connection's last error, for result code `rc`.
    fn error(&self, rc: c_int) -> Error {
        // SAFETY: open connection; the message is copied at once.
        let (code, message) = unsafe { (ffi::sqlite3_extended_errcode(self.ptr()), text(ffi::sqlite3_errmsg(self.ptr()))) };
        let code = if code == ffi::SQLITE_OK { rc } else { code };
        Error { code, message: message.unwrap_or_else(|| Error::from_code(rc).message) }
    }

    fn check(&self, rc: c_int) -> Result<()> {
        if rc == ffi::SQLITE_OK {
            Ok(())
        } else {
            Err(self.error(rc))
        }
    }

    pub(crate) fn interrupt_handle(&self) -> Arc<Interrupt> {
        Arc::clone(&self.interrupt)
    }

    pub(crate) fn busy_timeout(&self, ms: u128) -> Result<()> {
        // SAFETY: open connection.
        self.check(unsafe { ffi::sqlite3_busy_timeout(self.ptr(), ms.min(c_int::MAX as u128) as c_int) })
    }

    /// Prepare one statement. Empty SQL, or a second statement after the
    /// first, is refused (use [`exec`](Self::exec) for scripts).
    pub(crate) fn prepare(&self, sql: &str, persistent: bool) -> Result<Stmt> {
        let len = c_int::try_from(sql.len()).map_err(|_| Error::misuse("SQL text too long"))?;
        let mut stmt = ptr::null_mut();
        let mut tail: *const c_char = ptr::null();
        let flags = if persistent { ffi::SQLITE_PREPARE_PERSISTENT } else { 0 };
        // SAFETY: `sql` is valid for `len` bytes (no NUL needed with a length).
        let rc = unsafe { ffi::sqlite3_prepare_v3(self.ptr(), sql.as_ptr().cast(), len, flags, &mut stmt, &mut tail) };
        self.check(rc)?;
        let Some(stmt) = NonNull::new(stmt) else {
            return Err(Error::misuse("empty SQL statement"));
        };
        let stmt = Stmt { stmt, db: self.ptr() };
        // Anything after the first statement must be blank or comments.
        // SAFETY: `tail` points into `sql`.
        let used = unsafe { tail.offset_from(sql.as_ptr().cast()) } as usize;
        let rest = &sql[used.min(sql.len())..];
        if !rest.trim().is_empty() {
            let mut next = ptr::null_mut();
            let rest_len = rest.len() as c_int;
            // SAFETY: as above; the probe statement is finalized at once.
            let rc = unsafe { ffi::sqlite3_prepare_v3(self.ptr(), rest.as_ptr().cast(), rest_len, 0, &mut next, ptr::null_mut()) };
            if !next.is_null() {
                // SAFETY: just prepared, finalized once.
                unsafe { ffi::sqlite3_finalize(next) };
                return Err(Error::misuse("several statements in one call: use execute_script"));
            }
            self.check(rc)?;
        }
        Ok(stmt)
    }

    /// Run every statement of `sql` (a script), stopping at the first error.
    pub(crate) fn exec(&self, sql: &str) -> Result<()> {
        let sql = cstring(sql)?;
        let mut err: *mut c_char = ptr::null_mut();
        // SAFETY: valid C string; the error message is copied and freed.
        let rc = unsafe { ffi::sqlite3_exec(self.ptr(), sql.as_ptr(), ptr::null(), ptr::null_mut(), &mut err) };
        if rc == ffi::SQLITE_OK {
            return Ok(());
        }
        let mut e = self.error(rc);
        if !err.is_null() {
            // SAFETY: SQLite-allocated message, freed once.
            unsafe {
                e.message = text(err).unwrap_or(e.message);
                ffi::sqlite3_free(err.cast());
            }
        }
        Err(e)
    }

    pub(crate) fn changes(&self) -> u64 {
        // SAFETY: open connection.
        unsafe { ffi::sqlite3_changes64(self.ptr()) }.max(0) as u64
    }

    pub(crate) fn total_changes(&self) -> i64 {
        // SAFETY: open connection.
        unsafe { ffi::sqlite3_total_changes64(self.ptr()) }
    }

    pub(crate) fn last_insert_rowid(&self) -> i64 {
        // SAFETY: open connection.
        unsafe { ffi::sqlite3_last_insert_rowid(self.ptr()) }
    }

    /// Register `body` as SQL function `name` with `args` arguments (-1: any).
    pub(crate) fn create_function(
        &self,
        name: &str,
        args: i32,
        deterministic: bool,
        body: Arc<crate::config::FunctionBody>,
    ) -> Result<()> {
        let cname = cstring(name)?;
        let mut flags = ffi::SQLITE_UTF8;
        if deterministic {
            flags |= ffi::SQLITE_DETERMINISTIC;
        }
        let data = Box::into_raw(Box::new(body)).cast::<c_void>();
        // SAFETY: `data` is a leaked Box<Arc<FunctionBody>> that SQLite hands
        // back to `call_function` and frees through `drop_function` (also on
        // failure, per the documentation of create_function_v2).
        let rc = unsafe {
            ffi::sqlite3_create_function_v2(self.ptr(), cname.as_ptr(), args, flags, data, Some(call_function), None, None, Some(drop_function))
        };
        self.check(rc)
    }

    /// Call `f(op, database, table, rowid)` for every row change.
    pub(crate) fn update_hook(&mut self, f: ChangeFn) {
        let mut boxed = Box::new(f);
        let arg = ptr::addr_of_mut!(*boxed).cast::<c_void>();
        // SAFETY: `arg` points into a box this Db keeps until after close.
        unsafe { ffi::sqlite3_update_hook(self.ptr(), Some(on_update), arg) };
        self.on_change = Some(boxed);
    }

    /// Call `f` after each commit.
    pub(crate) fn commit_hook(&mut self, f: CommitFn) {
        let mut boxed = Box::new(f);
        let arg = ptr::addr_of_mut!(*boxed).cast::<c_void>();
        // SAFETY: as for update_hook.
        unsafe { ffi::sqlite3_commit_hook(self.ptr(), Some(on_commit), arg) };
        self.on_commit = Some(boxed);
    }

    /// Call `f` after each rollback.
    pub(crate) fn rollback_hook(&mut self, f: CommitFn) {
        let mut boxed = Box::new(f);
        let arg = ptr::addr_of_mut!(*boxed).cast::<c_void>();
        // SAFETY: as for update_hook.
        unsafe { ffi::sqlite3_rollback_hook(self.ptr(), Some(on_rollback), arg) };
        self.on_rollback = Some(boxed);
    }

    /// Copy the whole `main` database of `src` over this one's.
    pub(crate) fn restore_from(&self, src: &Db) -> Result<()> {
        let main = c"main";
        // SAFETY: two open connections; the backup is finished before return.
        unsafe {
            let b = ffi::sqlite3_backup_init(self.ptr(), main.as_ptr(), src.ptr(), main.as_ptr());
            if b.is_null() {
                return Err(self.error(ffi::SQLITE_ERROR));
            }
            let mut rc;
            loop {
                rc = ffi::sqlite3_backup_step(b, 256);
                if rc != ffi::SQLITE_OK && rc != ffi::SQLITE_BUSY && rc != ffi::SQLITE_LOCKED {
                    break;
                }
                if rc != ffi::SQLITE_OK {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
            let finish = ffi::sqlite3_backup_finish(b);
            if rc != ffi::SQLITE_DONE {
                return Err(Error { code: rc, ..self.error(rc) });
            }
            self.check(finish)
        }
    }

    /// Write a consistent copy of the database to a new file at `path`.
    pub(crate) fn backup_to(&self, path: &Path) -> Result<()> {
        let path = path.to_str().ok_or_else(|| Error::misuse("backup path is not valid UTF-8"))?;
        let dest = Db::open(path, ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE)?;
        dest.restore_from(self)
    }

    /// The `main` database as bytes (a database file image).
    pub(crate) fn serialize(&self) -> Result<Vec<u8>> {
        let mut size: i64 = 0;
        // SAFETY: returns a buffer from sqlite3_malloc (flags 0), copied and freed.
        unsafe {
            let p = ffi::sqlite3_serialize(self.ptr(), c"main".as_ptr(), &mut size, 0);
            if p.is_null() {
                return if size == 0 { Ok(Vec::new()) } else { Err(self.error(ffi::SQLITE_ERROR)) };
            }
            let out = std::slice::from_raw_parts(p, size as usize).to_vec();
            ffi::sqlite3_free(p.cast());
            Ok(out)
        }
    }

    /// Replace the `main` database with `image` (kept in SQLite-owned memory).
    pub(crate) fn deserialize(&self, image: &[u8]) -> Result<()> {
        let n = image.len();
        // SAFETY: the buffer is allocated with sqlite3_malloc64 and handed
        // over with FREEONCLOSE, so SQLite frees it (also on failure).
        unsafe {
            let buf = ffi::sqlite3_malloc64(n.max(1) as u64).cast::<u8>();
            if buf.is_null() {
                return Err(Error::from_code(7)); // SQLITE_NOMEM
            }
            ptr::copy_nonoverlapping(image.as_ptr(), buf, n);
            let flags = ffi::SQLITE_DESERIALIZE_FREEONCLOSE | ffi::SQLITE_DESERIALIZE_RESIZEABLE;
            self.check(ffi::sqlite3_deserialize(self.ptr(), c"main".as_ptr(), buf, n as i64, n as i64, flags))
        }
    }

    /// Open a BLOB for incremental I/O (`write`: read-write).
    pub(crate) fn blob(&self, table: &str, column: &str, rowid: i64, write: bool) -> Result<Blob> {
        let (t, c) = (cstring(table)?, cstring(column)?);
        let mut b = ptr::null_mut();
        // SAFETY: valid strings and out-pointer.
        let rc = unsafe { ffi::sqlite3_blob_open(self.ptr(), c"main".as_ptr(), t.as_ptr(), c.as_ptr(), rowid, c_int::from(write), &mut b) };
        self.check(rc)?;
        NonNull::new(b).map(|b| Blob { b, db: self.ptr() }).ok_or_else(|| self.error(ffi::SQLITE_ERROR))
    }

    /// Load the extension library at `path`; loading stays enabled only for
    /// this call.
    ///
    /// # Safety
    /// The library runs native code in this process.
    pub(crate) unsafe fn load_extension(&self, path: &Path, entry: Option<&str>) -> Result<()> {
        let file = path_cstring(path)?;
        let entry = entry.map(cstring).transpose()?;
        // SAFETY: per the function's contract; the message is copied and freed.
        unsafe {
            self.check(ffi::sqlite3_enable_load_extension(self.ptr(), 1))?;
            let mut err: *mut c_char = ptr::null_mut();
            let rc = ffi::sqlite3_load_extension(self.ptr(), file.as_ptr(), entry.as_ref().map_or(ptr::null(), |e| e.as_ptr()), &mut err);
            ffi::sqlite3_enable_load_extension(self.ptr(), 0);
            if rc == ffi::SQLITE_OK {
                return Ok(());
            }
            let message = text(err).unwrap_or_else(|| Error::from_code(rc).message);
            ffi::sqlite3_free(err.cast());
            Err(Error { code: rc, message })
        }
    }

    /// Start recording changes to `tables` (all when empty).
    pub(crate) fn session(&self, tables: &[String]) -> Result<Session> {
        let mut s = ptr::null_mut();
        // SAFETY: open connection, valid out-pointer.
        self.check(unsafe { ffi::sqlite3session_create(self.ptr(), c"main".as_ptr(), &mut s) })?;
        let s = Session { s: NonNull::new(s).ok_or_else(|| self.error(ffi::SQLITE_ERROR))? };
        if tables.is_empty() {
            // SAFETY: live session; NULL means every table.
            self.check(unsafe { ffi::sqlite3session_attach(s.s.as_ptr(), ptr::null()) })?;
        }
        for t in tables {
            let t = cstring(t)?;
            // SAFETY: live session, valid string.
            self.check(unsafe { ffi::sqlite3session_attach(s.s.as_ptr(), t.as_ptr()) })?;
        }
        Ok(s)
    }

    /// Apply a changeset; `conflict(kind)` chooses the action for each
    /// conflict.
    pub(crate) fn apply_changeset(&self, changeset: &[u8], conflict: &dyn Fn(c_int) -> c_int) -> Result<()> {
        let n = c_int::try_from(changeset.len()).map_err(|_| Error::misuse("changeset too large"))?;
        let mut handler: &dyn Fn(c_int) -> c_int = conflict;
        // SAFETY: SQLite only reads the changeset (the API takes `void*` for
        // historical reasons); the context points to `handler`, alive for
        // the whole call.
        let rc = unsafe {
            ffi::sqlite3changeset_apply(
                self.ptr(),
                n,
                changeset.as_ptr().cast_mut().cast(),
                None,
                Some(on_conflict),
                ptr::addr_of_mut!(handler).cast(),
            )
        };
        self.check(rc)
    }
}

// --- callbacks (never unwind into C) ----------------------------------------

unsafe extern "C" fn call_function(ctx: *mut ffi::sqlite3_context, argc: c_int, argv: *mut *mut ffi::sqlite3_value) {
    // SAFETY: user data is the Box<Arc<FunctionBody>> given at registration;
    // argv holds argc values valid for this call.
    let r = catch_unwind(AssertUnwindSafe(|| unsafe {
        let body = &*ffi::sqlite3_user_data(ctx).cast::<Arc<crate::config::FunctionBody>>();
        let args: Vec<Value> = (0..argc.max(0) as usize).map(|i| crate::worker::value_of(value_raw(*argv.add(i)), None)).collect();
        body(&args)
    }));
    // SAFETY: the context is valid for this call; results are copied (TRANSIENT).
    unsafe {
        match r {
            Ok(Ok(v)) => set_result(ctx, &v),
            Ok(Err(m)) => {
                let m = m.replace('\0', " ");
                ffi::sqlite3_result_error(ctx, m.as_ptr().cast(), m.len() as c_int);
            }
            Err(_) => {
                let m = "panic in a Rust SQL function";
                ffi::sqlite3_result_error(ctx, m.as_ptr().cast(), m.len() as c_int);
            }
        }
    }
}

unsafe extern "C" fn drop_function(p: *mut c_void) {
    // SAFETY: the box leaked at registration, freed once by SQLite's call.
    drop(unsafe { Box::from_raw(p.cast::<Arc<crate::config::FunctionBody>>()) });
}

/// # Safety
/// `v` is a protected value valid for the current callback.
unsafe fn value_raw(v: *mut ffi::sqlite3_value) -> Raw {
    // SAFETY: per the contract; text / blob are copied before returning.
    unsafe {
        match ffi::sqlite3_value_type(v) {
            ffi::SQLITE_INTEGER => Raw::Int(ffi::sqlite3_value_int64(v)),
            ffi::SQLITE_FLOAT => Raw::Real(ffi::sqlite3_value_double(v)),
            ffi::SQLITE_TEXT => {
                let p = ffi::sqlite3_value_text(v);
                Raw::Text(bytes(p.cast(), ffi::sqlite3_value_bytes(v)))
            }
            ffi::SQLITE_BLOB => {
                let p = ffi::sqlite3_value_blob(v);
                Raw::Blob(bytes(p, ffi::sqlite3_value_bytes(v)))
            }
            _ => Raw::Null,
        }
    }
}

/// # Safety
/// `ctx` is the context of the running function call.
unsafe fn set_result(ctx: *mut ffi::sqlite3_context, v: &Value) {
    // SAFETY: per the contract; TRANSIENT makes SQLite copy the buffers.
    unsafe {
        match v {
            Value::Null => ffi::sqlite3_result_null(ctx),
            Value::Bool(b) => ffi::sqlite3_result_int64(ctx, i64::from(*b)),
            Value::Int(i) => ffi::sqlite3_result_int64(ctx, *i),
            Value::UInt(u) => match i64::try_from(*u) {
                Ok(i) => ffi::sqlite3_result_int64(ctx, i),
                Err(_) => ffi::sqlite3_result_double(ctx, *u as f64),
            },
            Value::Float(f) => ffi::sqlite3_result_double(ctx, *f),
            Value::Bytes(b) => ffi::sqlite3_result_blob64(ctx, b.as_ptr().cast(), b.len() as u64, ffi::TRANSIENT),
            Value::Text(s) | Value::Decimal(s) | Value::Json(s) | Value::Date(s) | Value::Time(s) | Value::DateTime(s) | Value::Interval(s) => {
                ffi::sqlite3_result_text64(ctx, s.as_ptr().cast(), s.len() as u64, ffi::TRANSIENT, ffi::SQLITE_UTF8 as u8)
            }
            other => {
                let s = format!("{other:?}");
                ffi::sqlite3_result_text64(ctx, s.as_ptr().cast(), s.len() as u64, ffi::TRANSIENT, ffi::SQLITE_UTF8 as u8)
            }
        }
    }
}

unsafe extern "C" fn on_update(arg: *mut c_void, op: c_int, db: *const c_char, table: *const c_char, rowid: i64) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `arg` is the hook box owned by the Db; names are valid for
        // the call and copied.
        unsafe {
            let f = &mut *arg.cast::<ChangeFn>();
            f(op, &text(db).unwrap_or_default(), &text(table).unwrap_or_default(), rowid);
        }
    }));
}

unsafe extern "C" fn on_commit(arg: *mut c_void) -> c_int {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the hook box owned by the Db.
        unsafe { (*arg.cast::<CommitFn>())() }
    }));
    0 // never turn the commit into a rollback
}

unsafe extern "C" fn on_rollback(arg: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the hook box owned by the Db.
        unsafe { (*arg.cast::<CommitFn>())() }
    }));
}

unsafe extern "C" fn on_conflict(ctx: *mut c_void, kind: c_int, _iter: *mut ffi::sqlite3_changeset_iter) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `ctx` points to the `&dyn Fn` living in apply_changeset.
        let handler = unsafe { *ctx.cast::<&dyn Fn(c_int) -> c_int>() };
        handler(kind)
    }))
    .unwrap_or(ffi::SQLITE_CHANGESET_ABORT)
}

// ---------------------------------------------------------------------------
// Statements

/// A prepared statement.
pub(crate) struct Stmt {
    stmt: NonNull<ffi::sqlite3_stmt>,
    db: *mut ffi::sqlite3,
}

impl Drop for Stmt {
    fn drop(&mut self) {
        // SAFETY: finalized exactly once (valid even after close_v2).
        unsafe { ffi::sqlite3_finalize(self.stmt.as_ptr()) };
    }
}

impl Stmt {
    fn ptr(&self) -> *mut ffi::sqlite3_stmt {
        self.stmt.as_ptr()
    }

    fn error(&self, rc: c_int) -> Error {
        // SAFETY: the statement's connection is open while the statement is
        // in use (the Db outlives it on the worker thread).
        let (code, message) = unsafe { (ffi::sqlite3_extended_errcode(self.db), text(ffi::sqlite3_errmsg(self.db))) };
        Error { code: if code == ffi::SQLITE_OK { rc } else { code }, message: message.unwrap_or_else(|| Error::from_code(rc).message) }
    }

    fn check(&self, rc: c_int) -> Result<()> {
        if rc == ffi::SQLITE_OK {
            Ok(())
        } else {
            Err(self.error(rc))
        }
    }

    pub(crate) fn parameter_count(&self) -> usize {
        // SAFETY: live statement.
        unsafe { ffi::sqlite3_bind_parameter_count(self.ptr()) }.max(0) as usize
    }

    /// Bind parameter `i` (1-based).
    pub(crate) fn bind(&mut self, i: usize, v: &Raw) -> Result<()> {
        let i = i as c_int;
        // SAFETY: live statement; TRANSIENT makes SQLite copy text / blobs.
        let rc = unsafe {
            match v {
                Raw::Null => ffi::sqlite3_bind_null(self.ptr(), i),
                Raw::Int(n) => ffi::sqlite3_bind_int64(self.ptr(), i, *n),
                Raw::Real(f) => ffi::sqlite3_bind_double(self.ptr(), i, *f),
                Raw::Text(t) => ffi::sqlite3_bind_text64(self.ptr(), i, t.as_ptr().cast(), t.len() as u64, ffi::TRANSIENT, ffi::SQLITE_UTF8 as u8),
                Raw::Blob(b) => ffi::sqlite3_bind_blob64(self.ptr(), i, b.as_ptr().cast(), b.len() as u64, ffi::TRANSIENT),
            }
        };
        self.check(rc)
    }

    /// Advance: `true` when a row is ready, `false` when done.
    pub(crate) fn step(&mut self) -> Result<bool> {
        // SAFETY: live statement.
        match unsafe { ffi::sqlite3_step(self.ptr()) } {
            ffi::SQLITE_ROW => Ok(true),
            ffi::SQLITE_DONE => Ok(false),
            rc => Err(self.error(rc)),
        }
    }

    /// Back to before the first row, bindings kept.
    pub(crate) fn reset(&mut self) {
        // SAFETY: live statement. Its return repeats the last step's error,
        // already reported.
        unsafe { ffi::sqlite3_reset(self.ptr()) };
    }

    pub(crate) fn clear_bindings(&mut self) {
        // SAFETY: live statement.
        unsafe { ffi::sqlite3_clear_bindings(self.ptr()) };
    }

    pub(crate) fn column_count(&self) -> usize {
        // SAFETY: live statement.
        unsafe { ffi::sqlite3_column_count(self.ptr()) }.max(0) as usize
    }

    pub(crate) fn column_name(&self, i: usize) -> String {
        // SAFETY: live statement, index below column_count; copied at once.
        unsafe { text(ffi::sqlite3_column_name(self.ptr(), i as c_int)) }.unwrap_or_default()
    }

    pub(crate) fn column_decltype(&self, i: usize) -> Option<String> {
        // SAFETY: as above.
        unsafe { text(ffi::sqlite3_column_decltype(self.ptr(), i as c_int)) }
    }

    /// Column `i` of the current row.
    pub(crate) fn column(&self, i: usize) -> Raw {
        let i = i as c_int;
        // SAFETY: a row is current (after step returned true); the text /
        // blob pointer is read before column_bytes and copied at once, as the
        // documentation prescribes.
        unsafe {
            match ffi::sqlite3_column_type(self.ptr(), i) {
                ffi::SQLITE_INTEGER => Raw::Int(ffi::sqlite3_column_int64(self.ptr(), i)),
                ffi::SQLITE_FLOAT => Raw::Real(ffi::sqlite3_column_double(self.ptr(), i)),
                ffi::SQLITE_TEXT => {
                    let p = ffi::sqlite3_column_text(self.ptr(), i);
                    Raw::Text(bytes(p.cast(), ffi::sqlite3_column_bytes(self.ptr(), i)))
                }
                ffi::SQLITE_BLOB => {
                    let p = ffi::sqlite3_column_blob(self.ptr(), i);
                    Raw::Blob(bytes(p, ffi::sqlite3_column_bytes(self.ptr(), i)))
                }
                _ => Raw::Null,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// BLOBs, sessions, changesets

/// An open BLOB (incremental I/O).
pub(crate) struct Blob {
    b: NonNull<ffi::sqlite3_blob>,
    db: *mut ffi::sqlite3,
}

impl Drop for Blob {
    fn drop(&mut self) {
        // SAFETY: closed exactly once.
        unsafe { ffi::sqlite3_blob_close(self.b.as_ptr()) };
    }
}

impl Blob {
    fn check(&self, rc: c_int) -> Result<()> {
        if rc == ffi::SQLITE_OK {
            return Ok(());
        }
        // SAFETY: the BLOB's connection is open while it is.
        let message = unsafe { text(ffi::sqlite3_errmsg(self.db)) }.unwrap_or_else(|| Error::from_code(rc).message);
        Err(Error { code: rc, message })
    }

    pub(crate) fn len(&self) -> usize {
        // SAFETY: open BLOB.
        unsafe { ffi::sqlite3_blob_bytes(self.b.as_ptr()) }.max(0) as usize
    }

    /// Read `len` bytes at `offset`; past the end is an error.
    pub(crate) fn read(&self, offset: usize, len: usize) -> Result<Vec<u8>> {
        let (off, n) = (to_int(offset)?, to_int(len)?);
        let mut buf = vec![0u8; len];
        // SAFETY: `buf` has room for `n` bytes; SQLite checks the range.
        self.check(unsafe { ffi::sqlite3_blob_read(self.b.as_ptr(), buf.as_mut_ptr().cast(), n, off) })?;
        Ok(buf)
    }

    /// Overwrite bytes at `offset`; a BLOB never grows this way.
    pub(crate) fn write(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        let (off, n) = (to_int(offset)?, to_int(data.len())?);
        // SAFETY: `data` is valid for `n` bytes; SQLite checks the range.
        self.check(unsafe { ffi::sqlite3_blob_write(self.b.as_ptr(), data.as_ptr().cast(), n, off) })
    }
}

fn to_int(n: usize) -> Result<c_int> {
    c_int::try_from(n).map_err(|_| Error::misuse("offset or length beyond 2 GiB"))
}

/// A change-recording session.
pub(crate) struct Session {
    s: NonNull<ffi::sqlite3_session>,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: deleted exactly once, before or after the connection's
        // close_v2 (which waits for it).
        unsafe { ffi::sqlite3session_delete(self.s.as_ptr()) };
    }
}

/// Take ownership of an SQLite-allocated buffer of `n` bytes.
///
/// # Safety
/// `p` is NULL or a sqlite3_malloc'd buffer of `n` bytes, not used afterwards.
unsafe fn take_buffer(p: *mut c_void, n: c_int) -> Vec<u8> {
    // SAFETY: per the contract; freed once.
    unsafe {
        let out = bytes(p, n);
        ffi::sqlite3_free(p);
        out
    }
}

impl Session {
    /// Everything recorded so far, as a changeset (`patch`: a patchset).
    pub(crate) fn changes(&mut self, patch: bool) -> Result<Vec<u8>> {
        let (mut n, mut p): (c_int, *mut c_void) = (0, ptr::null_mut());
        // SAFETY: live session, valid out-pointers; the buffer is taken over.
        unsafe {
            let rc = if patch {
                ffi::sqlite3session_patchset(self.s.as_ptr(), &mut n, &mut p)
            } else {
                ffi::sqlite3session_changeset(self.s.as_ptr(), &mut n, &mut p)
            };
            let out = take_buffer(p, n);
            if rc != ffi::SQLITE_OK {
                return Err(Error::from_code(rc));
            }
            Ok(out)
        }
    }
}

/// The changeset that undoes `cs`.
pub(crate) fn invert_changeset(cs: &[u8]) -> Result<Vec<u8>> {
    let n = to_int(cs.len())?;
    let (mut on, mut op): (c_int, *mut c_void) = (0, ptr::null_mut());
    // SAFETY: input valid for `n` bytes; the output buffer is taken over.
    unsafe {
        let rc = ffi::sqlite3changeset_invert(n, cs.as_ptr().cast(), &mut on, &mut op);
        let out = take_buffer(op, on);
        if rc != ffi::SQLITE_OK {
            return Err(Error::from_code(rc));
        }
        Ok(out)
    }
}

/// `a` followed by `b`, as one changeset.
pub(crate) fn concat_changesets(a: &[u8], b: &[u8]) -> Result<Vec<u8>> {
    let (na, nb) = (to_int(a.len())?, to_int(b.len())?);
    let (mut on, mut op): (c_int, *mut c_void) = (0, ptr::null_mut());
    // SAFETY: SQLite only reads the inputs; the output buffer is taken over.
    unsafe {
        let rc = ffi::sqlite3changeset_concat(na, a.as_ptr().cast_mut().cast(), nb, b.as_ptr().cast_mut().cast(), &mut on, &mut op);
        let out = take_buffer(op, on);
        if rc != ffi::SQLITE_OK {
            return Err(Error::from_code(rc));
        }
        Ok(out)
    }
}

/// The operations of a changeset: `(table, op, indirect)`.
pub(crate) fn changeset_ops(cs: &[u8]) -> Result<Vec<(String, c_int, bool)>> {
    let n = to_int(cs.len())?;
    let mut iter = ptr::null_mut();
    // SAFETY: the iterator reads `cs` (alive for the whole loop) and is
    // finalized once; table names are copied while the item is current.
    unsafe {
        let rc = ffi::sqlite3changeset_start(&mut iter, n, cs.as_ptr().cast_mut().cast());
        if rc != ffi::SQLITE_OK {
            return Err(Error::from_code(rc));
        }
        let mut out = Vec::new();
        let mut failed = None;
        loop {
            match ffi::sqlite3changeset_next(iter) {
                ffi::SQLITE_ROW => {
                    let (mut table, mut cols, mut op, mut indirect): (*const c_char, c_int, c_int, c_int) = (ptr::null(), 0, 0, 0);
                    let rc = ffi::sqlite3changeset_op(iter, &mut table, &mut cols, &mut op, &mut indirect);
                    if rc != ffi::SQLITE_OK {
                        failed = Some(rc);
                        break;
                    }
                    out.push((text(table).unwrap_or_default(), op, indirect != 0));
                }
                ffi::SQLITE_DONE => break,
                rc => {
                    failed = Some(rc);
                    break;
                }
            }
        }
        let rc = ffi::sqlite3changeset_finalize(iter);
        match failed.or((rc != ffi::SQLITE_OK).then_some(rc)) {
            Some(rc) => Err(Error::from_code(rc)),
            None => Ok(out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Db {
        Db::open(":memory:", ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE).unwrap()
    }

    #[test]
    fn the_compiled_engine_is_the_vendored_one() {
        assert_eq!(version(), "3.54.1");
    }

    #[test]
    fn prepare_step_bind_and_read_every_storage_class() {
        let db = mem();
        let mut s = db.prepare("SELECT ?, ?, ?, ?, ?", false).unwrap();
        assert_eq!(s.parameter_count(), 5);
        let vals = [Raw::Null, Raw::Int(i64::MIN), Raw::Real(-0.25), Raw::Text("Việt 😀".as_bytes().to_vec()), Raw::Blob(vec![0, 255])];
        for (i, v) in vals.iter().enumerate() {
            s.bind(i + 1, v).unwrap();
        }
        assert!(s.step().unwrap());
        assert_eq!(s.column_count(), 5);
        assert_eq!((0..5).map(|i| s.column(i)).collect::<Vec<_>>(), vals);
        assert!(!s.step().unwrap());
    }

    #[test]
    fn errors_carry_extended_codes_and_messages() {
        let db = mem();
        let e = db.prepare("SELEC 1", false).err().unwrap();
        assert!(e.message.contains("syntax error"), "{e:?}");
        db.exec("CREATE TABLE t (id INTEGER PRIMARY KEY, u TEXT UNIQUE); INSERT INTO t VALUES (1, 'a');").unwrap();
        let e = db.exec("INSERT INTO t VALUES (2, 'a')").unwrap_err();
        assert_eq!(e.code, ffi::SQLITE_CONSTRAINT_UNIQUE, "{e:?}");
        assert!(e.message.contains("t.u"));
        assert!(db.prepare("", false).is_err(), "empty SQL");
        assert!(db.prepare("SELECT 1; SELECT 2", false).is_err(), "two statements");
        assert!(db.prepare("SELECT 1; -- just a comment", false).is_ok());
        assert!(db.prepare("SELECT 1 ;  ", false).is_ok());
    }

    #[test]
    fn open_failure_reports_why() {
        let e = Db::open("/no/such/dir/x.db", ffi::SQLITE_OPEN_READWRITE).err().unwrap();
        assert!(e.message.contains("unable to open"), "{e:?}");
    }

    #[test]
    fn serialize_round_trips_through_deserialize() {
        let db = mem();
        db.exec("CREATE TABLE t (n); INSERT INTO t VALUES (42);").unwrap();
        let image = db.serialize().unwrap();
        assert!(image.starts_with(b"SQLite format 3\0"));
        let copy = mem();
        copy.deserialize(&image).unwrap();
        let mut s = copy.prepare("SELECT n FROM t", false).unwrap();
        assert!(s.step().unwrap());
        assert_eq!(s.column(0), Raw::Int(42));
    }
}
