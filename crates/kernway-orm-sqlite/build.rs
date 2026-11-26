//! Compile the vendored SQLite amalgamation (sqlite/) into the driver.

fn main() {
    println!("cargo:rerun-if-changed=sqlite/sqlite3.c");
    println!("cargo:rerun-if-changed=sqlite/sqlite3.h");
    let target = std::env::var("TARGET").unwrap_or_default();
    let windows = target.contains("windows");
    let msvc = target.contains("msvc");

    let mut c = cc::Build::new();
    c.file("sqlite/sqlite3.c").warnings(false);
    for (name, value) in [
        // Serialized mode: safe even if a handle crosses threads; the driver
        // keeps each connection on one thread, interrupts come from others.
        ("SQLITE_THREADSAFE", "1"),
        ("SQLITE_DQS", "0"), // "double quotes" are identifiers, never strings
        ("SQLITE_USE_URI", "1"),
        ("SQLITE_DEFAULT_WAL_SYNCHRONOUS", "1"),
        ("SQLITE_ENABLE_API_ARMOR", "1"),
        ("SQLITE_ENABLE_COLUMN_METADATA", "1"),
        ("SQLITE_ENABLE_DBSTAT_VTAB", "1"),
        ("SQLITE_ENABLE_FTS3", "1"),
        ("SQLITE_ENABLE_FTS3_PARENTHESIS", "1"),
        ("SQLITE_ENABLE_FTS5", "1"),
        ("SQLITE_ENABLE_MATH_FUNCTIONS", "1"),
        ("SQLITE_ENABLE_RTREE", "1"),
        ("SQLITE_ENABLE_STAT4", "1"),
        ("SQLITE_ENABLE_PREUPDATE_HOOK", "1"),
        ("SQLITE_ENABLE_SESSION", "1"),
        ("SQLITE_ENABLE_LOAD_EXTENSION", "1"),
        ("SQLITE_OMIT_DEPRECATED", "1"),
        ("HAVE_ISNAN", "1"),
    ] {
        c.define(name, value);
    }
    if !windows {
        // Sub-second sleeps for the busy timeout (otherwise whole seconds).
        c.define("HAVE_USLEEP", "1");
        c.define("HAVE_LOCALTIME_R", "1");
    }
    if !msvc {
        c.flag_if_supported("-fno-strict-aliasing");
    }
    c.compile("kernway_sqlite3");
}
