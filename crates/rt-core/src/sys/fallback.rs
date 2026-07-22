//! Platforms without an affinity backend yet — notably Windows.
//!
//! Windows *can* do this via `SetThreadAffinityMask`, but that needs a
//! `windows-sys` dependency and a machine to verify it on, so it is deliberately
//! left unimplemented rather than shipped untested. Tracked for v0.2 alongside
//! the IOCP shard bootstrap (`docs/internal/modules/rt-net.md`).

use std::io;

pub(super) fn pin_current_thread_to_core(core: usize) -> io::Result<()> {
    if core >= super::default_shard_count() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("core {core} is out of range"),
        ));
    }
    Err(super::unsupported(
        "thread-to-core pinning is not implemented for this platform",
    ))
}
