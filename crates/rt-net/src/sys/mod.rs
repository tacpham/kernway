//! Platform layer — socket options that must be set *before* `bind`.
//!
//! Same rule as `rt-core`: `#[cfg(target_os = …)]` lives here and nowhere else.

use std::io;
use std::net::{SocketAddr, TcpListener};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as imp;

#[cfg(not(unix))]
mod fallback;
#[cfg(not(unix))]
use fallback as imp;

/// Can this platform give each shard its own listener on one port?
pub fn supports_reuseport() -> bool {
    imp::SUPPORTS_REUSEPORT
}

/// Does the kernel *distribute* incoming connections across those listeners?
///
/// Linux (3.9+) hashes each connection to one `SO_REUSEPORT` socket, which is
/// what makes shard-local accept queues work. The BSD/macOS option of the same
/// name only relaxes the bind conflict — it does not load-balance, so on macOS
/// the shards bind successfully but the accept distribution is lopsided.
/// Fine for development; the benchmark numbers in ROADMAP only mean something
/// on Linux.
pub fn balances_reuseport() -> bool {
    imp::BALANCES_REUSEPORT
}

/// Create a listening socket, optionally with `SO_REUSEPORT` set before bind.
pub fn bind_listener(addr: SocketAddr, backlog: i32, reuseport: bool) -> io::Result<TcpListener> {
    imp::bind_listener(addr, backlog, reuseport)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_bind_produces_a_usable_listener() {
        let listener = bind_listener("127.0.0.1:0".parse().unwrap(), 128, false).unwrap();
        let addr = listener.local_addr().unwrap();
        assert_ne!(addr.port(), 0, "the kernel must have assigned a port");
        std::net::TcpStream::connect(addr).unwrap();
        assert!(listener.accept().is_ok());
    }

    #[test]
    fn ipv6_bind_works() {
        // Skipped rather than failed on hosts without IPv6 configured.
        if let Ok(listener) = bind_listener("[::1]:0".parse().unwrap(), 128, false) {
            assert!(listener.local_addr().unwrap().is_ipv6());
        }
    }

    #[test]
    fn two_reuseport_listeners_can_share_one_port() {
        if !supports_reuseport() {
            return;
        }
        let first = bind_listener("127.0.0.1:0".parse().unwrap(), 128, true).unwrap();
        let addr = first.local_addr().unwrap();
        let second = bind_listener(addr, 128, true).expect("SO_REUSEPORT must allow the second bind");
        assert_eq!(second.local_addr().unwrap(), addr);
    }

    #[test]
    fn second_bind_without_reuseport_is_rejected() {
        let first = bind_listener("127.0.0.1:0".parse().unwrap(), 128, false).unwrap();
        let addr = first.local_addr().unwrap();
        assert!(
            bind_listener(addr, 128, false).is_err(),
            "without SO_REUSEPORT the port is exclusive"
        );
    }

    #[test]
    fn listener_is_close_on_exec() {
        // A leaked listener fd in a child process would hold the port open long
        // after the server exits.
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let listener = bind_listener("127.0.0.1:0".parse().unwrap(), 128, false).unwrap();
            // SAFETY: `fcntl(F_GETFD)` only reads flags for a live fd we own.
            let flags = unsafe { libc::fcntl(listener.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0);
            assert_ne!(flags & libc::FD_CLOEXEC, 0, "FD_CLOEXEC must be set");
        }
    }
}
