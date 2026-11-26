//! `AsyncUnixStream` — a Unix-domain socket connection driven by the shard's
//! reactor (local database servers: MySQL's `mysqld.sock`, PostgreSQL's
//! `.s.PGSQL.5432`).

use std::io::{self, Read, Write};
use std::path::Path;

use mio::Token;
use rt_core::Direction;

use crate::stream::{would_block, Readiness};

/// An async Unix-domain stream socket. Same readiness loop as
/// [`AsyncTcpStream`](crate::AsyncTcpStream): retry the syscall until it would
/// block, only then park.
pub struct AsyncUnixStream {
    inner: mio::net::UnixStream,
    token: Token,
}

impl AsyncUnixStream {
    /// Connect to the socket file at `path`. A local connect completes (or
    /// fails) at once, so it is done before the socket is made non-blocking.
    ///
    /// # Panics
    /// If called outside an executor — there is no reactor to register with.
    pub async fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        let std = std::os::unix::net::UnixStream::connect(path)?;
        std.set_nonblocking(true)?;
        let mut inner = mio::net::UnixStream::from_std(std);
        let token = rt_core::with_reactor(|r| r.register(&mut inner))?;
        Ok(Self { inner, token })
    }

    /// Read into `buf`; `Ok(0)` means the peer closed its side.
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
                0 => return Err(io::Error::new(io::ErrorKind::WriteZero, "connection accepted no more bytes")),
                n => buf = &buf[n..],
            }
        }
        Ok(())
    }
}

impl Drop for AsyncUnixStream {
    fn drop(&mut self) {
        let _ = rt_core::try_with_reactor(|r| r.deregister(&mut self.inner, self.token));
    }
}

impl std::fmt::Debug for AsyncUnixStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncUnixStream").field("token", &self.token).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echoes_over_a_socket_file() {
        let dir = std::env::temp_dir().join(format!("rt-net-unix-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("echo.sock");
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 5];
            s.read_exact(&mut buf).unwrap();
            s.write_all(&buf).unwrap();
        });
        let ex = rt_core::Executor::new().unwrap();
        let p = path.clone();
        let got = ex
            .block_on(async move {
                let mut c = AsyncUnixStream::connect(&p).await.unwrap();
                c.write_all(b"hello").await.unwrap();
                let mut buf = [0u8; 5];
                let mut n = 0;
                while n < 5 {
                    n += c.read(&mut buf[n..]).await.unwrap();
                }
                buf
            })
            .unwrap();
        assert_eq!(&got, b"hello");
        server.join().unwrap();
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_socket_file_is_an_error() {
        let ex = rt_core::Executor::new().unwrap();
        let r = ex.block_on(async { AsyncUnixStream::connect("/nonexistent/kernway.sock").await.map(|_| ()) }).unwrap();
        assert!(r.is_err());
    }
}
