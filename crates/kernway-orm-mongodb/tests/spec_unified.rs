//! The official unified-format tests of the MongoDB specifications, run live
//! against the replica set and the sharded cluster of
//! `tests/support/mongo-setup.sh` (see `unified/mod.rs` for how each test is
//! counted). Spec data: `tests/support/specs-fetch.sh` (`KERNWAY_MONGO_SPECS`).

mod common;
mod unified;

fn files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(files(&p));
            } else if p.extension().is_some_and(|x| x == "json") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn suite(label: &str, rel: &str) {
    let (Some(root), Some(_)) = (common::env("KERNWAY_MONGO_SPECS"), common::env("KERNWAY_MONGO_RS_URL")) else { return };
    let dir = std::path::Path::new(&root).join(rel);
    let label = label.to_string();
    // The runner's futures are large in debug builds: a big stack for them.
    let run = std::thread::Builder::new().stack_size(512 << 20).spawn(move || common::block(async move {
        let deps = unified::deployments().await;
        assert!(!deps.is_empty(), "no deployment reachable");
        let mut t = unified::Tally::default();
        // KERNWAY_UNIFIED_FILTER=<substring>: only matching files (for debugging).
        let filter = std::env::var("KERNWAY_UNIFIED_FILTER").ok();
        let paths: Vec<_> = files(&dir).into_iter().filter(|p| filter.as_deref().is_none_or(|f| p.to_string_lossy().contains(f))).collect();
        for p in &paths {
            unified::run_file(p, &deps, &mut t).await;
        }
        eprintln!(
            "unified/{label}: {} tests in {} files — {} passed, {} failed, {} gaps, {} not runnable here",
            t.total(),
            paths.len(),
            t.passed,
            t.failed.len(),
            t.gaps.len(),
            t.not_runnable.len()
        );
        for g in &t.gaps {
            eprintln!("  GAP {g}");
        }
        for n in &t.not_runnable {
            eprintln!("  NOT RUNNABLE {n}");
        }
        for f in &t.failed {
            eprintln!("  FAIL {f}");
        }
        assert!(t.failed.is_empty(), "unified/{label}: {} failed", t.failed.len());
    }));
    if let Err(p) = run.expect("spawn the runner thread").join() {
        std::panic::resume_unwind(p);
    }
}

#[test]
fn unified_crud() {
    suite("crud", "crud/tests/unified");
}

#[test]
fn unified_transactions() {
    suite("transactions", "transactions/tests/unified");
}

#[test]
fn unified_transactions_convenient_api() {
    suite("transactions-convenient-api", "transactions-convenient-api/tests/unified");
}

#[test]
fn unified_retryable_writes() {
    suite("retryable-writes", "retryable-writes/tests/unified");
}

#[test]
fn unified_retryable_reads() {
    suite("retryable-reads", "retryable-reads/tests/unified");
}

#[test]
fn unified_sessions() {
    suite("sessions", "sessions/tests");
}

#[test]
fn unified_causal_consistency() {
    suite("causal-consistency", "causal-consistency/tests");
}

#[test]
fn unified_change_streams() {
    suite("change-streams", "change-streams/tests/unified");
}

#[test]
fn unified_read_write_concern() {
    suite("read-write-concern", "read-write-concern/tests/operation");
}

#[test]
fn unified_gridfs() {
    suite("gridfs", "gridfs/tests");
}

#[test]
fn unified_collection_management() {
    suite("collection-management", "collection-management/tests");
}

#[test]
fn unified_command_monitoring() {
    suite("command-monitoring", "command-logging-and-monitoring/tests/monitoring");
}

#[test]
fn unified_run_command() {
    suite("run-command", "run-command/tests/unified");
}

#[test]
fn unified_index_management() {
    suite("index-management", "index-management/tests");
}

#[test]
fn unified_format_valid_pass() {
    suite("unified-test-format/valid-pass", "unified-test-format/tests/valid-pass");
}
