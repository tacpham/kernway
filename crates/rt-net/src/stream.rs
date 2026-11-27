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
        let mut stream = Self::from_mio(mio::net::TcpStream::connect(addr)?)?;
        loop {
            stream.park(Direction::Write).await?;
            if let Some(err) = stream.inner.take_error()? {
                return Err(err);
            }
            match stream.inner.peer_addr() {
                // Windows reports the peer of a socket that is still
                // connecting; a connected socket always has a concrete local
                // address, an unfinished one 0.0.0.0 / ::. Keep waiting.
                Ok(_) if stream.inner.local_addr().map_or(true, |a| a.ip().is_unspecified()) => continue,
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
                Err(e) if would_block(&e) => self.park(Direction::Read).await?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }

    /// Wait for one readiness edge on `direction`.
    ///
    /// A lost edge (readiness that fired while no waker was parked) is covered by
    /// the reactor remembering it (`Parked::read_ready`/`write_ready`) and honouring
    /// it at the next `park`. We deliberately do NOT `reregister` here: re-arming an
    /// already-registered fd re-delivers its *current* readiness as a fresh edge on
    /// every park, so an idle-but-signalled socket (`recvfrom` → `WouldBlock` while
    /// the poller still reports readable — e.g. a Cloudflare keep-alive) would wake,
    /// retry, `WouldBlock`, re-arm, wake … busy-looping a whole core. Register-once
    /// plus the remembered edge is enough and never spins.
    async fn park(&mut self, direction: Direction) -> io::Result<()> {
        Readiness::new(self.token, direction).await;
        Ok(())
    }

    /// Write from `buf`, returning the number of bytes accepted (may be short).
    pub async fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match self.inner.write(buf) {
                Err(e) if would_block(&e) => self.park(Direction::Write).await?,
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

    /// Write a response head and body together in one vectored write (`writev`).
    ///
    /// `head` is a small owned buffer; `body` is borrowed straight from the response
    /// — so, unlike coalescing into one buffer, the body is never copied. Handing the
    /// kernel head+body as a single `writev` still keeps them in one syscall, so a
    /// keep-alive response is not split for Nagle to stall (and `nodelay` is set
    /// anyway). Partial writes advance across the two slices; a 0-return is a closed
    /// peer. For an empty body this degenerates to writing just the head.
    pub async fn write_head_body(&mut self, head: &[u8], body: &[u8]) -> io::Result<()> {
        let total = head.len() + body.len();
        let mut written = 0usize;
        while written < total {
            // Rebuild the remaining slices each round: once the head is fully sent,
            // the first slice is empty and only the body tail remains.
            let (h, b): (&[u8], &[u8]) = if written < head.len() {
                (&head[written..], body)
            } else {
                (&[], &body[written - head.len()..])
            };
            let res = self
                .inner
                .write_vectored(&[io::IoSlice::new(h), io::IoSlice::new(b)]);
            match res {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "connection accepted no more bytes",
                    ))
                }
                Ok(n) => written += n,
                Err(e) if would_block(&e) => self.park(Direction::Write).await?,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
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

    /// Split into a read half and a write half that can be used by **two tasks at
    /// once** — one waiting to read while the other writes. That is what a
    /// full-duplex protocol such as WebSocket needs: a reader task blocked on the
    /// next frame must not stop a broadcast from being written.
    ///
    /// Both halves stay on this shard (they are `!Send`, like the stream). The
    /// reactor parks read and write wakers separately per token, so the two
    /// directions never steal each other's wake-ups. The socket is deregistered
    /// and closed when the last half is dropped.
    pub fn into_split(self) -> (TcpReadHalf, TcpWriteHalf) {
        let shared = std::rc::Rc::new(self);
        (
            TcpReadHalf {
                stream: std::rc::Rc::clone(&shared),
            },
            TcpWriteHalf { stream: shared },
        )
    }
}

/// The reading half of a split [`AsyncTcpStream`] — see [`AsyncTcpStream::into_split`].
pub struct TcpReadHalf {
    stream: std::rc::Rc<AsyncTcpStream>,
}

/// The writing half of a split [`AsyncTcpStream`] — see [`AsyncTcpStream::into_split`].
pub struct TcpWriteHalf {
    stream: std::rc::Rc<AsyncTcpStream>,
}

impl TcpReadHalf {
    /// Read into `buf`; `Ok(0)` means the peer closed its side.
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            // mio implements `Read` for `&TcpStream`: a shared borrow is enough.
            match (&self.stream.inner).read(buf) {
                Err(e) if would_block(&e) => {
                    Readiness::new(self.stream.token, Direction::Read).await;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => return other,
            }
        }
    }

    /// Address of the remote peer.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.stream.peer_addr()
    }
}

impl TcpWriteHalf {
    /// Write from `buf`, returning the number of bytes accepted (may be short).
    pub async fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match (&self.stream.inner).write(buf) {
                Err(e) if would_block(&e) => {
                    Readiness::new(self.stream.token, Direction::Write).await;
                }
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

    /// Write every buffer in order with vectored writes (`writev`): many small
    /// frames — or one shared broadcast buffer per connection — leave in as few
    /// syscalls as the kernel allows, with no copy into a joined buffer.
    ///
    /// At most [`MAX_IOV`] slices are passed per call (the POSIX `IOV_MAX`
    /// floor); short writes advance across buffer boundaries.
    pub async fn write_all_vectored(&mut self, bufs: &[&[u8]]) -> io::Result<()> {
        let (mut idx, mut off) = (0usize, 0usize);
        // Skip leading empty buffers.
        while idx < bufs.len() && bufs[idx].is_empty() {
            idx += 1;
        }
        let mut slices: Vec<io::IoSlice<'_>> = Vec::with_capacity(bufs.len().min(MAX_IOV));
        while idx < bufs.len() {
            slices.clear();
            slices.push(io::IoSlice::new(&bufs[idx][off..]));
            for b in bufs[idx + 1..].iter().take(MAX_IOV - 1) {
                slices.push(io::IoSlice::new(b));
            }
            let mut n = match (&self.stream.inner).write_vectored(&slices) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "connection accepted no more bytes",
                    ))
                }
                Ok(n) => n,
                Err(e) if would_block(&e) => {
                    Readiness::new(self.stream.token, Direction::Write).await;
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            // Advance (idx, off) by n bytes.
            while n > 0 {
                let left = bufs[idx].len() - off;
                if n >= left {
                    n -= left;
                    idx += 1;
                    off = 0;
                } else {
                    off += n;
                    n = 0;
                }
            }
            while idx < bufs.len() && bufs[idx].is_empty() {
                idx += 1;
            }
        }
        Ok(())
    }

    /// Close one or both directions of the shared socket.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.stream.shutdown(how)
    }
}

/// Most slices handed to one `writev` (POSIX guarantees `IOV_MAX` ≥ 1024 on Linux/macOS).
pub const MAX_IOV: usize = 1024;

impl std::fmt::Debug for TcpReadHalf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("TcpReadHalf").field(&self.stream).finish()
    }
}

impl std::fmt::Debug for TcpWriteHalf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("TcpWriteHalf").field(&self.stream).finish()
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

#[cfg(test)]
mod split_tests {
    use super::*;
    use rt_core::Executor;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    /// Connecting where nothing listens is an error on every OS — over IPv4
    /// and IPv6 loopback — not a stream that fails later.
    #[test]
    fn connecting_to_a_closed_port_is_refused() {
        let ex = Executor::new().unwrap();
        for addr in ["127.0.0.1:1", "[::1]:1"] {
            let addr: SocketAddr = addr.parse().unwrap();
            let r = ex.block_on(async move { AsyncTcpStream::connect(addr).await }).unwrap();
            // Which error depends on the OS and the runner's IPv6 setup
            // (refused, unreachable, even EINVAL on a macOS runner without
            // ::1); what matters is that it is one.
            if let Ok(s) = r {
                panic!("{addr}: nothing listens, yet connected: peer {:?}, local {:?}", s.peer_addr(), s.local_addr());
            }
        }
    }

    /// Several refused connects in a row on one task — what a client trying
    /// each address of a name does. On Windows the second one was taken for
    /// connected: a wake came early and `peer_addr` answers for a socket
    /// still connecting.
    #[test]
    fn refused_connects_in_a_row_are_each_refused() {
        let ex = Executor::new().unwrap();
        let results = ex
            .block_on(async {
                let mut out = Vec::new();
                for addr in ["[::1]:1", "127.0.0.1:1", "[::1]:1", "127.0.0.1:1"] {
                    let addr: SocketAddr = addr.parse().unwrap();
                    out.push((addr, AsyncTcpStream::connect(addr).await.map(|s| (s.peer_addr().ok(), s.local_addr().ok()))));
                }
                out
            })
            .unwrap();
        for (addr, r) in results {
            assert!(r.is_err(), "{addr}: nothing listens, yet connected (peer, local) = {r:?}");
        }
    }

    /// A reader task parked on `read` must not block a writer task on the same
    /// socket: the server greets the client while its reader is already waiting.
    #[test]
    fn halves_read_and_write_concurrently() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            let mut greet = [0u8; 5];
            c.read_exact(&mut greet).unwrap();
            c.write_all(b"ping").unwrap();
            greet
        });
        let (server, _) = listener.accept().unwrap();

        let ex = Executor::new().unwrap();
        let got = ex
            .block_on(async move {
                let (mut rd, mut wr) = AsyncTcpStream::from_std(server).unwrap().into_split();
                let read: Rc<RefCell<Option<Vec<u8>>>> = Rc::default();
                let slot = Rc::clone(&read);
                // The reader starts first and parks: nothing has been sent yet.
                rt_core::spawn(async move {
                    let mut buf = [0u8; 4];
                    let mut n = 0;
                    while n < 4 {
                        n += rd.read(&mut buf[n..]).await.unwrap();
                    }
                    *slot.borrow_mut() = Some(buf.to_vec());
                });
                rt_core::sleep(Duration::from_millis(20)).await;
                wr.write_all(b"hello").await.unwrap();
                for _ in 0..200 {
                    if read.borrow().is_some() {
                        break;
                    }
                    rt_core::sleep(Duration::from_millis(5)).await;
                }
                let out = read.borrow_mut().take();
                out
            })
            .unwrap();
        assert_eq!(client.join().unwrap(), *b"hello");
        assert_eq!(got.as_deref(), Some(&b"ping"[..]));
    }

    /// Vectored writes deliver every byte in order, across many buffers (more
    /// than one `writev` worth) and large ones that force short writes.
    #[test]
    fn vectored_write_delivers_everything_in_order() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut bufs: Vec<Vec<u8>> = (0..3000u32).map(|i| i.to_be_bytes().to_vec()).collect();
        bufs.insert(10, Vec::new());
        bufs.push(vec![0xAB; 4 << 20]); // big: the kernel buffer fills mid-write
        let expected: Vec<u8> = bufs.concat();
        let len = expected.len();
        let client = std::thread::spawn(move || {
            use std::io::Read as _;
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            let mut got = Vec::with_capacity(len);
            c.read_to_end(&mut got).unwrap();
            got
        });
        let (server, _) = listener.accept().unwrap();
        let ex = Executor::new().unwrap();
        ex.block_on(async move {
            let (_rd, mut wr) = AsyncTcpStream::from_std(server).unwrap().into_split();
            let refs: Vec<&[u8]> = bufs.iter().map(Vec::as_slice).collect();
            wr.write_all_vectored(&refs).await.unwrap();
            wr.shutdown(Shutdown::Write).unwrap();
        })
        .unwrap();
        assert!(client.join().unwrap() == expected);
    }

    /// Dropping both halves closes the socket: the peer reads EOF.
    #[test]
    fn dropping_both_halves_closes_the_socket() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut rest = Vec::new();
            c.read_to_end(&mut rest).unwrap();
            rest
        });
        let (server, _) = listener.accept().unwrap();
        let ex = Executor::new().unwrap();
        ex.block_on(async move {
            let (rd, mut wr) = AsyncTcpStream::from_std(server).unwrap().into_split();
            wr.write_all(b"bye").await.unwrap();
            drop(rd);
            drop(wr);
        })
        .unwrap();
        assert_eq!(client.join().unwrap(), b"bye");
    }
}
