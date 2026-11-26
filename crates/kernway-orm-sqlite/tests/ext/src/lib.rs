//! A test extension: `kw_ext_version()` and `kw_reverse(text)`, plus a
//! second entry point registering `kw_ext_alt()`.

use std::os::raw::{c_char, c_int};

use rusqlite::ffi;
use rusqlite::functions::FunctionFlags;
use rusqlite::{to_sqlite_error, Connection, Result};

fn init(db: *mut ffi::sqlite3, p_api: *mut ffi::sqlite3_api_routines, alt: bool) -> Result<()> {
    // SAFETY: called by SQLite's loader with its own handle and API table.
    let db = unsafe { Connection::extension_init2(db, p_api)? };
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    if alt {
        return db.create_scalar_function("kw_ext_alt", 0, flags, |_| Ok("alternate entry point".to_string()));
    }
    db.create_scalar_function("kw_ext_version", 0, flags, |_| Ok("kernway-test-ext 1".to_string()))?;
    db.create_scalar_function("kw_reverse", 1, flags, |ctx| Ok(ctx.get::<String>(0)?.chars().rev().collect::<String>()))
}

fn entry(db: *mut ffi::sqlite3, err: *mut *mut c_char, p_api: *mut ffi::sqlite3_api_routines, alt: bool) -> c_int {
    if p_api.is_null() {
        return ffi::SQLITE_ERROR;
    }
    match init(db, p_api, alt) {
        Ok(()) => ffi::SQLITE_OK,
        // SAFETY: `err` is the error-message slot SQLite passed in.
        Err(e) => unsafe { to_sqlite_error(&e, err) },
    }
}

/// The default entry point SQLite looks for.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn sqlite3_extension_init(db: *mut ffi::sqlite3, err: *mut *mut c_char, p_api: *mut ffi::sqlite3_api_routines) -> c_int {
    entry(db, err, p_api, false)
}

/// A named entry point, chosen with `entry_point`.
#[no_mangle]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub extern "C" fn kw_alt_init(db: *mut ffi::sqlite3, err: *mut *mut c_char, p_api: *mut ffi::sqlite3_api_routines) -> c_int {
    entry(db, err, p_api, true)
}
