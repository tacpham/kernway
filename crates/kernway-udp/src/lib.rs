//! # kernway-udp
//!
//! Async UDP socket for the Kernway runtime (`rt-core` / `rt-net`).
//!
//! [`AsyncUdpSocket`] is the datagram counterpart to `rt_net::AsyncTcpStream`:
//! one `mio::net::UdpSocket` registered with the current shard's reactor, driven
//! by the same readiness loop. It exists to power `kernway-dns` — a pure-async
//! DNS resolver — so `kernway-http-client` can drop its
//! `spawn_blocking(getaddrinfo)` call.
//!
//! ## Readiness loop
//!
//! UDP has no handshake, so `bind` registers immediately. Every operation retries
//! the syscall until it succeeds or returns something other than `WouldBlock`, and
//! only parks once the kernel has actually said "not ready". `mio` is
//! edge-triggered, so parking before draining would wait for an edge already
//! consumed — the same ordering rule as the TCP stream.
//!
//! ## Example
//! ```no_run
//! use rt_core::Executor;
//! use kernway_udp::AsyncUdpSocket;
//!
//! let ex = Executor::new().unwrap();
//! ex.block_on(async {
//!     let sock = AsyncUdpSocket::bind("0.0.0.0:0".parse().unwrap())?;
//!     sock.send_to(b"\x00", "8.8.8.8:53".parse().unwrap()).await?;
//!     let mut buf = [0u8; 512];
//!     let (n, from) = sock.recv_from(&mut buf).await?;
//!     let _ = (n, from);
//!     Ok::<_, std::io::Error>(())
//! }).unwrap().unwrap();
//! ```
#![deny(unsafe_op_in_unsafe_fn)]

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use mio::Token;
use rt_core::Direction;

/// Waits for one readiness edge on a registered source.
///
/// Parks the current waker on the first poll and completes on the second — the
/// caller then retries its syscall. A spurious wake just costs one extra retry.
/// (Mirrors `rt_net::stream::Readiness`, which is crate-private there.)
struct Readiness {
    token: Token,
    direction: Direction,
    parked: bool,
}

impl Readiness {
    fn new(token: Token, direction: Direction) -> Self {
        Self {
            token,
            direction,
            parked: false,
        }
    }
}

impl Future for Readiness {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.parked {
            return Poll::Ready(());
        }
        self.parked = true;
        let (token, direction) = (self.token, self.direction);
        rt_core::with_reactor(|r| r.park(token, direction, cx.waker().clone()));
        Poll::Pending
    }
}

/// An async UDP socket bound to the current shard's reactor.
///
/// All I/O methods take `&self`: `mio::net::UdpSocket` is `Sync`-friendly for
/// send/recv, so one socket can be shared (e.g. a resolver that fans out queries
/// on the same source port). The socket deregisters on drop.
pub struct AsyncUdpSocket {
    inner: mio::net::UdpSocket,
    token: Token,
}

impl AsyncUdpSocket {
    /// Bind to `addr` and register with the current shard.
    ///
    /// Use port `0` for an ephemeral source port (the usual choice for a client
    /// resolver — a random source port is also a spoofing defence).
    ///
    /// # Panics
    /// If called outside an executor — there is no reactor to register with.
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let mut inner = mio::net::UdpSocket::bind(addr)?;
        let token = rt_core::with_reactor(|r| r.register(&mut inner))?;
        Ok(Self { inner, token })
    }

    /// Set the default peer for [`send`](Self::send) / [`recv`](Self::recv).
    ///
    /// UDP `connect` only records the peer and filters incoming datagrams to it;
    /// there is no handshake, so this is a cheap non-blocking call.
    pub fn connect(&self, addr: SocketAddr) -> io::Result<()> {
        self.inner.connect(addr)
    }

    /// Send `buf` to `target`, returning the number of bytes sent.
    ///
    /// A datagram is sent whole or not at all, so a short send is not possible —
    /// the return equals `buf.len()` on success.
    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        loop {
            match self.inner.send_to(buf, target) {
                Err(e) if would_block(&e) => Readiness::new(self.token, Direction::Write).await,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }

    /// Receive one datagram into `buf`, returning `(bytes, sender)`.
    ///
    /// A datagram longer than `buf` is truncated to `buf.len()` and the rest is
    /// discarded (UDP semantics) — size `buf` for the largest reply you expect.
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        loop {
            match self.inner.recv_from(buf) {
                Err(e) if would_block(&e) => Readiness::new(self.token, Direction::Read).await,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }

    /// Send `buf` to the connected peer. Requires a prior [`connect`](Self::connect).
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match self.inner.send(buf) {
                Err(e) if would_block(&e) => Readiness::new(self.token, Direction::Write).await,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }

    /// Receive one datagram from the connected peer into `buf`.
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.inner.recv(buf) {
                Err(e) if would_block(&e) => Readiness::new(self.token, Direction::Read).await,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }

    /// The local address this socket is bound to (resolves an ephemeral `:0` port).
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

impl Drop for AsyncUdpSocket {
    fn drop(&mut self) {
        // `try_with_reactor`: a socket may outlive its executor (dropped during
        // shutdown), and there is nothing to deregister from in that case.
        let _ = rt_core::try_with_reactor(|r| r.deregister(&mut self.inner, self.token));
    }
}

impl std::fmt::Debug for AsyncUdpSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncUdpSocket")
            .field("local", &self.inner.local_addr().ok())
            .field("token", &self.token)
            .finish()
    }
}

fn would_block(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::WouldBlock
}

#[cfg(test)]
mod tests {
    use super::*;
    use rt_core::Executor;

    fn local(port_zero: &str) -> SocketAddr {
        port_zero.parse().unwrap()
    }

    #[test]
    fn send_to_and_recv_from_roundtrip() {
        let ex = Executor::new().unwrap();
        ex.block_on(async {
            let a = AsyncUdpSocket::bind(local("127.0.0.1:0")).unwrap();
            let b = AsyncUdpSocket::bind(local("127.0.0.1:0")).unwrap();
            let b_addr = b.local_addr().unwrap();

            let sent = a.send_to(b"ping", b_addr).await.unwrap();
            assert_eq!(sent, 4);

            let mut buf = [0u8; 16];
            let (n, from) = b.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"ping");
            assert_eq!(from, a.local_addr().unwrap());
        })
        .unwrap();
    }

    #[test]
    fn connected_send_and_recv() {
        let ex = Executor::new().unwrap();
        ex.block_on(async {
            let a = AsyncUdpSocket::bind(local("127.0.0.1:0")).unwrap();
            let b = AsyncUdpSocket::bind(local("127.0.0.1:0")).unwrap();
            let b_addr = b.local_addr().unwrap();
            a.connect(b_addr).unwrap();

            a.send(b"hello").await.unwrap();
            let mut buf = [0u8; 16];
            let (n, _from) = b.recv_from(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"hello");
        })
        .unwrap();
    }

    #[test]
    fn a_datagram_longer_than_the_buffer_is_truncated() {
        let ex = Executor::new().unwrap();
        ex.block_on(async {
            let a = AsyncUdpSocket::bind(local("127.0.0.1:0")).unwrap();
            let b = AsyncUdpSocket::bind(local("127.0.0.1:0")).unwrap();
            let b_addr = b.local_addr().unwrap();

            a.send_to(&[7u8; 100], b_addr).await.unwrap();
            let mut small = [0u8; 10];
            let (n, _) = b.recv_from(&mut small).await.unwrap();
            assert_eq!(n, 10, "recv truncates to the buffer length");
            assert!(small.iter().all(|&x| x == 7));
        })
        .unwrap();
    }
}
