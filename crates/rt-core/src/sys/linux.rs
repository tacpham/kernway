//! Linux CPU affinity — `sched_setaffinity(2)`.

use std::io;

pub(super) fn pin_current_thread_to_core(core: usize) -> io::Result<()> {
    if core >= super::default_shard_count() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("core {core} is out of range"),
        ));
    }

    // SAFETY: `cpu_set_t` is a plain bitmask with no invalid representations, so
    // an all-zero value is a valid (empty) set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: `set` is a live, initialised `cpu_set_t`, and `core` was bounds-
    // checked above against the CPU count.
    unsafe { libc::CPU_SET(core, &mut set) };

    // SAFETY: pid 0 means "the calling thread"; `set` outlives the call and its
    // size is passed exactly.
    let rc = unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
