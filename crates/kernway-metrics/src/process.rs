//! `process_*` metrics, read from the OS at scrape time.
//!
//! Names follow the Prometheus client conventions so stock dashboards work:
//! `process_resident_memory_bytes`, `process_open_fds`, `process_max_fds`,
//! `process_cpu_seconds_total`. A value the platform cannot provide cheaply is
//! omitted rather than reported as 0.

use std::fmt::Write;

use crate::expose::write_header;

#[derive(Debug, Default)]
pub(crate) struct ProcessStats {
    pub(crate) resident_bytes: Option<u64>,
    pub(crate) open_fds: Option<u64>,
    pub(crate) max_fds: Option<u64>,
    pub(crate) cpu_seconds: Option<f64>,
}

pub(crate) fn write(out: &mut String) {
    let s = collect();
    if let Some(v) = s.cpu_seconds {
        write_header(
            out,
            "process_cpu_seconds_total",
            "Total user and system CPU time spent in seconds.",
            "counter",
        );
        let _ = writeln!(out, "process_cpu_seconds_total {v}");
    }
    if let Some(v) = s.max_fds {
        write_header(
            out,
            "process_max_fds",
            "Maximum number of open file descriptors.",
            "gauge",
        );
        let _ = writeln!(out, "process_max_fds {v}");
    }
    if let Some(v) = s.open_fds {
        write_header(
            out,
            "process_open_fds",
            "Number of open file descriptors.",
            "gauge",
        );
        let _ = writeln!(out, "process_open_fds {v}");
    }
    if let Some(v) = s.resident_bytes {
        write_header(
            out,
            "process_resident_memory_bytes",
            "Resident memory size in bytes.",
            "gauge",
        );
        let _ = writeln!(out, "process_resident_memory_bytes {v}");
    }
}

#[cfg(unix)]
pub(crate) fn collect() -> ProcessStats {
    ProcessStats {
        resident_bytes: resident_bytes(),
        open_fds: open_fds(),
        max_fds: max_fds(),
        cpu_seconds: cpu_seconds(),
    }
}

// TODO: Windows (GetProcessMemoryInfo / GetProcessHandleCount) — needs a
// windows-sys dependency the workspace does not declare yet.
#[cfg(not(unix))]
pub(crate) fn collect() -> ProcessStats {
    ProcessStats::default()
}

#[cfg(unix)]
fn cpu_seconds() -> Option<f64> {
    // SAFETY: getrusage writes a plain C struct we own; zeroed is a valid initial value.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` is a valid, writable rusage.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
        return None;
    }
    let secs = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
    Some(secs(ru.ru_utime) + secs(ru.ru_stime))
}

#[cfg(unix)]
fn max_fds() -> Option<u64> {
    // SAFETY: as above — a plain C struct written by the call.
    let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: `rl` is a valid, writable rlimit.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } != 0 {
        return None;
    }
    Some(rl.rlim_cur as u64)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn resident_bytes() -> Option<u64> {
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    // SAFETY: proc_taskinfo is plain old data; zeroed is valid.
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    // SAFETY: the buffer is exactly `size` writable bytes.
    let n = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        )
    };
    (n == size).then_some(info.pti_resident_size)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn open_fds() -> Option<u64> {
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    // SAFETY: a null buffer asks for the size needed; nothing is written.
    let pid = unsafe { libc::getpid() };
    // SAFETY: null buffer + 0 size is the documented size query.
    let needed =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return None;
    }
    // The size query over-estimates (it is the fd table, not the open set), so
    // list into a buffer that large and count what was actually written.
    let capacity = needed as usize / entry + 16;
    // SAFETY: proc_fdinfo is plain old data; zeroed is valid.
    let mut buf: Vec<libc::proc_fdinfo> = vec![unsafe { std::mem::zeroed() }; capacity];
    // SAFETY: `buf` has `capacity * entry` writable bytes.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            buf.as_mut_ptr().cast(),
            (capacity * entry) as libc::c_int,
        )
    };
    (written > 0).then(|| written as u64 / entry as u64)
}

#[cfg(target_os = "linux")]
fn resident_bytes() -> Option<u64> {
    // statm: size resident shared text lib data dt — in pages.
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // SAFETY: sysconf has no preconditions.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    (page > 0).then(|| pages * page as u64)
}

#[cfg(target_os = "linux")]
fn open_fds() -> Option<u64> {
    // Minus one: the directory handle read_dir itself holds while listing.
    let n = std::fs::read_dir("/proc/self/fd").ok()?.count() as u64;
    Some(n.saturating_sub(1))
}

// TODO: the BSDs (kinfo_proc via sysctl) — not needed by any current target.
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "linux"))
))]
fn resident_bytes() -> Option<u64> {
    None
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "linux"))
))]
fn open_fds() -> Option<u64> {
    None
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn reads_plausible_process_stats() {
        let before = collect();
        let f = std::fs::File::open(file!()).or_else(|_| std::fs::File::open("Cargo.toml"));
        let after = collect();
        let rss = after.resident_bytes.unwrap();
        assert!(rss > 1 << 20, "rss {rss}");
        let (b, a) = (before.open_fds.unwrap(), after.open_fds.unwrap());
        assert!(a >= 3, "open fds {a}");
        if f.is_ok() {
            // Other test threads open files too, so only a loose check.
            assert!(a + 8 >= b);
        }
        assert!(after.max_fds.unwrap() >= a);
        assert!(after.cpu_seconds.unwrap() >= 0.0);
    }
}
