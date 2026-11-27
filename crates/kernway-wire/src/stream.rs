//! A buffered plaintext / TLS stream for message-framed database protocols.

use std::io;
use std::sync::Arc;

use kernway_tls::rustls::ClientConfig;
use kernway_tls::AsyncTlsStream;
use rt_net::AsyncTcpStream;

/// Bytes requested from the socket per read: one syscall drains many small
/// messages (a result set is typically one message per row).
pub const READ_CHUNK: usize = 16 * 1024;
/// A buffer grown past this (by one large message) is shrunk back once drained,
/// so a single big row does not pin megabytes per idle pooled connection.
pub const READ_RETAIN: usize = 256 * 1024;

enum Socket {
    Plain(AsyncTcpStream),
    /// A local Unix-domain socket (never upgraded to TLS).
    #[cfg(unix)]
    Unix(rt_net::AsyncUnixStream),
    /// A local Windows named pipe (never upgraded to TLS).
    #[cfg(windows)]
    Pipe(rt_net::AsyncNamedPipe),
    Tls(Box<AsyncTlsStream>),
    /// Transient, only while the plaintext socket is handed to the TLS handshake.
    Upgrading,
}

/// A socket (plaintext, or TLS once upgraded) with a read buffer and a reused
/// write buffer. The driver frames messages; this only moves bytes.
///
/// Reading: [`fill`](Self::fill) until a header is available, look at it with
/// [`peek`](Self::peek), then [`take`](Self::take) the body — a slice into the
/// buffer, valid until the next read. Writing: assemble the frame in
/// [`write_buf`](Self::write_buf) and [`flush`](Self::flush) it.
pub struct BufStream {
    socket: Socket,
    /// Bytes `[rpos, rlen)` are received but not yet consumed.
    rbuf: Vec<u8>,
    rpos: usize,
    rlen: usize,
    wbuf: Vec<u8>,
}

impl BufStream {
    /// Wrap a connected plaintext socket.
    pub fn new(tcp: AsyncTcpStream) -> Self {
        Self {
            socket: Socket::Plain(tcp),
            rbuf: vec![0; READ_CHUNK],
            rpos: 0,
            rlen: 0,
            wbuf: Vec::with_capacity(READ_CHUNK),
        }
    }

    /// Wrap a connected Unix-domain socket.
    #[cfg(unix)]
    pub fn new_unix(sock: rt_net::AsyncUnixStream) -> Self {
        Self {
            socket: Socket::Unix(sock),
            rbuf: vec![0; READ_CHUNK],
            rpos: 0,
            rlen: 0,
            wbuf: Vec::with_capacity(READ_CHUNK),
        }
    }

    /// Wrap a connected Windows named pipe.
    #[cfg(windows)]
    pub fn new_pipe(pipe: rt_net::AsyncNamedPipe) -> Self {
        Self {
            socket: Socket::Pipe(pipe),
            rbuf: vec![0; READ_CHUNK],
            rpos: 0,
            rlen: 0,
            wbuf: Vec::with_capacity(READ_CHUNK),
        }
    }

    /// Whether this is a local channel — a Unix-domain socket or a Windows
    /// named pipe — rather than a network connection.
    #[inline]
    pub fn is_local(&self) -> bool {
        match self.socket {
            #[cfg(unix)]
            Socket::Unix(_) => true,
            #[cfg(windows)]
            Socket::Pipe(_) => true,
            _ => false,
        }
    }

    /// Whether this is a Windows named pipe (MySQL does not count one as a
    /// secure transport, unlike a Unix socket).
    #[inline]
    pub fn is_pipe(&self) -> bool {
        #[cfg(windows)]
        {
            matches!(self.socket, Socket::Pipe(_))
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// The server's leaf TLS certificate (DER), `None` without TLS.
    pub fn peer_certificate(&self) -> Option<&[u8]> {
        match &self.socket {
            Socket::Tls(s) => s.peer_certificate(),
            _ => None,
        }
    }

    /// Whether the session is encrypted.
    #[inline]
    pub fn is_tls(&self) -> bool {
        matches!(self.socket, Socket::Tls(_))
    }

    /// Make at least `n` unconsumed bytes available, reading from the socket only
    /// when needed and up to [`READ_CHUNK`] (or the rest of a large message) at a
    /// time. A clean EOF before `n` bytes is an `UnexpectedEof` error.
    pub async fn fill(&mut self, n: usize) -> io::Result<()> {
        if self.rlen - self.rpos >= n {
            return Ok(());
        }
        self.release_drained();
        while self.rlen - self.rpos < n {
            // Slide the unconsumed tail to the front before reading more.
            if self.rpos > 0 {
                self.rbuf.copy_within(self.rpos..self.rlen, 0);
                self.rlen -= self.rpos;
                self.rpos = 0;
            }
            let want = self.rlen + n.max(READ_CHUNK);
            if self.rbuf.len() < want {
                self.rbuf.resize(want, 0);
            }
            let got = match &mut self.socket {
                Socket::Plain(s) => s.read(&mut self.rbuf[self.rlen..]).await?,
                Socket::Tls(s) => s.read(&mut self.rbuf[self.rlen..]).await?,
                #[cfg(unix)]
                Socket::Unix(s) => s.read(&mut self.rbuf[self.rlen..]).await?,
                #[cfg(windows)]
                Socket::Pipe(s) => s.read(&mut self.rbuf[self.rlen..]).await?,
                Socket::Upgrading => return Err(lost()),
            };
            if got == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed the connection"));
            }
            self.rlen += got;
        }
        Ok(())
    }

    /// The first `n` unconsumed bytes, without consuming them. Call
    /// [`fill`](Self::fill)`(n)` first.
    #[inline]
    pub fn peek(&self, n: usize) -> &[u8] {
        &self.rbuf[self.rpos..self.rpos + n]
    }

    /// Consume and return the next `n` bytes, in place. Call
    /// [`fill`](Self::fill)`(n)` first; the slice is valid until the next read.
    #[inline]
    pub fn take(&mut self, n: usize) -> &[u8] {
        let start = self.rpos;
        self.rpos += n;
        &self.rbuf[start..start + n]
    }

    /// Unconsumed bytes currently buffered.
    #[inline]
    pub fn buffered(&self) -> usize {
        self.rlen - self.rpos
    }

    /// Current read-buffer size (capacity in use), for tests and diagnostics.
    pub fn read_buffer_len(&self) -> usize {
        self.rbuf.len()
    }

    /// Once everything buffered is consumed, rewind the read buffer and give back
    /// memory a large message made it grow. Called at the start of every
    /// [`fill`](Self::fill); call it at the end of a response to release eagerly.
    #[inline]
    pub fn release_drained(&mut self) {
        if self.rpos == self.rlen {
            self.rpos = 0;
            self.rlen = 0;
            if self.rbuf.len() > READ_RETAIN {
                self.rbuf.truncate(READ_CHUNK);
                self.rbuf.shrink_to(READ_CHUNK);
            }
        }
    }

    /// The (cleared) write buffer to assemble the next frame in.
    #[inline]
    pub fn write_buf(&mut self) -> &mut Vec<u8> {
        self.wbuf.clear();
        &mut self.wbuf
    }

    /// The write buffer **without** clearing it: frames appended here go out
    /// together with the next [`flush`](Self::flush) (pipelined commands).
    #[inline]
    pub fn write_buf_append(&mut self) -> &mut Vec<u8> {
        &mut self.wbuf
    }

    /// Send the write buffer's contents. A buffer grown by a large frame is
    /// dropped afterwards rather than kept per connection.
    pub async fn flush(&mut self) -> io::Result<()> {
        let result = match &mut self.socket {
            Socket::Plain(s) => s.write_all(&self.wbuf).await,
            Socket::Tls(s) => s.write_all(&self.wbuf).await,
            #[cfg(unix)]
            Socket::Unix(s) => s.write_all(&self.wbuf).await,
            #[cfg(windows)]
            Socket::Pipe(s) => s.write_all(&self.wbuf).await,
            Socket::Upgrading => Err(lost()),
        };
        if self.wbuf.capacity() > READ_RETAIN {
            self.wbuf = Vec::with_capacity(READ_CHUNK);
        }
        result
    }

    /// Upgrade the plaintext session to TLS (the handshake runs on the same
    /// socket), checking the server per `config` with `server_name` for SNI and
    /// certificate validation. Nothing may be buffered: the protocol must be at
    /// the point where the server waits for the TLS client hello.
    pub async fn upgrade_tls(&mut self, config: Arc<ClientConfig>, server_name: &str) -> io::Result<()> {
        if self.buffered() != 0 {
            return Err(io::Error::other("unread bytes buffered at the TLS upgrade"));
        }
        let Socket::Plain(tcp) = std::mem::replace(&mut self.socket, Socket::Upgrading) else {
            return Err(io::Error::other("TLS upgrade on a connection that is not plaintext"));
        };
        let tls = AsyncTlsStream::connect(tcp, config, server_name).await?;
        self.socket = Socket::Tls(Box::new(tls));
        Ok(())
    }
}

impl BufStream {
    /// Start TLS over again on the same TCP connection: drop the current TLS
    /// session (without close_notify) and run a fresh handshake. Oracle TCPS
    /// listeners ask for this after the first CONNECT (a RESEND with the
    /// TLS-renegotiation flag). Nothing may be buffered.
    pub async fn renegotiate_tls(&mut self, config: Arc<ClientConfig>, server_name: &str) -> io::Result<()> {
        if self.buffered() != 0 {
            return Err(io::Error::other("unread bytes buffered at the TLS renegotiation"));
        }
        let Socket::Tls(tls) = std::mem::replace(&mut self.socket, Socket::Upgrading) else {
            return Err(io::Error::other("TLS renegotiation on a connection that is not TLS"));
        };
        let tcp = tls.into_inner();
        let tls = AsyncTlsStream::connect(tcp, config, server_name).await?;
        self.socket = Socket::Tls(Box::new(tls));
        Ok(())
    }
}

fn lost() -> io::Error {
    io::Error::other("connection lost during TLS upgrade")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A connected loopback pair: our `BufStream` and the peer's std socket.
    fn pair(ex: &rt_core::Executor) -> (BufStream, std::net::TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let tcp = ex.block_on(async move { AsyncTcpStream::connect(addr).await.unwrap() }).unwrap();
        let (peer, _) = listener.accept().unwrap();
        (BufStream::new(tcp), peer)
    }

    #[test]
    fn many_small_messages_are_cut_from_one_read_in_place() {
        let ex = rt_core::Executor::new().unwrap();
        let (mut s, mut peer) = pair(&ex);
        // 500 length-prefixed messages ("1-byte len + body"), sent in one write.
        let mut wire = Vec::new();
        for i in 0..500u32 {
            let body = format!("msg-{i}");
            wire.push(body.len() as u8);
            wire.extend_from_slice(body.as_bytes());
        }
        peer.write_all(&wire).unwrap();
        ex.block_on(async move {
            for i in 0..500u32 {
                s.fill(1).await.unwrap();
                let len = s.peek(1)[0] as usize;
                s.take(1);
                s.fill(len).await.unwrap();
                assert_eq!(s.take(len), format!("msg-{i}").as_bytes());
            }
            assert_eq!(s.buffered(), 0);
        })
        .unwrap();
    }

    #[test]
    fn a_large_message_grows_the_buffer_which_shrinks_once_drained() {
        let ex = rt_core::Executor::new().unwrap();
        let (mut s, mut peer) = pair(&ex);
        let big = vec![7u8; READ_RETAIN * 2];
        std::thread::spawn(move || {
            peer.write_all(&big).unwrap();
            peer.write_all(b"after").unwrap();
        });
        ex.block_on(async move {
            s.fill(READ_RETAIN * 2).await.unwrap();
            assert!(s.take(READ_RETAIN * 2).iter().all(|&b| b == 7));
            assert!(s.read_buffer_len() > READ_RETAIN);
            s.fill(5).await.unwrap();
            assert_eq!(s.take(5), b"after");
            s.release_drained();
            assert!(s.read_buffer_len() <= READ_RETAIN, "shrunk once drained");
        })
        .unwrap();
    }

    #[test]
    fn eof_before_a_full_message_is_an_error() {
        let ex = rt_core::Executor::new().unwrap();
        let (mut s, mut peer) = pair(&ex);
        peer.write_all(b"abc").unwrap();
        drop(peer);
        ex.block_on(async move {
            let err = s.fill(10).await.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
        })
        .unwrap();
    }

    #[test]
    fn writes_go_out_from_the_reused_buffer() {
        let ex = rt_core::Executor::new().unwrap();
        let (mut s, mut peer) = pair(&ex);
        ex.block_on(async move {
            s.write_buf().extend_from_slice(b"hello ");
            s.flush().await.unwrap();
            s.write_buf().extend_from_slice(b"world");
            s.flush().await.unwrap();
        })
        .unwrap();
        let mut got = [0u8; 11];
        std::io::Read::read_exact(&mut peer, &mut got).unwrap();
        assert_eq!(&got, b"hello world");
    }
}
