//! macOS / iOS — no thread affinity available.
//!
//! Darwin never exposed a `sched_setaffinity` equivalent. The nearest thing,
//! `thread_policy_set(THREAD_AFFINITY_POLICY)`, was only ever a scheduler *hint*
//! and is ignored outright on Apple Silicon, where the kernel places threads
//! across the P/E clusters itself.
//!
//! Reporting [`io::ErrorKind::Unsupported`] is therefore the honest answer:
//! pretending to pin would let a caller believe it had locality it does not have.

use std::io;

pub(super) fn pin_current_thread_to_core(core: usize) -> io::Result<()> {
    if core >= super::default_shard_count() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("core {core} is out of range"),
        ));
    }
    Err(super::unsupported(
        "macOS does not support thread-to-core pinning; shards run unpinned",
    ))
}
