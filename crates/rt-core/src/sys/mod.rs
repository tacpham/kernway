//! Platform layer.
//!
//! **Rule** (`docs/design/modules/rt-core.md`): `#[cfg(target_os = …)]` appears
//! in this module and nowhere else in the crate.

use std::io;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as imp;

#[cfg(target_vendor = "apple")]
mod macos;
#[cfg(target_vendor = "apple")]
use macos as imp;

#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
mod fallback;
#[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
use fallback as imp;

mod signal;
pub use signal::on_interrupt;

/// Pin the calling thread to `core`, so a shard's tasks, its reactor, and its
/// socket buffers stay on one CPU's caches.
///
/// # Portability
/// Only Linux can actually do this. macOS/iOS removed thread-affinity control
/// (`THREAD_AFFINITY_POLICY` is ignored on Apple Silicon), and the Windows
/// implementation is not written yet. Both return
/// [`io::ErrorKind::Unsupported`].
///
/// Treat the error as advisory: shards run correctly unpinned, they just lose
/// cache locality. Callers should log and continue, not abort.
pub fn pin_current_thread_to_core(core: usize) -> io::Result<()> {
    imp::pin_current_thread_to_core(core)
}

/// Number of shards to start by default — one per available CPU.
///
/// Falls back to 1 when the OS cannot report it (containers with no cgroup
/// quota exposed, exotic targets).
pub fn default_shard_count() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// The error the unsupported platforms return.
#[allow(dead_code)] // used only by the macos/fallback backends
pub(crate) fn unsupported(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, what)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shard_count_is_at_least_one() {
        assert!(default_shard_count() >= 1);
    }

    #[test]
    fn pinning_either_succeeds_or_reports_unsupported() {
        // Never panics, and an unsupported platform is a clean, typed error
        // rather than a silent no-op the caller cannot detect.
        match pin_current_thread_to_core(0) {
            Ok(()) => {}
            Err(e) => assert_eq!(e.kind(), io::ErrorKind::Unsupported),
        }
    }

    #[test]
    fn pinning_out_of_range_core_is_an_error_not_a_panic() {
        let absurd = default_shard_count() + 4096;
        assert!(pin_current_thread_to_core(absurd).is_err());
    }
}
