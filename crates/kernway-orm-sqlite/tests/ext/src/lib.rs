//! Exports the C entry points of src/ext.c from the shared library under the
//! names SQLite's loader looks for (a static C archive's symbols are only
//! exported from a cdylib when Rust re-exports them).

use std::os::raw::{c_char, c_int, c_void};

extern "C" {
    fn kw_c_init(db: *mut c_void, err: *mut *mut c_char, api: *const c_void) -> c_int;
    fn kw_c_alt_init(db: *mut c_void, err: *mut *mut c_char, api: *const c_void) -> c_int;
}

/// The default entry point.
///
/// # Safety
/// Called by SQLite's extension loader with its own arguments.
#[no_mangle]
pub unsafe extern "C" fn sqlite3_extension_init(db: *mut c_void, err: *mut *mut c_char, api: *const c_void) -> c_int {
    // SAFETY: arguments passed through unchanged from SQLite's loader.
    unsafe { kw_c_init(db, err, api) }
}

/// A named entry point, chosen with `entry_point`.
///
/// # Safety
/// As above.
#[no_mangle]
pub unsafe extern "C" fn kw_alt_init(db: *mut c_void, err: *mut *mut c_char, api: *const c_void) -> c_int {
    // SAFETY: as above.
    unsafe { kw_c_alt_init(db, err, api) }
}
