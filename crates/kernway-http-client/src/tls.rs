//! TLS for the client, by driving `rustls` over the async socket ourselves.
//!
//! `rustls` is a sync state machine, not an I/O library: you feed it the TLS bytes you
//! read from the socket ([`read_tls`](rustls::ConnectionCommon::read_tls) +
//! [`process_new_packets`](rustls::ConnectionCommon::process_new_packets)) and drain
//! the TLS bytes it wants to send ([`write_tls`](rustls::ConnectionCommon::write_tls)),
//! while reading/writing *plaintext* through its [`reader`](rustls::Reader)/
//! [`writer`](rustls::Writer). That decoupling is exactly what lets us pump it over the
//! Kernway async socket with no tokio — the same job `tokio-rustls` does for tokio.
//!
//! Server certificates are verified against Mozilla's root CAs (`webpki-roots`), so a
//! forged or wrong-host certificate fails the handshake.

use std::io::{self, Read, Write};
use std::sync::Arc;

use rt_net::AsyncTcpStream;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore};

/// Build the shared client TLS config once: the safe defaults + Mozilla roots, no
/// client certificate. Cheap to clone (an `Arc`).
pub fn default_config() -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    // Use the ring provider explicitly, so we never depend on a process-wide default
    // provider being installed.
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring provider supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
    Arc::new(config)
}

/// A TLS connection to a server: the async TCP socket plus the `rustls` state machine.
/// Exposes `read`/`write_all` matching [`AsyncTcpStream`], so the HTTP layer treats it
/// the same as a plain socket.
pub struct AsyncTlsStream {
    tcp: AsyncTcpStream,
    conn: ClientConnection,
    /// Decrypted plaintext pulled out of `rustls` but not yet handed to the caller. We
    /// drain `rustls`'s own (bounded) plaintext buffer into here after *every*
    /// `process_new_packets`, so it can never hit its `"received plaintext buffer full"`
    /// limit — which `read_tls` raises as backpressure when a fast, large download (e.g.
    /// Cloudflare pushing a multi-MB `.ts` segment in a burst of TLS records) decrypts
    /// faster than the caller reads. Treating that as fatal truncated the body mid-stream.
    inbox: Vec<u8>,
    /// Read cursor into `inbox`; bytes before it were already returned to the caller.
    inbox_pos: usize,
    /// The peer sent `close_notify` — no more plaintext will arrive.
    closed: bool,
}

impl AsyncTlsStream {
    /// Wrap `tcp` in TLS for `server_name` (used for SNI and certificate validation),
    /// completing the handshake before returning.
    pub async fn connect(
        tcp: AsyncTcpStream,
        config: Arc<ClientConfig>,
        server_name: &str,
    ) -> io::Result<Self> {
        let name = ServerName::try_from(server_name.to_string())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid server name"))?;
        let conn = ClientConnection::new(config, name).map_err(to_io)?;
        let mut stream = Self {
            tcp,
            conn,
            inbox: Vec::new(),
            inbox_pos: 0,
            closed: false,
        };
        stream.handshake().await?;
        Ok(stream)
    }

    /// Run the handshake to completion, exchanging TLS records over the socket.
    async fn handshake(&mut self) -> io::Result<()> {
        while self.conn.is_handshaking() {
            self.flush_tls().await?; // send our pending handshake records
            if !self.conn.is_handshaking() {
                break;
            }
            if self.pump_from_socket().await? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "peer closed during TLS handshake",
                ));
            }
        }
        self.flush_tls().await // drain any final records
    }

    /// Write plaintext: buffer it into `rustls`, then flush the resulting TLS records.
    pub async fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.conn.writer().write_all(buf)?;
        self.flush_tls().await
    }

    /// Read plaintext, pulling and decrypting more TLS from the socket as needed. Plaintext
    /// is served from our own `inbox` (filled by [`pump_from_socket`], which drains `rustls`
    /// eagerly so its bounded buffer never overflows on a large, fast body).
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            // 1. Serve anything already decrypted and waiting.
            if self.inbox_pos < self.inbox.len() {
                let n = buf.len().min(self.inbox.len() - self.inbox_pos);
                buf[..n].copy_from_slice(&self.inbox[self.inbox_pos..self.inbox_pos + n]);
                self.inbox_pos += n;
                if self.inbox_pos == self.inbox.len() {
                    self.inbox.clear();
                    self.inbox_pos = 0;
                }
                return Ok(n);
            }
            // 2. Nothing buffered and the peer said goodbye — clean end of stream.
            if self.closed {
                return Ok(0);
            }
            // 3. Pull more TLS; it decrypts new plaintext into `inbox` (or sets `closed`).
            if self.pump_from_socket().await? == 0 && self.inbox.is_empty() {
                // Socket EOF without a close_notify — treat as end of body.
                return Ok(0);
            }
        }
    }

    /// Drain every TLS record `rustls` wants to send, writing them to the socket.
    async fn flush_tls(&mut self) -> io::Result<()> {
        while self.conn.wants_write() {
            let mut out = Vec::new();
            self.conn.write_tls(&mut out)?; // serialise pending records into `out`
            if out.is_empty() {
                break;
            }
            self.tcp.write_all(&out).await?;
        }
        Ok(())
    }

    /// Read one chunk of TLS bytes from the socket and feed them to `rustls`, draining the
    /// decrypted plaintext into `inbox` after each step so `rustls`'s bounded plaintext
    /// buffer never fills (its `"received plaintext buffer full"` backpressure would
    /// otherwise abort a fast, large download mid-body). Returns the number of socket bytes
    /// read (`0` at EOF).
    async fn pump_from_socket(&mut self) -> io::Result<usize> {
        let mut chunk = [0u8; 8192];
        let n = self.tcp.read(&mut chunk).await?;
        if n == 0 {
            return Ok(0);
        }
        let mut cursor = &chunk[..n];
        while !cursor.is_empty() {
            self.conn.read_tls(&mut cursor)?; // consumes from `cursor`
            self.conn.process_new_packets().map_err(to_io)?;
            self.drain_plaintext()?; // keep rustls's buffer empty as we go
        }
        Ok(n)
    }

    /// Move all currently-decrypted plaintext out of `rustls` and into `inbox`. Called after
    /// every `process_new_packets` so `rustls` never has to hold more than one record's worth.
    fn drain_plaintext(&mut self) -> io::Result<()> {
        let mut tmp = [0u8; 16384];
        loop {
            match self.conn.reader().read(&mut tmp) {
                // `Ok(0)` from the reader is a clean close_notify — record it and stop.
                Ok(0) => {
                    self.closed = true;
                    return Ok(());
                }
                Ok(n) => self.inbox.extend_from_slice(&tmp[..n]),
                // Nothing more decrypted right now.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    }
}

/// Map a `rustls` error to `io::Error`.
fn to_io(e: rustls::Error) -> io::Error {
    io::Error::other(e)
}
