//! The part of SQLite's C API the driver calls, declared by hand from
//! `sqlite/sqlite3.h` (3.54.1). Only the safe wrappers in [`crate::raw`] use
//! these.
#![allow(non_camel_case_types, missing_docs, dead_code)]

use std::os::raw::{c_char, c_int, c_uchar, c_uint, c_void};

#[repr(C)]
pub struct sqlite3 {
    _p: [u8; 0],
}
#[repr(C)]
pub struct sqlite3_stmt {
    _p: [u8; 0],
}
#[repr(C)]
pub struct sqlite3_value {
    _p: [u8; 0],
}
#[repr(C)]
pub struct sqlite3_context {
    _p: [u8; 0],
}
#[repr(C)]
pub struct sqlite3_backup {
    _p: [u8; 0],
}
#[repr(C)]
pub struct sqlite3_blob {
    _p: [u8; 0],
}
#[repr(C)]
pub struct sqlite3_session {
    _p: [u8; 0],
}
#[repr(C)]
pub struct sqlite3_changeset_iter {
    _p: [u8; 0],
}

/// A destructor argument: a function, or one of the two sentinels.
pub type Destructor = isize;
/// `SQLITE_TRANSIENT`: SQLite copies the buffer before the call returns.
pub const TRANSIENT: Destructor = -1;

pub const SQLITE_OK: c_int = 0;
pub const SQLITE_ERROR: c_int = 1;
pub const SQLITE_BUSY: c_int = 5;
pub const SQLITE_LOCKED: c_int = 6;
pub const SQLITE_INTERRUPT: c_int = 9;
pub const SQLITE_CONSTRAINT: c_int = 19;
pub const SQLITE_MISUSE: c_int = 21;
pub const SQLITE_ROW: c_int = 100;
pub const SQLITE_DONE: c_int = 101;
pub const SQLITE_CONSTRAINT_UNIQUE: c_int = SQLITE_CONSTRAINT | (8 << 8);
pub const SQLITE_CONSTRAINT_PRIMARYKEY: c_int = SQLITE_CONSTRAINT | (6 << 8);
pub const SQLITE_CONSTRAINT_FOREIGNKEY: c_int = SQLITE_CONSTRAINT | (3 << 8);

pub const SQLITE_INTEGER: c_int = 1;
pub const SQLITE_FLOAT: c_int = 2;
pub const SQLITE_TEXT: c_int = 3;
pub const SQLITE_BLOB: c_int = 4;
pub const SQLITE_NULL: c_int = 5;

pub const SQLITE_OPEN_READONLY: c_int = 0x0000_0001;
pub const SQLITE_OPEN_READWRITE: c_int = 0x0000_0002;
pub const SQLITE_OPEN_CREATE: c_int = 0x0000_0004;
pub const SQLITE_OPEN_URI: c_int = 0x0000_0040;
pub const SQLITE_OPEN_NOMUTEX: c_int = 0x0000_8000;
pub const SQLITE_OPEN_EXRESCODE: c_int = 0x0200_0000;

pub const SQLITE_UTF8: c_int = 1;
pub const SQLITE_DETERMINISTIC: c_int = 0x0000_0800;
pub const SQLITE_PREPARE_PERSISTENT: c_uint = 0x01;

pub const SQLITE_DELETE: c_int = 9;
pub const SQLITE_INSERT: c_int = 18;
pub const SQLITE_UPDATE: c_int = 23;

pub const SQLITE_CHANGESET_DATA: c_int = 1;
pub const SQLITE_CHANGESET_NOTFOUND: c_int = 2;
pub const SQLITE_CHANGESET_CONFLICT: c_int = 3;
pub const SQLITE_CHANGESET_CONSTRAINT: c_int = 4;
pub const SQLITE_CHANGESET_FOREIGN_KEY: c_int = 5;
pub const SQLITE_CHANGESET_OMIT: c_int = 0;
pub const SQLITE_CHANGESET_REPLACE: c_int = 1;
pub const SQLITE_CHANGESET_ABORT: c_int = 2;

pub const SQLITE_DESERIALIZE_FREEONCLOSE: c_uint = 1;
pub const SQLITE_DESERIALIZE_RESIZEABLE: c_uint = 2;

pub type FuncCallback = unsafe extern "C" fn(*mut sqlite3_context, c_int, *mut *mut sqlite3_value);
pub type DestroyCallback = unsafe extern "C" fn(*mut c_void);
pub type UpdateHook = unsafe extern "C" fn(*mut c_void, c_int, *const c_char, *const c_char, i64);
pub type CommitHook = unsafe extern "C" fn(*mut c_void) -> c_int;
pub type RollbackHook = unsafe extern "C" fn(*mut c_void);
pub type ConflictCallback = unsafe extern "C" fn(*mut c_void, c_int, *mut sqlite3_changeset_iter) -> c_int;
pub type FilterCallback = unsafe extern "C" fn(*mut c_void, *const c_char) -> c_int;

extern "C" {
    pub fn sqlite3_libversion() -> *const c_char;
    pub fn sqlite3_free(p: *mut c_void);
    pub fn sqlite3_malloc64(n: u64) -> *mut c_void;

    pub fn sqlite3_open_v2(filename: *const c_char, db: *mut *mut sqlite3, flags: c_int, vfs: *const c_char) -> c_int;
    pub fn sqlite3_close_v2(db: *mut sqlite3) -> c_int;
    pub fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char;
    pub fn sqlite3_errstr(rc: c_int) -> *const c_char;
    pub fn sqlite3_extended_errcode(db: *mut sqlite3) -> c_int;
    pub fn sqlite3_busy_timeout(db: *mut sqlite3, ms: c_int) -> c_int;
    pub fn sqlite3_interrupt(db: *mut sqlite3);
    pub fn sqlite3_changes64(db: *mut sqlite3) -> i64;
    pub fn sqlite3_total_changes64(db: *mut sqlite3) -> i64;
    pub fn sqlite3_last_insert_rowid(db: *mut sqlite3) -> i64;
    pub fn sqlite3_get_autocommit(db: *mut sqlite3) -> c_int;
    pub fn sqlite3_exec(
        db: *mut sqlite3,
        sql: *const c_char,
        callback: *const c_void,
        arg: *mut c_void,
        errmsg: *mut *mut c_char,
    ) -> c_int;

    pub fn sqlite3_prepare_v3(
        db: *mut sqlite3,
        sql: *const c_char,
        n_byte: c_int,
        flags: c_uint,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int;
    pub fn sqlite3_step(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_reset(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_clear_bindings(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_finalize(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_bind_parameter_count(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_bind_null(stmt: *mut sqlite3_stmt, i: c_int) -> c_int;
    pub fn sqlite3_bind_int64(stmt: *mut sqlite3_stmt, i: c_int, v: i64) -> c_int;
    pub fn sqlite3_bind_double(stmt: *mut sqlite3_stmt, i: c_int, v: f64) -> c_int;
    pub fn sqlite3_bind_text64(stmt: *mut sqlite3_stmt, i: c_int, v: *const c_char, n: u64, del: Destructor, enc: c_uchar) -> c_int;
    pub fn sqlite3_bind_blob64(stmt: *mut sqlite3_stmt, i: c_int, v: *const c_void, n: u64, del: Destructor) -> c_int;
    pub fn sqlite3_column_count(stmt: *mut sqlite3_stmt) -> c_int;
    pub fn sqlite3_column_name(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_char;
    pub fn sqlite3_column_decltype(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_char;
    pub fn sqlite3_column_type(stmt: *mut sqlite3_stmt, i: c_int) -> c_int;
    pub fn sqlite3_column_int64(stmt: *mut sqlite3_stmt, i: c_int) -> i64;
    pub fn sqlite3_column_double(stmt: *mut sqlite3_stmt, i: c_int) -> f64;
    pub fn sqlite3_column_text(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_uchar;
    pub fn sqlite3_column_blob(stmt: *mut sqlite3_stmt, i: c_int) -> *const c_void;
    pub fn sqlite3_column_bytes(stmt: *mut sqlite3_stmt, i: c_int) -> c_int;

    pub fn sqlite3_create_function_v2(
        db: *mut sqlite3,
        name: *const c_char,
        n_arg: c_int,
        text_rep: c_int,
        app: *mut c_void,
        func: Option<FuncCallback>,
        step: Option<FuncCallback>,
        last: Option<unsafe extern "C" fn(*mut sqlite3_context)>,
        destroy: Option<DestroyCallback>,
    ) -> c_int;
    pub fn sqlite3_user_data(ctx: *mut sqlite3_context) -> *mut c_void;
    pub fn sqlite3_value_type(v: *mut sqlite3_value) -> c_int;
    pub fn sqlite3_value_int64(v: *mut sqlite3_value) -> i64;
    pub fn sqlite3_value_double(v: *mut sqlite3_value) -> f64;
    pub fn sqlite3_value_text(v: *mut sqlite3_value) -> *const c_uchar;
    pub fn sqlite3_value_blob(v: *mut sqlite3_value) -> *const c_void;
    pub fn sqlite3_value_bytes(v: *mut sqlite3_value) -> c_int;
    pub fn sqlite3_result_null(ctx: *mut sqlite3_context);
    pub fn sqlite3_result_int64(ctx: *mut sqlite3_context, v: i64);
    pub fn sqlite3_result_double(ctx: *mut sqlite3_context, v: f64);
    pub fn sqlite3_result_text64(ctx: *mut sqlite3_context, v: *const c_char, n: u64, del: Destructor, enc: c_uchar);
    pub fn sqlite3_result_blob64(ctx: *mut sqlite3_context, v: *const c_void, n: u64, del: Destructor);
    pub fn sqlite3_result_error(ctx: *mut sqlite3_context, msg: *const c_char, n: c_int);

    pub fn sqlite3_update_hook(db: *mut sqlite3, cb: Option<UpdateHook>, arg: *mut c_void) -> *mut c_void;
    pub fn sqlite3_commit_hook(db: *mut sqlite3, cb: Option<CommitHook>, arg: *mut c_void) -> *mut c_void;
    pub fn sqlite3_rollback_hook(db: *mut sqlite3, cb: Option<RollbackHook>, arg: *mut c_void) -> *mut c_void;

    pub fn sqlite3_backup_init(dest: *mut sqlite3, dest_name: *const c_char, src: *mut sqlite3, src_name: *const c_char) -> *mut sqlite3_backup;
    pub fn sqlite3_backup_step(b: *mut sqlite3_backup, pages: c_int) -> c_int;
    pub fn sqlite3_backup_finish(b: *mut sqlite3_backup) -> c_int;

    pub fn sqlite3_blob_open(
        db: *mut sqlite3,
        db_name: *const c_char,
        table: *const c_char,
        column: *const c_char,
        row: i64,
        flags: c_int,
        blob: *mut *mut sqlite3_blob,
    ) -> c_int;
    pub fn sqlite3_blob_close(b: *mut sqlite3_blob) -> c_int;
    pub fn sqlite3_blob_bytes(b: *mut sqlite3_blob) -> c_int;
    pub fn sqlite3_blob_read(b: *mut sqlite3_blob, z: *mut c_void, n: c_int, offset: c_int) -> c_int;
    pub fn sqlite3_blob_write(b: *mut sqlite3_blob, z: *const c_void, n: c_int, offset: c_int) -> c_int;

    pub fn sqlite3_serialize(db: *mut sqlite3, schema: *const c_char, size: *mut i64, flags: c_uint) -> *mut c_uchar;
    pub fn sqlite3_deserialize(db: *mut sqlite3, schema: *const c_char, data: *mut c_uchar, size: i64, buf: i64, flags: c_uint) -> c_int;

    pub fn sqlite3_enable_load_extension(db: *mut sqlite3, on: c_int) -> c_int;
    pub fn sqlite3_load_extension(db: *mut sqlite3, file: *const c_char, entry: *const c_char, err: *mut *mut c_char) -> c_int;

    pub fn sqlite3session_create(db: *mut sqlite3, schema: *const c_char, out: *mut *mut sqlite3_session) -> c_int;
    pub fn sqlite3session_delete(s: *mut sqlite3_session);
    pub fn sqlite3session_attach(s: *mut sqlite3_session, table: *const c_char) -> c_int;
    pub fn sqlite3session_changeset(s: *mut sqlite3_session, n: *mut c_int, out: *mut *mut c_void) -> c_int;
    pub fn sqlite3session_patchset(s: *mut sqlite3_session, n: *mut c_int, out: *mut *mut c_void) -> c_int;
    pub fn sqlite3changeset_invert(n_in: c_int, input: *const c_void, n_out: *mut c_int, out: *mut *mut c_void) -> c_int;
    pub fn sqlite3changeset_concat(n_a: c_int, a: *mut c_void, n_b: c_int, b: *mut c_void, n_out: *mut c_int, out: *mut *mut c_void) -> c_int;
    pub fn sqlite3changeset_start(iter: *mut *mut sqlite3_changeset_iter, n: c_int, cs: *mut c_void) -> c_int;
    pub fn sqlite3changeset_next(iter: *mut sqlite3_changeset_iter) -> c_int;
    pub fn sqlite3changeset_op(
        iter: *mut sqlite3_changeset_iter,
        table: *mut *const c_char,
        n_col: *mut c_int,
        op: *mut c_int,
        indirect: *mut c_int,
    ) -> c_int;
    pub fn sqlite3changeset_finalize(iter: *mut sqlite3_changeset_iter) -> c_int;
    pub fn sqlite3changeset_apply(
        db: *mut sqlite3,
        n: c_int,
        cs: *mut c_void,
        filter: Option<FilterCallback>,
        conflict: Option<ConflictCallback>,
        ctx: *mut c_void,
    ) -> c_int;
}
