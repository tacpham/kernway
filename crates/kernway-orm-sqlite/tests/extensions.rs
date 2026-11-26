//! Native extensions: the test builds a real SQLite extension (tests/ext, a
//! cdylib) with this toolchain and loads it — `.so` on Linux, `.dylib` on
//! macOS, `.dll` on Windows.

use kernway_orm_sqlite::{SqliteConfig, SqliteDriver};
use std::path::PathBuf;
use std::process::Command;

fn run<F: std::future::Future>(f: F) -> F::Output {
    rt_core::Executor::new().unwrap().block_on(f).unwrap()
}

/// Build tests/ext and return the library path. Its own target directory:
/// it is a separate workspace (see its Cargo.toml).
fn build_extension() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("ext");
    let target = std::env::temp_dir().join("kernway-test-ext-target");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args(["build", "--locked", "--quiet", "--manifest-path"])
        .arg(dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(&target)
        .output()
        .expect("run cargo");
    assert!(out.status.success(), "building the test extension failed:\n{}", String::from_utf8_lossy(&out.stderr));
    let lib = target
        .join("debug")
        .join(format!("{}kernway_test_ext{}", std::env::consts::DLL_PREFIX, std::env::consts::DLL_SUFFIX));
    assert!(lib.exists(), "{} missing", lib.display());
    lib
}

#[test]
fn a_native_extension_is_loaded_into_every_connection() {
    let lib = build_extension();
    run(async move {
        // SAFETY: our own test extension, just built from this repository.
        let cfg = unsafe { SqliteConfig::memory().pool_size(3).extension(&lib, None) };
        let db = SqliteDriver::with_config(cfg).await.unwrap();
        // Three connections at once: each one has the extension's functions.
        let (a, b, c) = (
            db.query_scalar("SELECT kw_ext_version()", &[]),
            db.query_scalar("SELECT kw_reverse('Việt Nam')", &[]),
            db.query_scalar("SELECT kw_reverse(kw_ext_version())", &[]),
        );
        assert_eq!(a.await.unwrap().as_str(), Some("kernway-test-ext 1"));
        assert_eq!(b.await.unwrap().as_str(), Some("maN tệiV"));
        assert_eq!(c.await.unwrap().as_str(), Some("1 txe-tset-yawnrek"));

        // SQL cannot load libraries by itself.
        let e = db.query(&format!("SELECT load_extension('{}')", lib.display().to_string().replace('\'', "''")), &[]).await.unwrap_err();
        assert!(e.to_string().contains("not authorized"), "{e}");
    });
}

#[test]
fn a_named_entry_point_and_load_failures() {
    let lib = build_extension();
    run(async move {
        // SAFETY: as above.
        let cfg = unsafe { SqliteConfig::memory().extension(&lib, Some("kw_alt_init")) };
        let db = SqliteDriver::with_config(cfg).await.unwrap();
        assert_eq!(db.query_scalar("SELECT kw_ext_alt()", &[]).await.unwrap().as_str(), Some("alternate entry point"));
        assert!(db.query("SELECT kw_ext_version()", &[]).await.is_err(), "only the chosen entry point ran");

        // SAFETY: nothing is loaded — the open must fail.
        let missing = unsafe { SqliteConfig::memory().extension(lib.with_file_name("no-such-extension"), None) };
        let e = SqliteDriver::with_config(missing).await.err().expect("a missing library fails the open");
        assert!(e.to_string().contains("load extension"), "{e}");
        // SAFETY: the real library, a wrong entry point: refused, nothing runs.
        let wrong = unsafe { SqliteConfig::memory().extension(&lib, Some("no_such_init")) };
        assert!(SqliteDriver::with_config(wrong).await.is_err());
    });
}
