//! Unix socket setup — `SO_REUSEADDR` / `SO_REUSEPORT` before `bind(2)`.
//!
//! `std::net::TcpListener::bind` binds immediately, so it cannot be used here:
//! `SO_REUSEPORT` is only meaningful when set on the socket *before* it binds.
//! Hence the raw `socket`/`setsockopt`/`bind`/`listen` sequence.

use std::io;
use std::mem;
use std::net::{SocketAddr, TcpListener};
use std::os::fd::{FromRawFd, OwnedFd};

pub(super) const SUPPORTS_REUSEPORT: bool = true;

/// Only Linux hashes connections across `SO_REUSEPORT` sockets; the BSD option
/// merely permits the shared bind. See `super::balances_reuseport`.
pub(super) const BALANCES_REUSEPORT: bool = cfg!(target_os = "linux");

pub(super) fn bind_listener(
    addr: SocketAddr,
    backlog: i32,
    reuseport: bool,
) -> io::Result<TcpListener> {
    let domain = match addr {
        SocketAddr::V4(_) => libc::AF_INET,
        SocketAddr::V6(_) => libc::AF_INET6,
    };

    // SAFETY: a plain `socket(2)` call with constant, valid arguments.
    let fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Take ownership immediately so every `?` below closes the fd.
    // SAFETY: `fd` is a fresh, valid, exclusively-owned descriptor.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    set_cloexec(fd)?;
    // Lets a restarted server rebind while old connections sit in TIME_WAIT.
    setsockopt_bool(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, true)?;
    if reuseport {
        setsockopt_bool(fd, libc::SOL_SOCKET, libc::SO_REUSEPORT, true)?;
    }

    bind_addr(fd, addr)?;

    // SAFETY: `fd` is a valid bound socket; `backlog` is an ordinary int.
    if unsafe { libc::listen(fd, backlog) } < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(TcpListener::from(owned))
}

/// Without `FD_CLOEXEC` the listener leaks into every child process, which keeps
/// the port occupied after the server exits.
fn set_cloexec(fd: i32) -> io::Result<()> {
    // SAFETY: `F_GETFD` only reads the flags of a descriptor we own.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: sets exactly one additional flag on the same owned descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn setsockopt_bool(fd: i32, level: i32, name: i32, value: bool) -> io::Result<()> {
    let value: libc::c_int = value.into();
    // SAFETY: `value` is a live `c_int` for the duration of the call and its
    // size is passed exactly — the layout every SOL_SOCKET boolean option wants.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            level,
            name,
            std::ptr::addr_of!(value).cast(),
            mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn bind_addr(fd: i32, addr: SocketAddr) -> io::Result<()> {
    let rc = match addr {
        SocketAddr::V4(v4) => {
            // SAFETY: `sockaddr_in` is a POD struct; all-zero is a valid value
            // and every field that matters is written below.
            let mut raw: libc::sockaddr_in = unsafe { mem::zeroed() };
            raw.sin_family = libc::AF_INET as libc::sa_family_t;
            raw.sin_port = v4.port().to_be();
            // `octets()` is already network byte order, so reading them as a
            // native u32 yields the correct `in_addr` on either endianness.
            raw.sin_addr.s_addr = u32::from_ne_bytes(v4.ip().octets());
            set_sin_len(&mut raw);
            // SAFETY: `raw` is a fully initialised `sockaddr_in` and the length
            // passed matches its type exactly.
            unsafe {
                libc::bind(
                    fd,
                    std::ptr::addr_of!(raw).cast(),
                    mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }
        SocketAddr::V6(v6) => {
            // SAFETY: as above, for `sockaddr_in6`.
            let mut raw: libc::sockaddr_in6 = unsafe { mem::zeroed() };
            raw.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            raw.sin6_port = v6.port().to_be();
            raw.sin6_addr.s6_addr = v6.ip().octets();
            raw.sin6_flowinfo = v6.flowinfo();
            raw.sin6_scope_id = v6.scope_id();
            set_sin6_len(&mut raw);
            // SAFETY: as above.
            unsafe {
                libc::bind(
                    fd,
                    std::ptr::addr_of!(raw).cast(),
                    mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// BSD-derived kernels (macOS included) carry a length byte in the sockaddr.
// Linux has no such field, so these are no-ops there.

#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn set_sin_len(raw: &mut libc::sockaddr_in) {
    raw.sin_len = mem::size_of::<libc::sockaddr_in>() as u8;
}

#[cfg(not(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
fn set_sin_len(_raw: &mut libc::sockaddr_in) {}

#[cfg(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn set_sin6_len(raw: &mut libc::sockaddr_in6) {
    raw.sin6_len = mem::size_of::<libc::sockaddr_in6>() as u8;
}

#[cfg(not(any(
    target_vendor = "apple",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
fn set_sin6_len(_raw: &mut libc::sockaddr_in6) {}
