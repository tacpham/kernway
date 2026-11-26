//! `AsyncNamedPipe` — a Windows named-pipe client driven by the shard's
//! reactor (IOCP through mio). Local database servers listen on one: MySQL's
//! `\\.\pipe\MySQL`, SQL Server's `\\.\pipe\sql\query`. It is Windows'
//! counterpart of [`AsyncUnixStream`](crate::AsyncUnixStream).

use std::io::{self, Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{FromRawHandle, IntoRawHandle};
use std::time::Duration;

use mio::windows::NamedPipe;
use mio::Token;
use rt_core::Direction;

use crate::stream::{would_block, Readiness};

/// `FILE_FLAG_OVERLAPPED`: the handle does asynchronous I/O (required by IOCP).
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
/// `ERROR_PIPE_BUSY`: every instance of the pipe is taken; try again shortly.
const ERROR_PIPE_BUSY: i32 = 231;

/// An async named-pipe client. Same readiness loop as the sockets: retry the
/// operation until it would block, only then park.
pub struct AsyncNamedPipe {
    inner: NamedPipe,
    token: Token,
}

impl AsyncNamedPipe {
    /// Open pipe `name`: a full path (`\\.\pipe\MySQL`) or a bare name
    /// (`MySQL`, opened on the local machine). While every pipe instance is
    /// busy the open is retried on a timer (up to ~5 s) — never by blocking
    /// the shard.
    ///
    /// # Panics
    /// If called outside an executor — there is no reactor to register with.
    pub async fn connect(name: &str) -> io::Result<Self> {
        let path = full_name(name);
        let mut attempts = 0;
        let file = loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(FILE_FLAG_OVERLAPPED)
                .open(&path)
            {
                Ok(f) => break f,
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && attempts < 100 => {
                    attempts += 1;
                    rt_core::time::sleep(Duration::from_millis(50)).await;
                }
                Err(e) => return Err(e),
            }
        };
        // SAFETY: the handle was just opened with FILE_FLAG_OVERLAPPED and is
        // owned by nothing else; `NamedPipe` takes ownership of it.
        let mut inner = unsafe { NamedPipe::from_raw_handle(file.into_raw_handle()) };
        let token = rt_core::with_reactor(|r| r.register(&mut inner))?;
        Ok(Self { inner, token })
    }

    /// Read into `buf`; `Ok(0)` means the server closed the pipe.
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.inner.read(buf) {
                Err(e) if would_block(&e) => Readiness::new(self.token, Direction::Read).await,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // A closed pipe reads as end of stream, like a socket.
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(0),
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
                0 => return Err(io::Error::new(io::ErrorKind::WriteZero, "pipe accepted no more bytes")),
                n => buf = &buf[n..],
            }
        }
        Ok(())
    }
}

/// `\\.\pipe\<name>` unless `name` is already a pipe path.
pub fn full_name(name: &str) -> String {
    if name.starts_with(r"\\") {
        name.to_string()
    } else {
        format!(r"\\.\pipe\{name}")
    }
}

impl Drop for AsyncNamedPipe {
    fn drop(&mut self) {
        let _ = rt_core::try_with_reactor(|r| r.deregister(&mut self.inner, self.token));
    }
}

impl std::fmt::Debug for AsyncNamedPipe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncNamedPipe").field("token", &self.token).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_names_become_local_pipe_paths() {
        assert_eq!(full_name("MySQL"), r"\\.\pipe\MySQL");
        assert_eq!(full_name(r"\\server\pipe\x"), r"\\server\pipe\x");
    }

    #[test]
    fn echoes_over_a_named_pipe() {
        let name = format!("rt-net-test-{}", std::process::id());
        let ex = rt_core::Executor::new().unwrap();
        let got = ex
            .block_on(async move {
                // Server side: a mio named pipe on the same reactor.
                let mut server = NamedPipe::new(full_name(&name)).unwrap();
                let st = rt_core::with_reactor(|r| r.register(&mut server)).unwrap();
                let _ = server.connect();
                let mut client = AsyncNamedPipe::connect(&name).await.unwrap();
                client.write_all(b"hello").await.unwrap();
                let mut buf = [0u8; 5];
                let mut n = 0;
                while n < 5 {
                    match server.read(&mut buf[n..]) {
                        Ok(k) => n += k,
                        Err(e) if would_block(&e) => Readiness::new(st, Direction::Read).await,
                        Err(e) => panic!("{e}"),
                    }
                }
                let mut w = 0;
                while w < 5 {
                    match server.write(&buf[w..]) {
                        Ok(k) => w += k,
                        Err(e) if would_block(&e) => Readiness::new(st, Direction::Write).await,
                        Err(e) => panic!("{e}"),
                    }
                }
                let mut back = [0u8; 5];
                let mut r = 0;
                while r < 5 {
                    r += client.read(&mut back[r..]).await.unwrap();
                }
                let _ = rt_core::with_reactor(|re| re.deregister(&mut server, st));
                back
            })
            .unwrap();
        assert_eq!(&got, b"hello");
    }

    #[test]
    fn a_missing_pipe_is_an_error() {
        let ex = rt_core::Executor::new().unwrap();
        let r = ex.block_on(async { AsyncNamedPipe::connect("kernway-no-such-pipe").await.map(|_| ()) }).unwrap();
        assert!(r.is_err());
    }
}
