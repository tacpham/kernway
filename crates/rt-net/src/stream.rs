//! `AsyncTcpStream` — a TCP connection driven by the shard's reactor.

use std::future::Future;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};

use mio::Token;
use rt_core::Direction;

/// Waits for one readiness edge on a registered source.
///
/// Parks the current waker on the first poll and completes on the second — the
/// caller then retries its syscall. A spurious wake just costs one extra retry.
pub(crate) struct Readiness {
    token: Token,
    direction: Direction,
    parked: bool,
}

impl Readiness {
    pub(crate) fn new(token: Token, direction: Direction) -> Self {
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

/// An async TCP connection.
///
/// # Readiness loop
/// Every operation retries the syscall until it succeeds or returns something
/// other than `WouldBlock`, and only parks once the kernel has actually said
/// "no more data". That ordering matters: `mio` is edge-triggered, so parking
/// before draining would wait for an edge that has already been consumed.
pub struct AsyncTcpStream {
    inner: mio::net::TcpStream,
    token: Token,
}

impl AsyncTcpStream {
    /// Register an already-connected mio stream with the current shard.
    ///
    /// # Panics
    /// If called outside an executor — there is no reactor to register with.
    pub fn from_mio(mut inner: mio::net::TcpStream) -> io::Result<Self> {
        let token = rt_core::with_reactor(|r| r.register(&mut inner))?;
        Ok(Self { inner, token })
    }

    /// Adopt a std stream (it is switched to non-blocking mode).
    pub fn from_std(stream: std::net::TcpStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Self::from_mio(mio::net::TcpStream::from_std(stream))
    }

    /// Connect to `addr`.
    ///
    /// TCP connect is asynchronous at the kernel level: the socket reports
    /// writable once the handshake resolves, and `take_error` is what
    /// distinguishes "connected" from "refused" — a writable event alone does
    /// not mean success.
    pub async fn connect(addr: SocketAddr) -> io::Result<Self> {
        let stream = Self::from_mio(mio::net::TcpStream::connect(addr)?)?;
        loop {
            Readiness::new(stream.token, Direction::Write).await;
            if let Some(err) = stream.inner.take_error()? {
                return Err(err);
            }
            match stream.inner.peer_addr() {
                Ok(_) => return Ok(stream),
                // Still in progress — wait for the next writable edge.
                Err(e) if e.kind() == io::ErrorKind::NotConnected => continue,
                Err(e) if e.raw_os_error() == Some(libc_einprogress()) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// Read into `buf`, returning the number of bytes read. `Ok(0)` means the
    /// peer closed its side.
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.inner.read(buf) {
                Err(e) if would_block(&e) => Readiness::new(self.token, Direction::Read).await,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }

    /// Write from `buf`, returning the number of bytes accepted (may be short).
    pub async fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match self.inner.write(buf) {
                Err(e) if would_block(&e) => Readiness::new(self.token, Direction::Write).await,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }

    /// Write the whole buffer, looping over short writes.
    pub async fn write_all(&mut self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            match self.write(buf).await? {
                0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "connection accepted no more bytes",
                    ))
                }
                n => buf = &buf[n..],
            }
        }
        Ok(())
    }

    /// Flush the socket. A no-op for TCP (the kernel owns the send buffer), kept
    /// so callers can write runtime-agnostic code.
    pub async fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }

    /// Close one or both directions.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.inner.shutdown(how)
    }

    /// Address of the remote peer.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.inner.peer_addr()
    }

    /// Local address of this socket.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// Disable Nagle's algorithm — worth setting for request/response traffic.
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        self.inner.set_nodelay(nodelay)
    }
}

impl Drop for AsyncTcpStream {
    fn drop(&mut self) {
        // `try_with_reactor`: a stream may outlive its executor (dropped during
        // shutdown), and there is nothing to deregister from in that case.
        let _ = rt_core::try_with_reactor(|r| r.deregister(&mut self.inner, self.token));
    }
}

impl std::fmt::Debug for AsyncTcpStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncTcpStream")
            .field("peer", &self.inner.peer_addr().ok())
            .field("token", &self.token)
            .finish()
    }
}

pub(crate) fn would_block(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::WouldBlock
}

/// `EINPROGRESS` — a connect still in flight. Only Unix reports it here.
fn libc_einprogress() -> i32 {
    #[cfg(unix)]
    {
        libc::EINPROGRESS
    }
    #[cfg(not(unix))]
    {
        -1 // never matches a real errno on this platform
    }
}
