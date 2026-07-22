//! `AsyncTcpListener` — accepts connections onto the shard that owns it.

use std::io;
use std::net::SocketAddr;

use mio::Token;
use rt_core::Direction;

use crate::stream::{would_block, AsyncTcpStream, Readiness};

/// A listening socket driven by the current shard's reactor.
pub struct AsyncTcpListener {
    inner: mio::net::TcpListener,
    token: Token,
}

impl AsyncTcpListener {
    /// Register an existing mio listener with the current shard.
    ///
    /// # Panics
    /// If called outside an executor.
    pub fn from_mio(mut inner: mio::net::TcpListener) -> io::Result<Self> {
        let token = rt_core::with_reactor(|r| r.register(&mut inner))?;
        Ok(Self { inner, token })
    }

    /// Adopt a std listener (switched to non-blocking mode).
    ///
    /// This is how a shard picks up its own `SO_REUSEPORT` listener from
    /// [`bootstrap_shards`](crate::bootstrap_shards).
    pub fn from_std(listener: std::net::TcpListener) -> io::Result<Self> {
        listener.set_nonblocking(true)?;
        Self::from_mio(mio::net::TcpListener::from_std(listener))
    }

    /// Bind directly — convenient for tests and single-shard servers. Use
    /// [`bootstrap_shards`](crate::bootstrap_shards) for the per-core setup.
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        Self::from_mio(mio::net::TcpListener::bind(addr)?)
    }

    /// Accept the next connection.
    pub async fn accept(&mut self) -> io::Result<(AsyncTcpStream, SocketAddr)> {
        loop {
            match self.inner.accept() {
                Ok((stream, addr)) => return Ok((AsyncTcpStream::from_mio(stream)?, addr)),
                Err(e) if would_block(&e) => Readiness::new(self.token, Direction::Read).await,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // A connection that died between the readiness edge and the
                // accept must not kill the accept loop — skip it and retry.
                Err(e) if is_transient_accept_error(&e) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// The address this listener is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

impl Drop for AsyncTcpListener {
    fn drop(&mut self) {
        let _ = rt_core::try_with_reactor(|r| r.deregister(&mut self.inner, self.token));
    }
}

impl std::fmt::Debug for AsyncTcpListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncTcpListener")
            .field("local", &self.inner.local_addr().ok())
            .field("token", &self.token)
            .finish()
    }
}

/// Per-connection failures that must not abort the accept loop: the client hung
/// up, or a firewall/hook rejected it.
fn is_transient_accept_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::PermissionDenied
    )
}
