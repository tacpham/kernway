//! Process plumbing: the file-descriptor limit and reading a process's RSS and
//! CPU time.

use std::process::Command;

/// Raise `RLIMIT_NOFILE`'s soft limit as far as the kernel allows and return
/// the effective `(soft, hard)` limit.
///
/// On Linux the hard limit is the ceiling. On macOS the hard limit is often
/// `RLIM_INFINITY` while `setrlimit` refuses anything above
/// `kern.maxfilesperproc`, so candidates are tried from the top down.
#[allow(unsafe_code)]
pub fn raise_fd_limit() -> (u64, u64) {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `lim` is a valid, writable rlimit for the call's duration.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
        return (0, 0);
    }
    let hard = lim.rlim_max;
    let candidates = [
        hard,
        4 << 20,
        1 << 20,
        524_288,
        262_144,
        245_760,
        131_072,
        65_536,
        32_768,
        16_384,
        10_240,
    ];
    for want in candidates {
        if want <= lim.rlim_cur || want > hard {
            continue;
        }
        let new = libc::rlimit {
            rlim_cur: want,
            rlim_max: hard,
        };
        // SAFETY: `new` is a valid rlimit; setrlimit only reads it.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &new) } == 0 {
            lim.rlim_cur = want;
            break;
        }
    }
    (lim.rlim_cur, hard)
}

/// Print the effective fd limit (both binaries do this at startup).
///
/// Returns the *effective* per-process limit: on macOS a soft limit of
/// `RLIM_INFINITY` is still capped by `kern.maxfilesperproc`.
pub fn report_fd_limit(who: &str) -> u64 {
    let (soft, hard) = raise_fd_limit();
    let show = |v: u64| {
        if v == libc::RLIM_INFINITY {
            "unlimited".to_string()
        } else {
            v.to_string()
        }
    };
    let effective = match max_files_per_proc() {
        Some(cap) => soft.min(cap),
        None => soft,
    };
    eprintln!(
        "[{who}] RLIMIT_NOFILE soft={} hard={} -> effective fd limit {effective}",
        show(soft),
        show(hard)
    );
    effective
}

/// `kern.maxfilesperproc` (macOS only): the real ceiling on open fds.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn max_files_per_proc() -> Option<u64> {
    let mut v: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>();
    // SAFETY: the name is NUL-terminated, `v`/`len` are valid for writes and
    // `len` holds `v`'s size; no new value is set.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&mut v as *mut libc::c_int).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then(|| u64::try_from(v).unwrap_or(0))
}

#[cfg(not(target_os = "macos"))]
fn max_files_per_proc() -> Option<u64> {
    None
}

/// Resident set size of `pid` in KiB, via `ps -o rss= -p <pid>` (works the same
/// on Linux and macOS; both report KiB).
#[must_use]
pub fn rss_kib(pid: u32) -> Option<u64> {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Accumulated CPU time (user + system) of `pid` in seconds, via
/// `ps -o time=` (`[[dd-]hh:]mm:ss[.ss]`).
#[must_use]
pub fn cpu_secs(pid: u32) -> Option<f64> {
    let out = Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    parse_cpu_time(String::from_utf8_lossy(&out.stdout).trim())
}

fn parse_cpu_time(s: &str) -> Option<f64> {
    if s.is_empty() {
        return None;
    }
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().ok()?, r),
        None => (0.0, s),
    };
    let mut secs = 0.0;
    for part in rest.split(':') {
        secs = secs * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(days * 86_400.0 + secs)
}

/// CPU seconds (user + system) used so far by this process.
#[allow(unsafe_code)]
#[must_use]
pub fn self_cpu_secs() -> f64 {
    // SAFETY: zeroed rusage is a valid value; getrusage fills it.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` is valid and writable.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) } != 0 {
        return 0.0;
    }
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// Start a non-blocking TCP connect to `dst` from source address `src`
/// (IPv4 only), returning the socket while the handshake is in flight.
///
/// Binding the source address is how one client box opens more connections
/// than one ephemeral-port range allows: each source IP has its own range. On
/// Linux `IP_BIND_ADDRESS_NO_PORT` defers the port choice to `connect`, so a
/// port is shared across destinations (the 4-tuple stays unique).
#[allow(unsafe_code)]
pub fn start_connect_from(
    src: std::net::Ipv4Addr,
    dst: std::net::SocketAddr,
) -> std::io::Result<std::net::TcpStream> {
    use std::io::Error;
    use std::os::fd::FromRawFd;

    let std::net::SocketAddr::V4(dst) = dst else {
        return Err(Error::new(
            std::io::ErrorKind::InvalidInput,
            "--bind supports IPv4 targets only",
        ));
    };
    let sockaddr = |ip: std::net::Ipv4Addr, port: u16| {
        // SAFETY: an all-zero sockaddr_in is a valid value.
        let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        #[cfg(any(target_os = "macos", target_os = "freebsd"))]
        {
            sa.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
        }
        sa.sin_family = libc::AF_INET as libc::sa_family_t;
        sa.sin_port = port.to_be();
        sa.sin_addr = libc::in_addr {
            s_addr: u32::from(ip).to_be(),
        };
        sa
    };
    let len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    // SAFETY: plain socket(2); the fd is owned below.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh socket we own; the TcpStream closes it on drop.
    let stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
    #[cfg(target_os = "linux")]
    {
        let one: libc::c_int = 1;
        // SAFETY: valid fd, option value points to a live c_int of the given size.
        unsafe {
            libc::setsockopt(
                fd,
                libc::IPPROTO_IP,
                libc::IP_BIND_ADDRESS_NO_PORT,
                (&one as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    let local = sockaddr(src, 0);
    // SAFETY: `local` is a valid sockaddr_in of `len` bytes.
    if unsafe { libc::bind(fd, (&local as *const libc::sockaddr_in).cast(), len) } != 0 {
        return Err(Error::last_os_error());
    }
    stream.set_nonblocking(true)?;
    let remote = sockaddr(*dst.ip(), dst.port());
    // SAFETY: `remote` is a valid sockaddr_in of `len` bytes.
    if unsafe { libc::connect(fd, (&remote as *const libc::sockaddr_in).cast(), len) } != 0 {
        let e = Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINPROGRESS) {
            return Err(e);
        }
    }
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ps_time_formats() {
        assert_eq!(parse_cpu_time("0:01.50"), Some(1.5));
        assert_eq!(parse_cpu_time("01:02:03"), Some(3723.0));
        assert_eq!(parse_cpu_time("1-00:00:01"), Some(86_401.0));
        assert_eq!(parse_cpu_time(""), None);
    }

    #[test]
    fn reads_own_rss() {
        assert!(rss_kib(std::process::id()).unwrap_or(0) > 0);
    }
}
