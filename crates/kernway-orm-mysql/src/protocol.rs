//! A self-written MySQL client protocol — no external MySQL client crate.
//!
//! This speaks the classic MySQL protocol directly over the per-core async
//! reactor ([`rt_net::AsyncTcpStream`]), optionally upgraded to TLS. Connections
//! are `!Send` and live on one shard, like the rest of the thread-per-core stack.
//!
//! ## Scope
//!
//! - Handshake v10; auth with `mysql_native_password` (SHA-1 scramble) or
//!   `caching_sha2_password` (MySQL 8's default: SHA-256 fast auth, and full auth
//!   over TLS), including server-requested auth switches.
//! - TLS via `SSLRequest` + rustls, per [`SslMode`] (mirrors the official
//!   client's `--ssl-mode`).
//! - `COM_QUERY` (text protocol) and prepared statements (binary protocol, cached
//!   per connection) for server-side parameter binding.
//! - Buffered packet I/O: one read syscall serves many small packets; rows are
//!   decoded in place, straight into the caller's type.
//! - Multi-result responses (`CLIENT_MULTI_RESULTS`): stored procedures via
//!   [`Conn::call_results`] / [`Conn::query_multi`]; every response is read to its end,
//!   so an error part-way never leaves the connection out of sync.
//!
//! - `caching_sha2_password` full authentication without TLS: the password
//!   RSA-OAEP-encrypted with the server's public key (given as a file, or
//!   fetched when explicitly allowed).
//! - Client certificates (mutual TLS) for accounts that `REQUIRE X509`.
//! - `LOAD DATA LOCAL INFILE` from caller-supplied bytes only, and
//!   multi-statement scripts enabled per call (`COM_SET_OPTION`).
//! - `OUT` / `INOUT` procedure arguments via the binary protocol.

use kernway_orm_core::error::OrmError;
pub use kernway_orm_core::decode::Cell;
use kernway_orm_core::decode::recycle;
use kernway_tls::CertCheck;
use kernway_wire::{BufStream, StmtCache, READ_RETAIN};
use rt_net::AsyncTcpStream;
use std::net::ToSocketAddrs;

// --- Capability flags (only the ones we assert) ---
const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
/// Report rows *matched* (not only changed) by UPDATE, like other databases
/// and JDBC's default — so "0 rows" always means "no such row".
const CLIENT_FOUND_ROWS: u32 = 0x0000_0002;
const CLIENT_LONG_FLAG: u32 = 0x0000_0004;
const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
const CLIENT_SSL: u32 = 0x0000_0800;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_TRANSACTIONS: u32 = 0x0000_2000;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;
/// Accept several result sets per response — required for a `CALL` whose
/// procedure `SELECT`s (else the server refuses with error 1312).
const CLIENT_MULTI_RESULTS: u32 = 0x0002_0000;
/// The same for a `CALL` run as a prepared statement.
const CLIENT_PS_MULTI_RESULTS: u32 = 0x0004_0000;
/// Server status flag: another result follows this one in the response.
const SERVER_MORE_RESULTS_EXISTS: u16 = 0x0008;
/// Status flag: this result set holds a procedure's `OUT` parameters.
const SERVER_PS_OUT_PARAMS: u16 = 0x1000;
/// Client capability: may answer `LOAD DATA LOCAL INFILE` requests (only
/// ever with data the caller handed to [`Conn::load_local_infile`]).
const CLIENT_LOCAL_FILES: u32 = 0x0000_0080;
/// Compressed protocol (zlib), switched on after authentication.
const CLIENT_COMPRESS: u32 = 0x0000_0020;
/// Payloads shorter than this travel uncompressed inside a compressed frame,
/// as the official client does.
const MIN_COMPRESS_LENGTH: usize = 50;
/// Packet size for `LOAD DATA LOCAL INFILE` data.
const LOCAL_INFILE_CHUNK: usize = 1 << 20;

const UTF8MB4_GENERAL_CI: u8 = 45;
const MAX_PACKET: u32 = 16 * 1024 * 1024; // 16 MiB

fn conn_err(msg: impl Into<String>) -> OrmError {
    OrmError::Connection(msg.into())
}

/// Connection parameters, usually parsed from a `mysql://` URL.
#[derive(Debug, Clone)]
pub struct MysqlConfig {
    /// Host to connect to.
    pub host: String,
    /// TCP port (3306 by default).
    pub port: u16,
    /// Account name.
    pub user: String,
    /// Account password (empty string for none).
    pub password: String,
    /// Default database to `USE` on connect, if any.
    pub database: Option<String>,
    /// Whether and how to use TLS (URL: `?ssl-mode=`). Default [`SslMode::Preferred`].
    pub ssl_mode: SslMode,
    /// PEM file with the CA(s) to trust for [`SslMode::VerifyCa`] /
    /// [`SslMode::VerifyIdentity`] (URL: `?ssl-ca=`). `None` = Mozilla's public roots.
    pub ssl_ca: Option<std::path::PathBuf>,
    /// PEM client certificate for servers that require one (`REQUIRE X509`;
    /// URL: `?ssl-cert=`). Needs [`ssl_key`](Self::ssl_key) too.
    pub ssl_cert: Option<std::path::PathBuf>,
    /// PEM private key of [`ssl_cert`](Self::ssl_cert) (URL: `?ssl-key=`).
    pub ssl_key: Option<std::path::PathBuf>,
    /// PEM RSA public key of the server, for `caching_sha2_password` full
    /// authentication **without TLS** (URL: `?server-public-key=`). The
    /// password is RSA-encrypted with it.
    pub server_public_key: Option<std::path::PathBuf>,
    /// Without TLS and without [`server_public_key`](Self::server_public_key),
    /// ask the server for its public key (URL: `?get-server-public-key=true`).
    /// Off by default, as in the official client: over plaintext the key
    /// itself could be swapped by a man in the middle.
    pub get_server_public_key: bool,
    /// Connect over the server's local channel instead of TCP (URL:
    /// `?socket=`): on Unix the socket file (`/var/run/mysqld/mysqld.sock`),
    /// on Windows the named pipe (`MySQL`, or `\\.\pipe\MySQL`; the server
    /// needs `enable-named-pipe`). The channel is local, so it is trusted like
    /// TLS: no TLS is negotiated over it (an `ssl-mode` that requires TLS is
    /// refused) and `caching_sha2_password` full authentication sends the
    /// password as the official client does.
    pub socket: Option<std::path::PathBuf>,
    /// Use the compressed protocol (zlib) when the server offers it (URL:
    /// `?compress=true`). Worth it for large results over slow links; it
    /// costs CPU on both sides.
    pub compress: bool,
}

/// TLS policy, with the same names and meaning as the official client's
/// `--ssl-mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SslMode {
    /// Never use TLS.
    Disabled,
    /// Use TLS if the server offers it, without checking its certificate; fall
    /// back to plaintext otherwise. The default, as in the official client.
    #[default]
    Preferred,
    /// Require TLS; do not check the certificate (encryption only).
    Required,
    /// Require TLS and a certificate that chains to a trusted CA (any name).
    VerifyCa,
    /// Require TLS, a trusted chain, and a certificate naming the host.
    VerifyIdentity,
}

impl SslMode {
    /// Parse `disabled` / `preferred` / `required` / `verify_ca` /
    /// `verify_identity` (case-insensitive; `-` and `_` both accepted).
    pub fn parse(s: &str) -> Result<Self, OrmError> {
        match s.to_ascii_lowercase().replace('-', "_").as_str() {
            "disabled" => Ok(Self::Disabled),
            "preferred" => Ok(Self::Preferred),
            "required" => Ok(Self::Required),
            "verify_ca" => Ok(Self::VerifyCa),
            "verify_identity" => Ok(Self::VerifyIdentity),
            other => Err(conn_err(format!("unknown ssl-mode '{other}'"))),
        }
    }

    /// TLS is mandatory: no plaintext fallback.
    fn requires_tls(self) -> bool {
        matches!(self, Self::Required | Self::VerifyCa | Self::VerifyIdentity)
    }

    fn cert_check(self) -> CertCheck {
        match self {
            Self::VerifyIdentity => CertCheck::Full,
            Self::VerifyCa => CertCheck::CaOnly,
            Self::Disabled | Self::Preferred | Self::Required => CertCheck::None,
        }
    }
}

impl MysqlConfig {
    /// Parse `mysql://user:password@host:port/database[?ssl-mode=…&ssl-ca=…]`.
    ///
    /// `port` defaults to 3306, `database` is optional, and `ssl-mode` defaults to
    /// `preferred` (see [`SslMode`]); `ssl-ca` names a PEM file of trusted CAs. Credentials and the
    /// database are percent-decoded only for the few characters a password
    /// commonly needs; anything exotic should be passed via [`MysqlConfig`]
    /// directly.
    pub fn from_url(url: &str) -> Result<Self, OrmError> {
        let rest = url
            .strip_prefix("mysql://")
            .ok_or_else(|| conn_err("MySQL URL must start with mysql://"))?;

        // Split credentials from the authority/path at the last '@'.
        let (creds, host_part) = match rest.rsplit_once('@') {
            Some((c, h)) => (c, h),
            None => ("", rest),
        };
        let (user, password) = match creds.split_once(':') {
            Some((u, p)) => (u.to_string(), p.to_string()),
            None => (creds.to_string(), String::new()),
        };

        // host_part = host[:port][/database][?options]
        let (host_part, options) = match host_part.split_once('?') {
            Some((h, o)) => (h, o),
            None => (host_part, ""),
        };
        let mut ssl_mode = SslMode::default();
        let mut ssl_ca = None;
        let (mut ssl_cert, mut ssl_key, mut server_public_key) = (None, None, None);
        let mut get_server_public_key = false;
        let mut socket = None;
        let mut compress = false;
        let flag = |v: &str| matches!(v.to_ascii_lowercase().as_str(), "true" | "1" | "yes");
        for pair in options.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key.to_ascii_lowercase().replace('_', "-").as_str() {
                "ssl-mode" | "sslmode" => ssl_mode = SslMode::parse(value)?,
                "ssl-ca" => ssl_ca = Some(std::path::PathBuf::from(value)),
                "ssl-cert" => ssl_cert = Some(std::path::PathBuf::from(value)),
                "ssl-key" => ssl_key = Some(std::path::PathBuf::from(value)),
                "server-public-key" => server_public_key = Some(std::path::PathBuf::from(value)),
                "get-server-public-key" => get_server_public_key = flag(value),
                "socket" => socket = Some(std::path::PathBuf::from(value)),
                "compress" => compress = flag(value),
                other => return Err(conn_err(format!("unknown MySQL URL option '{other}'"))),
            }
        }
        let (hostport, database) = match host_part.split_once('/') {
            Some((hp, db)) => (hp, if db.is_empty() { None } else { Some(db.to_string()) }),
            None => (host_part, None),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse::<u16>().map_err(|_| conn_err("invalid port in MySQL URL"))?,
            ),
            None => (hostport.to_string(), 3306),
        };
        if host.is_empty() {
            return Err(conn_err("missing host in MySQL URL"));
        }
        if ssl_cert.is_some() != ssl_key.is_some() {
            return Err(conn_err("ssl-cert and ssl-key must be given together"));
        }
        Ok(Self {
            host,
            port,
            user,
            password,
            database,
            ssl_mode,
            ssl_ca,
            ssl_cert,
            ssl_key,
            server_public_key,
            get_server_public_key,
            socket,
            compress,
        })
    }
}

/// Outcome of a single statement.
#[derive(Debug)]
pub enum QueryResult {
    /// No result set — `INSERT` / `UPDATE` / `DELETE` / `BEGIN` / `COMMIT` / …
    Ok {
        /// Rows affected by the statement.
        affected_rows: u64,
        /// Auto-increment value generated, or 0.
        last_insert_id: u64,
    },
    /// A result set: column names and text-encoded rows (`None` = SQL NULL).
    Rows {
        /// Column names in `SELECT` order.
        columns: Vec<String>,
        /// Each row's values, one `Option<String>` per column.
        rows: Vec<Vec<Option<String>>>,
    },
}

/// Outcome of a statement whose rows were decoded by a caller-supplied
/// function (see [`Conn::query_with`] / [`Conn::exec_prepared_with`]).
#[derive(Debug)]
pub enum Fetched<R> {
    /// No result set.
    Ok {
        /// Rows affected by the statement.
        affected_rows: u64,
        /// Auto-increment value generated, or 0.
        last_insert_id: u64,
    },
    /// A result set: column names and one decoded value per row.
    Rows {
        /// Column names in `SELECT` order.
        columns: Vec<String>,
        /// What the row function returned for each row.
        rows: Vec<R>,
        /// Whether this result set carries a procedure's `OUT` parameter
        /// values (binary protocol, `SERVER_PS_OUT_PARAMS`).
        out_params: bool,
    },
}

impl<R> Fetched<R> {
    /// The decoded rows; empty for a statement without a result set.
    pub fn into_rows(self) -> Vec<R> {
        match self {
            Fetched::Rows { rows, .. } => rows,
            Fetched::Ok { .. } => Vec::new(),
        }
    }
}

impl From<Fetched<Vec<Option<String>>>> for QueryResult {
    fn from(f: Fetched<Vec<Option<String>>>) -> Self {
        match f {
            Fetched::Ok { affected_rows, last_insert_id } => {
                QueryResult::Ok { affected_rows, last_insert_id }
            }
            Fetched::Rows { columns, rows, .. } => QueryResult::Rows { columns, rows },
        }
    }
}

/// A row as text cells — the [`QueryResult`] shape.
fn cells_to_text(cells: &[Cell<'_>]) -> Result<Vec<Option<String>>, OrmError> {
    Ok(cells.iter().map(Cell::to_text).collect())
}

/// Prepared statements kept per connection. Past this the oldest is closed on
/// the server, bounding server-side statement memory (`max_prepared_stmt_count`
/// is global, ~16k by default) under dynamic SQL such as varying `IN` lists.
const STMT_CACHE_CAP: usize = 256;
/// `ER_UNKNOWN_STMT_HANDLER`: the server no longer knows a cached statement id.
const ER_UNKNOWN_STMT_HANDLER: u16 = 1243;
/// `ER_NEED_REPREPARE`: the statement must be re-prepared (e.g. after DDL).
const ER_NEED_REPREPARE: u16 = 1615;

/// A server-side prepared statement, cached by its SQL text.
#[derive(Clone, Copy)]
struct Stmt {
    id: u32,
    num_params: u16,
    /// Columns of its result set (0 for DML).
    num_columns: u16,
}

/// A live, authenticated MySQL connection over the per-core async reactor.
pub struct Conn {
    /// The buffered plaintext / TLS socket (shared driver plumbing).
    io: BufStream,
    seq: u8,
    /// Assembly area for a payload split over 16 MiB continuation packets.
    big: Vec<u8>,
    /// Prepared statements by SQL text (bounded; the oldest is closed).
    stmts: StmtCache<Stmt>,
    /// Data for the `LOAD DATA LOCAL INFILE` request of the statement in
    /// flight (see [`Conn::load_local_infile`]); `None` refuses any request.
    local_data: Option<Vec<u8>>,
    /// The compressed-protocol layer, once negotiated.
    z: Option<Box<Compressed>>,
    /// Server status flags of the last OK / EOF packet (`SERVER_STATUS_*`).
    status: u16,
    /// Open server-side cursors by id (`SqlConn::cursor_open`).
    pub(crate) cursors: std::collections::HashMap<u64, CursorState>,
    pub(crate) next_cursor: u64,
}

/// Compressed protocol state: decompressed bytes not yet consumed by the
/// packet reader, and the compressed-frame sequence id.
#[derive(Default)]
struct Compressed {
    inb: Vec<u8>,
    pos: usize,
    seq: u8,
    /// Plain frames waiting to be compressed into the socket's write buffer.
    out: Vec<u8>,
}

impl Compressed {
    fn avail(&self) -> usize {
        self.inb.len() - self.pos
    }

    /// Drop consumed bytes (and memory a large frame made the buffer grow).
    fn compact(&mut self) {
        if self.pos == self.inb.len() {
            self.inb.clear();
            self.pos = 0;
            if self.inb.capacity() > READ_RETAIN {
                self.inb = Vec::new();
            }
        } else if self.pos > 0 {
            self.inb.drain(..self.pos);
            self.pos = 0;
        }
    }
}

/// Server status flag: a transaction is open.
const SERVER_STATUS_IN_TRANS: u16 = 0x0001;

impl Conn {
    /// Connect, authenticate, and select the default database if one was given.
    ///
    /// Nothing here blocks the shard: a host name is resolved on the blocking
    /// pool (an IP literal is just parsed), files (CA, client certificate,
    /// server key) are read there too, and every socket is non-blocking.
    pub async fn connect(cfg: &MysqlConfig) -> Result<Self, OrmError> {
        if let Some(local) = &cfg.socket {
            return Self::connect_local(cfg, local).await;
        }
        let addr = resolve(&cfg.host, cfg.port).await?;
        let stream = AsyncTcpStream::connect(addr)
            .await
            .map_err(|e| conn_err(format!("connect {}:{} failed: {e}", cfg.host, cfg.port)))?;
        let mut conn = Self::over(BufStream::new(stream));
        conn.handshake(cfg).await?;
        Ok(conn)
    }

    /// Connect over the server's local channel: a Unix-domain socket file on
    /// Unix, a named pipe on Windows (`socket=` names either).
    async fn connect_local(cfg: &MysqlConfig, local: &std::path::Path) -> Result<Self, OrmError> {
        if cfg.ssl_mode.requires_tls() {
            return Err(conn_err(format!(
                "ssl-mode={:?} over a local socket / named pipe: the channel is local and \
                 is not encrypted; use ssl-mode=preferred or disabled with socket=",
                cfg.ssl_mode
            )));
        }
        let io = open_local(local).await.map_err(|e| conn_err(format!("connect {} failed: {e}", local.display())))?;
        let mut conn = Self::over(io);
        conn.handshake(cfg).await?;
        Ok(conn)
    }

    fn over(io: BufStream) -> Self {
        Self {
            io,
            seq: 0,
            big: Vec::new(),
            stmts: StmtCache::new(STMT_CACHE_CAP),
            local_data: None,
            z: None,
            status: 0,
            cursors: std::collections::HashMap::new(),
            next_cursor: 1,
        }
    }

    /// Whether the session uses the compressed protocol.
    pub fn is_compressed(&self) -> bool {
        self.z.is_some()
    }

    /// Whether the connection runs over the server's local channel (a
    /// Unix-domain socket or a Windows named pipe) rather than TCP.
    pub fn is_local(&self) -> bool {
        self.io.is_local()
    }

    /// Whether the server reported an open transaction in its last reply.
    pub fn in_transaction(&self) -> bool {
        self.status & SERVER_STATUS_IN_TRANS != 0
    }

    /// Each command starts a fresh sequence (and compressed sequence).
    fn start_command(&mut self) {
        self.seq = 0;
        if let Some(z) = &mut self.z {
            z.seq = 0;
        }
    }

    /// Make at least `n` unconsumed bytes available in the read buffer.
    async fn fill(&mut self, n: usize) -> Result<(), OrmError> {
        let Some(z) = self.z.as_deref_mut() else {
            return self.io.fill(n).await.map_err(|e| conn_err(format!("read: {e}")));
        };
        if z.avail() >= n {
            return Ok(());
        }
        z.compact();
        while z.avail() < n {
            // Compressed frame: 3-byte compressed length, sequence id, 3-byte
            // uncompressed length (0 = sent as is), then the payload.
            self.io.fill(7).await.map_err(|e| conn_err(format!("read: {e}")))?;
            let h = self.io.take(7);
            let clen = (h[0] as usize) | (h[1] as usize) << 8 | (h[2] as usize) << 16;
            let ulen = (h[4] as usize) | (h[5] as usize) << 8 | (h[6] as usize) << 16;
            z.seq = h[3].wrapping_add(1);
            self.io.fill(clen).await.map_err(|e| conn_err(format!("read: {e}")))?;
            let body = self.io.take(clen);
            if ulen == 0 {
                z.inb.extend_from_slice(body);
            } else if ulen <= OFFLOAD_BYTES {
                inflate_into(body, ulen, &mut z.inb)?;
            } else {
                // A large frame inflates on the blocking pool, not the shard.
                let body = body.to_vec();
                let plain = rt_core::spawn_blocking(move || {
                    let mut out = Vec::with_capacity(ulen);
                    inflate_into(&body, ulen, &mut out).map(|()| out)
                })
                .await
                .ok_or_else(|| conn_err("decompress: worker panicked"))??;
                z.inb.extend_from_slice(&plain);
            }
        }
        Ok(())
    }

    /// Consume the next `n` buffered bytes (after [`fill`](Self::fill)).
    fn take(&mut self, n: usize) -> &[u8] {
        match self.z.as_deref_mut() {
            Some(z) => {
                let start = z.pos;
                z.pos += n;
                &z.inb[start..start + n]
            }
            None => self.io.take(n),
        }
    }

    // --- Packet framing ---------------------------------------------------

    /// Once every buffered byte is consumed, rewind the read buffer — and give
    /// back memory a large packet made it (or the assembly area) grow.
    fn release_drained(&mut self) {
        self.io.release_drained();
        if let Some(z) = self.z.as_deref_mut() {
            if z.pos == z.inb.len() {
                z.compact();
            }
        }
        if self.big.capacity() > READ_RETAIN {
            self.big = Vec::new();
        }
    }

    /// Read one logical packet payload **in place**: the slice points into the
    /// read buffer and stays valid until the next read. A payload split over
    /// 16 MiB continuation packets is joined in a side buffer instead.
    async fn read_packet_ref(&mut self) -> Result<&[u8], OrmError> {
        self.fill(4).await?;
        let mut len = self.take_header();
        if len < 0xFF_FFFF {
            self.fill(len).await?;
            return Ok(self.take(len));
        }
        self.big.clear();
        loop {
            self.fill(len).await?;
            let chunk = match self.z.as_deref_mut() {
                Some(z) => {
                    z.pos += len;
                    &z.inb[z.pos - len..z.pos]
                }
                None => self.io.take(len),
            };
            self.big.extend_from_slice(chunk);
            if len < 0xFF_FFFF {
                return Ok(&self.big);
            }
            self.fill(4).await?;
            len = self.take_header();
        }
    }

    /// Consume a 4-byte packet header: returns the payload length and advances
    /// the sequence id.
    fn take_header(&mut self) -> usize {
        let h = self.take(4);
        let len = (h[0] as usize) | (h[1] as usize) << 8 | (h[2] as usize) << 16;
        self.seq = h[3].wrapping_add(1);
        len
    }

    /// Read one logical packet as an owned payload (handshake, OK/ERR, the
    /// first packet of a response) — one allocation, one copy.
    async fn read_packet(&mut self) -> Result<Vec<u8>, OrmError> {
        let payload = self.read_packet_ref().await?.to_vec();
        self.release_drained();
        Ok(payload)
    }

    /// Write one packet, using (and advancing) the current sequence id. The frame
    /// is assembled in a reused buffer, so steady-state writes do not allocate.
    async fn write_packet(&mut self, payload: &[u8]) -> Result<(), OrmError> {
        // A payload of 16 MiB - 1 or more is split into maximum-size packets,
        // ended by a shorter one (empty if the size is an exact multiple).
        let mut rest = payload;
        loop {
            let len = rest.len().min(0xFF_FFFF);
            let seq = self.seq;
            let header = [len as u8, (len >> 8) as u8, (len >> 16) as u8, seq];
            match self.z.as_deref_mut() {
                None => {
                    let frame = self.io.write_buf();
                    frame.extend_from_slice(&header);
                    frame.extend_from_slice(&rest[..len]);
                }
                Some(z) => {
                    z.out.clear();
                    z.out.extend_from_slice(&header);
                    z.out.extend_from_slice(&rest[..len]);
                    self.io.write_buf();
                    self.compress_queued().await?;
                }
            }
            self.seq = self.seq.wrapping_add(1);
            self.io
                .flush()
                .await
                .map_err(|e| conn_err(format!("write packet: {e}")))?;
            rest = &rest[len..];
            if len < 0xFF_FFFF {
                return Ok(());
            }
        }
    }

    // --- Connection phase -------------------------------------------------

    async fn handshake(&mut self, cfg: &MysqlConfig) -> Result<(), OrmError> {
        let greeting = self.read_packet().await?;
        let hello = parse_handshake(&greeting)?;

        // TLS first, if wanted: an SSLRequest (the handshake response's fixed
        // header with CLIENT_SSL), then the TLS handshake on the same socket. The
        // real handshake response, with the credentials, then travels encrypted.
        let server_tls = hello.capabilities & CLIENT_SSL != 0;
        if cfg.ssl_mode.requires_tls() && !server_tls {
            return Err(conn_err(format!(
                "ssl-mode={:?} but the server does not support TLS",
                cfg.ssl_mode
            )));
        }
        let use_tls = cfg.ssl_mode != SslMode::Disabled && server_tls && !self.io.is_local();
        let compress = cfg.compress && hello.capabilities & CLIENT_COMPRESS != 0;
        let compress_cap = if compress { CLIENT_COMPRESS } else { 0 };
        if use_tls {
            // The SSLRequest must announce the same capabilities as the
            // handshake response that follows (the server keeps these).
            self.write_packet(&handshake_header(cfg, CLIENT_SSL | compress_cap)).await?;
            self.upgrade_to_tls(cfg).await?;
        }

        // Answer with the plugin the server proposed; anything we do not speak
        // gets a native-password answer, which the server may switch away from.
        let plugin = match hello.plugin.as_str() {
            "caching_sha2_password" => "caching_sha2_password",
            _ => "mysql_native_password",
        };
        let auth = auth_response(plugin, cfg.password.as_bytes(), &hello.salt);
        let extra = if use_tls { CLIENT_SSL } else { 0 } | compress_cap;
        self.write_packet(&build_handshake_response(cfg, &auth, plugin, extra))
            .await?;
        // A local socket is as private as TLS (the official client treats
        // it the same way for the clear-text password exchange).
        let secure = use_tls || self.io.is_local();
        self.finish_auth(cfg, plugin, secure, hello.salt).await?;
        // Every packet after the authentication OK travels compressed.
        if compress {
            self.z = Some(Box::default());
        }
        Ok(())
    }

    /// Swap the plaintext socket for a TLS session over it, checking the server
    /// certificate as `cfg.ssl_mode` says.
    async fn upgrade_to_tls(&mut self, cfg: &MysqlConfig) -> Result<(), OrmError> {
        let ca_pem = match &cfg.ssl_ca {
            Some(path) => Some(read_file(path, "ssl-ca").await?),
            None => None,
        };
        let identity = match (&cfg.ssl_cert, &cfg.ssl_key) {
            (Some(c), Some(k)) => Some((read_file(c, "ssl-cert").await?, read_file(k, "ssl-key").await?)),
            _ => None,
        };
        let tls_config = kernway_tls::client_config_with_identity(
            ca_pem.as_deref(),
            cfg.ssl_mode.cert_check(),
            identity.as_ref().map(|(c, k)| kernway_tls::ClientIdentity { cert_pem: c, key_pem: k }),
        )
        .map_err(|e| conn_err(format!("TLS config: {e}")))?;
        self.io
            .upgrade_tls(tls_config, &cfg.host)
            .await
            .map_err(|e| conn_err(format!("TLS handshake with {}: {e}", cfg.host)))
    }

    /// Drive the authentication exchange to `OK`: auth switches (`0xFE`) and the
    /// extra rounds of `caching_sha2_password` (`0x01`: fast-auth success, or a
    /// request for the full password — sent only over TLS).
    async fn finish_auth(
        &mut self,
        cfg: &MysqlConfig,
        mut plugin: &'static str,
        tls: bool,
        mut scramble: Vec<u8>,
    ) -> Result<(), OrmError> {
        // A well-behaved server needs at most a switch plus one extra round; the
        // bound only stops a misbehaving one from looping us forever.
        for _ in 0..8 {
            let reply = self.read_packet().await?;
            match reply.first().copied() {
                Some(0x00) => return Ok(()),
                Some(0xFF) => return Err(parse_err_packet(&reply)),
                Some(0xFE) => {
                    // AuthSwitchRequest: plugin name (NUL-terminated), then its data.
                    let (name, data) = parse_auth_switch(&reply);
                    plugin = match name.as_str() {
                        "mysql_native_password" => "mysql_native_password",
                        "caching_sha2_password" => "caching_sha2_password",
                        other => {
                            return Err(conn_err(format!(
                                "server asked to switch to unsupported auth plugin '{other}'"
                            )))
                        }
                    };
                    let auth = auth_response(plugin, cfg.password.as_bytes(), &data);
                    self.write_packet(&auth).await?;
                    scramble = data;
                }
                Some(0x01) if plugin == "caching_sha2_password" => match reply.get(1).copied() {
                    // Fast auth: the server's cache matched the scramble; OK follows.
                    Some(0x03) => {}
                    // Full auth: the server wants the password itself.
                    Some(0x04) if tls => {
                        let mut clear = cfg.password.as_bytes().to_vec();
                        clear.push(0);
                        self.write_packet(&clear).await?;
                    }
                    // Full auth without TLS: the password RSA-encrypted with
                    // the server's public key.
                    Some(0x04) => {
                        let pem = match (&cfg.server_public_key, cfg.get_server_public_key) {
                            (Some(path), _) => read_file(path, "server-public-key").await?,
                            (None, true) => {
                                self.write_packet(&[0x02]).await?; // request public key
                                let reply = self.read_packet().await?;
                                match reply.first() {
                                    Some(0x01) => reply[1..].to_vec(),
                                    Some(0xFF) => return Err(parse_err_packet(&reply)),
                                    _ => return Err(conn_err("bad public key reply")),
                                }
                            }
                            (None, false) => {
                                return Err(conn_err(
                                    "caching_sha2_password needs a full authentication (first login \
                                     since the server's cache was cleared, or a wrong password): \
                                     use TLS (ssl-mode=preferred or stricter), give the server's RSA \
                                     key with server-public-key=<pem>, or allow fetching it with \
                                     get-server-public-key=true",
                                ))
                            }
                        };
                        let key = crate::rsa::PublicKey::from_pem(&pem)
                            .map_err(|e| conn_err(format!("server public key: {e}")))?;
                        // password + NUL, XOR the 20-byte scramble (repeated).
                        let salt: Vec<u8> = scramble.iter().copied().take(20).collect();
                        if salt.is_empty() {
                            return Err(conn_err("no auth scramble for RSA password exchange"));
                        }
                        let mut clear = cfg.password.as_bytes().to_vec();
                        clear.push(0);
                        for (i, b) in clear.iter_mut().enumerate() {
                            *b ^= salt[i % salt.len()];
                        }
                        let mut seed = [0u8; 20];
                        kernway_tls::fill_random(&mut seed).map_err(|e| conn_err(format!("RNG: {e}")))?;
                        let encrypted = key.encrypt_oaep_sha1(&clear, seed).map_err(conn_err)?;
                        self.write_packet(&encrypted).await?;
                    }
                    other => {
                        return Err(conn_err(format!(
                            "unexpected caching_sha2_password state {other:?}"
                        )))
                    }
                },
                other => return Err(conn_err(format!("unexpected auth reply byte {other:?}"))),
            }
        }
        Err(conn_err("authentication did not complete"))
    }

    // --- Command phase ----------------------------------------------------

    /// Run one textual statement (`COM_QUERY`) and collect its result as text.
    pub async fn query(&mut self, sql: &str) -> Result<QueryResult, OrmError> {
        self.query_with(sql, cells_to_text).await.map(QueryResult::from)
    }

    /// Run one textual statement (`COM_QUERY`), handing each result row to `row`
    /// as it is read — decoded straight from the read buffer, so no per-cell
    /// allocation and no intermediate row representation.
    pub async fn query_with<R, F>(&mut self, sql: &str, mut row: F) -> Result<Fetched<R>, OrmError>
    where
        F: FnMut(&[Cell<'_>]) -> Result<R, OrmError>,
    {
        // Each command starts a fresh sequence.
        self.start_command();
        let mut payload = Vec::with_capacity(sql.len() + 1);
        payload.push(0x03); // COM_QUERY
        payload.extend_from_slice(sql.as_bytes());
        self.write_packet(&payload).await?;
        let first = self.read_packet().await?;
        let mut row = |_: &[ColMeta], cells: &[Cell<'_>]| row(cells);
        let results = self.read_results(first, false, &mut row, false).await?;
        Ok(first_result(results))
    }

    /// Run one textual statement and return **every** result of its response —
    /// e.g. a `CALL` whose procedure runs several `SELECT`s yields one
    /// [`QueryResult::Rows`] per `SELECT`, then the [`QueryResult::Ok`] of the call.
    pub async fn query_multi(&mut self, sql: &str) -> Result<Vec<QueryResult>, OrmError> {
        self.start_command();
        let mut payload = Vec::with_capacity(sql.len() + 1);
        payload.push(0x03); // COM_QUERY
        payload.extend_from_slice(sql.as_bytes());
        self.write_packet(&payload).await?;
        let first = self.read_packet().await?;
        let mut row = |_: &[ColMeta], cells: &[Cell<'_>]| cells_to_text(cells);
        let results = self.read_results(first, false, &mut row, true).await?;
        Ok(results.into_iter().map(QueryResult::from).collect())
    }

    /// Call stored procedure `name` with bound arguments (a prepared
    /// `CALL name(?, …)`) and return every result: one [`QueryResult::Rows`] per
    /// result set the procedure produces, then the [`QueryResult::Ok`] of the call.
    /// `name` may be schema-qualified (`db.proc`); it must be a plain identifier.
    /// `OUT` parameter values come back as a result set of their own (see
    /// the typed `SqlConn::call` for them split out).
    pub async fn call_results(&mut self, name: &str, args: &[BindValue]) -> Result<Vec<QueryResult>, OrmError> {
        let sql = call_sql(name, args.len())?;
        let mut row = |_: &[ColMeta], cells: &[Cell<'_>]| cells_to_text(cells);
        let results = self.exec_prepared_inner(&sql, args, &mut row, true).await?;
        Ok(results.into_iter().map(QueryResult::from).collect())
    }

    /// Run `sql` — over the text protocol without parameters (any
    /// statement), as a cached prepared statement with them — handing each row
    /// and its column definitions to `row`. Every result when `keep_all`.
    pub(crate) async fn run_meta<R, F>(
        &mut self,
        sql: &str,
        params: &[BindValue],
        row: &mut F,
        keep_all: bool,
    ) -> Result<Vec<Fetched<R>>, OrmError>
    where
        F: FnMut(&[ColMeta], &[Cell<'_>]) -> Result<R, OrmError>,
    {
        if params.is_empty() {
            self.start_command();
            let mut payload = Vec::with_capacity(sql.len() + 1);
            payload.push(0x03); // COM_QUERY
            payload.extend_from_slice(sql.as_bytes());
            self.write_packet(&payload).await?;
            let first = self.read_packet().await?;
            self.read_results(first, false, row, keep_all).await
        } else {
            self.exec_prepared_inner(sql, params, row, keep_all).await
        }
    }

    /// Execute a prepared statement (even without parameters) and keep every
    /// result, handing rows and column definitions to `row`.
    pub(crate) async fn exec_prepared_all<R, F>(
        &mut self,
        sql: &str,
        params: &[BindValue],
        row: &mut F,
    ) -> Result<Vec<Fetched<R>>, OrmError>
    where
        F: FnMut(&[ColMeta], &[Cell<'_>]) -> Result<R, OrmError>,
    {
        self.exec_prepared_inner(sql, params, row, true).await
    }

    /// `COM_SET_OPTION`: turn multi-statement support on (`0`) or off (`1`).
    async fn set_option(&mut self, option: u16) -> Result<(), OrmError> {
        self.start_command();
        let mut payload = vec![0x1b]; // COM_SET_OPTION
        payload.extend_from_slice(&option.to_le_bytes());
        self.write_packet(&payload).await?;
        let reply = self.read_packet().await?;
        match reply.first() {
            Some(0xFF) => Err(parse_err_packet(&reply)),
            _ => Ok(()),
        }
    }

    /// Run a script of `;`-separated statements in one round trip (schema
    /// files, migrations), reading every result; the first failing statement
    /// ends the script with its error. Multi-statement mode is enabled only
    /// for this call, so ordinary queries can never be stacked.
    pub async fn execute_script(&mut self, script: &str) -> Result<(), OrmError> {
        self.set_option(0).await?;
        let mut row = |_: &[ColMeta], _: &[Cell<'_>]| Ok(());
        let result = self.run_meta(script, &[], &mut row, true).await.map(|_| ());
        let reset = self.set_option(1).await;
        result.and(reset)
    }

    /// Run a `LOAD DATA LOCAL INFILE '<any name>' INTO TABLE …` statement,
    /// streaming `data` as the file's contents; returns the rows loaded. The
    /// server's file name is ignored: this client only ever sends the bytes
    /// given here, never a file from disk. The server must allow it
    /// (`local_infile = ON`).
    pub async fn load_local_infile(&mut self, sql: &str, data: &[u8]) -> Result<u64, OrmError> {
        self.local_data = Some(data.to_vec());
        let mut row = |_: &[ColMeta], _: &[Cell<'_>]| Ok(());
        let result = self.run_meta(sql, &[], &mut row, false).await;
        self.local_data = None;
        match first_result(result?) {
            Fetched::Ok { affected_rows, .. } => Ok(affected_rows),
            Fetched::Rows { .. } => Err(OrmError::Query("LOAD DATA returned rows".into())),
        }
    }

    /// Read every result of a response whose first packet is `first`: result
    /// sets and `OK`s, following `SERVER_MORE_RESULTS_EXISTS` to the end. Only
    /// the first result is kept unless `keep_all`, but the rest is always read,
    /// so the connection is left at a clean command boundary.
    ///
    /// An `ERR` packet ends the response and is returned at once. An error from
    /// the row function (say, a value that does not fit the entity) is held
    /// until the response is fully read, then returned — never leaving unread
    /// packets behind to be mistaken for the next command's reply.
    async fn read_results<R, F>(
        &mut self,
        mut first: Vec<u8>,
        binary: bool,
        row: &mut F,
        keep_all: bool,
    ) -> Result<Vec<Fetched<R>>, OrmError>
    where
        F: FnMut(&[ColMeta], &[Cell<'_>]) -> Result<R, OrmError>,
    {
        let mut results = Vec::new();
        let mut deferred: Option<OrmError> = None;
        loop {
            let (result, more) = match first.first().copied() {
                Some(0x00) => {
                    self.status = ok_status(&first);
                    let more = self.status & SERVER_MORE_RESULTS_EXISTS != 0;
                    (Ok(ok_fetched(&first)), more)
                }
                Some(0xFF) => return Err(parse_err_packet(&first)),
                Some(0xFB) => {
                    // LOAD DATA LOCAL INFILE: send the data the caller supplied
                    // for this statement — never a file the server names.
                    match self.local_data.take() {
                        Some(data) => {
                            for chunk in data.chunks(LOCAL_INFILE_CHUNK) {
                                self.write_packet(chunk).await?;
                            }
                        }
                        None => {
                            deferred.get_or_insert(OrmError::Query(
                                "the server asked for a local file; use load_local_infile to supply data".into(),
                            ));
                        }
                    }
                    self.write_packet(&[]).await?; // end of data
                    first = self.read_packet().await?;
                    continue;
                }
                _ => {
                    let (col_count, _) = read_lenenc_int(&first, 0)
                        .ok_or_else(|| OrmError::Query("bad column count packet".into()))?;
                    // Rows of a result set we are not keeping are read and dropped.
                    if keep_all || results.is_empty() {
                        self.read_rows(col_count as usize, binary, row).await?
                    } else {
                        let (_, more) = self
                            .read_rows(col_count as usize, binary, &mut |_: &[ColMeta], _: &[Cell<'_>]| Ok(()))
                            .await?;
                        (Ok(Fetched::Ok { affected_rows: 0, last_insert_id: 0 }), more)
                    }
                }
            };
            match result {
                Ok(fetched) if keep_all || results.is_empty() => results.push(fetched),
                Ok(_) => {}
                Err(e) => {
                    deferred.get_or_insert(e);
                }
            }
            if !more {
                break;
            }
            first = self.read_packet().await?;
        }
        match deferred {
            Some(e) => Err(e),
            None => Ok(results),
        }
    }

    /// Read a result set — column definitions, EOF, rows, EOF — decoding each
    /// row (text or binary protocol) in place and handing it to `row`.
    ///
    /// Returns the result set (or the first error the row function raised — the
    /// remaining rows are still read off the wire) and whether another result
    /// follows. An `Err` from this function itself means an `ERR` packet ended
    /// the response, or the connection failed.
    async fn read_rows<R, F>(
        &mut self,
        col_count: usize,
        binary: bool,
        row: &mut F,
    ) -> Result<(Result<Fetched<R>, OrmError>, bool), OrmError>
    where
        F: FnMut(&[ColMeta], &[Cell<'_>]) -> Result<R, OrmError>,
    {
        let mut metas = Vec::with_capacity(col_count);
        for _ in 0..col_count {
            metas.push(parse_col_meta(self.read_packet_ref().await?));
        }
        let columns: Vec<String> = metas.iter().map(|m| m.name.clone()).collect();

        let mut rows = Vec::new();
        let mut failed: Option<OrmError> = None;
        let mut scratch: Vec<Cell<'static>> = Vec::with_capacity(col_count);
        // Decode one row and hand it over — unless an earlier row already failed,
        // in which case the rest are only drained.
        let mut take = |packet: &[u8], scratch: &mut Vec<Cell<'static>>| {
            if failed.is_some() {
                return;
            }
            let mut cells = recycle(std::mem::take(scratch));
            let outcome = decode_row(packet, binary, &metas, &mut cells).and_then(|()| row(&metas, &cells));
            *scratch = recycle(cells);
            match outcome {
                Ok(r) => rows.push(r),
                Err(e) => failed = Some(e),
            }
        };
        // EOF after the column definitions (CLIENT_DEPRECATE_EOF is not set). A
        // text result set that skips it starts with its first row instead.
        let after_cols = self.read_packet_ref().await?;
        let out_params = is_eof(after_cols) && eof_status(after_cols) & SERVER_PS_OUT_PARAMS != 0;
        if !binary && !is_eof(after_cols) {
            take(after_cols, &mut scratch);
        }
        let more = loop {
            let packet = self.read_packet_ref().await?;
            if is_eof(packet) {
                let status = eof_status(packet);
                self.status = status;
                // The OUT-parameter result set's EOF does not set
                // MORE_RESULTS, yet the CALL's own OK always follows it.
                break out_params || status & SERVER_MORE_RESULTS_EXISTS != 0;
            }
            if packet.first() == Some(&0xFF) {
                return Err(parse_err_packet(packet));
            }
            take(packet, &mut scratch);
        };
        self.release_drained();
        let result = match failed {
            Some(e) => Err(e),
            None => Ok(Fetched::Rows { columns, rows, out_params }),
        };
        Ok((result, more))
    }

    // --- Prepared statements (binary protocol) ---------------------------

    /// Execute `sql` with server-side bound parameters (binary protocol).
    ///
    /// Statements are cached per connection by their SQL text, so a hot query is
    /// prepared once and then costs a single round trip (`COM_STMT_EXECUTE`)
    /// instead of prepare + execute + close. The cache holds at most
    /// 256 statements (`STMT_CACHE_CAP`), closing the oldest on the server when full.
    /// If the server no longer recognises a cached statement (it was reset, or
    /// DDL forced a re-prepare), the entry is dropped and the statement is
    /// prepared and run once more.
    pub async fn exec_prepared(
        &mut self,
        sql: &str,
        params: &[BindValue],
    ) -> Result<QueryResult, OrmError> {
        self.exec_prepared_with(sql, params, cells_to_text)
            .await
            .map(QueryResult::from)
    }

    /// [`exec_prepared`](Self::exec_prepared), handing each result row to `row`
    /// as it is read (decoded in place from the binary protocol).
    pub async fn exec_prepared_with<R, F>(
        &mut self,
        sql: &str,
        params: &[BindValue],
        mut row: F,
    ) -> Result<Fetched<R>, OrmError>
    where
        F: FnMut(&[Cell<'_>]) -> Result<R, OrmError>,
    {
        let mut row = |_: &[ColMeta], cells: &[Cell<'_>]| row(cells);
        let results = self.exec_prepared_inner(sql, params, &mut row, false).await?;
        Ok(first_result(results))
    }

    /// Execute a (cached) prepared statement and read its response; every
    /// result when `keep_all`, else just the first. Re-prepares once if the
    /// server forgot the statement.
    async fn exec_prepared_inner<R, F>(
        &mut self,
        sql: &str,
        params: &[BindValue],
        row: &mut F,
        keep_all: bool,
    ) -> Result<Vec<Fetched<R>>, OrmError>
    where
        F: FnMut(&[ColMeta], &[Cell<'_>]) -> Result<R, OrmError>,
    {
        let cached = self.stmts.get(sql);
        let stmt = match cached {
            Some(stmt) => stmt,
            None => self.prepare_cached(sql).await?,
        };
        match self.stmt_execute(stmt.id, stmt.num_params, params, row, keep_all).await? {
            Exec::Done(result) => Ok(result),
            Exec::Stale if cached.is_some() => {
                self.forget_stmt(sql);
                let stmt = self.prepare_cached(sql).await?;
                match self.stmt_execute(stmt.id, stmt.num_params, params, row, keep_all).await? {
                    Exec::Done(result) => Ok(result),
                    Exec::Stale => Err(OrmError::Query(
                        "MySQL rejected a freshly prepared statement".into(),
                    )),
                }
            }
            Exec::Stale => Err(OrmError::Query(
                "MySQL rejected a freshly prepared statement".into(),
            )),
        }
    }

    /// Prepare `sql` and remember it, evicting (and closing) the oldest cached
    /// statement first if the cache is full.
    async fn prepare_cached(&mut self, sql: &str) -> Result<Stmt, OrmError> {
        if let Some(old) = self.stmts.evict_for_insert() {
            self.stmt_close(old.id).await?;
        }
        let (id, num_params, num_columns) = self.stmt_prepare(sql).await?;
        let stmt = Stmt { id, num_params, num_columns };
        self.stmts.insert(sql, stmt);
        Ok(stmt)
    }

    /// Drop a cached statement the server no longer knows (no `COM_STMT_CLOSE`:
    /// the id is already gone server-side).
    fn forget_stmt(&mut self, sql: &str) {
        self.stmts.remove(sql);
    }

    /// Number of prepared statements currently cached on this connection.
    #[cfg(test)]
    fn cached_statements(&self) -> usize {
        self.stmts.len()
    }

    async fn stmt_prepare(&mut self, sql: &str) -> Result<(u32, u16, u16), OrmError> {
        self.start_command();
        let mut payload = Vec::with_capacity(sql.len() + 1);
        payload.push(0x16); // COM_STMT_PREPARE
        payload.extend_from_slice(sql.as_bytes());
        self.write_packet(&payload).await?;

        let ok = self.read_packet().await?;
        if ok.first() == Some(&0xFF) {
            return Err(parse_err_packet(&ok));
        }
        if ok.first() != Some(&0x00) || ok.len() < 9 {
            return Err(OrmError::Query(format!("bad COM_STMT_PREPARE response {:02x?}", &ok[..ok.len().min(40)])));
        }
        let stmt_id = u32::from_le_bytes([ok[1], ok[2], ok[3], ok[4]]);
        let num_columns = u16::from_le_bytes([ok[5], ok[6]]);
        let num_params = u16::from_le_bytes([ok[7], ok[8]]);

        // Consume parameter-definition packets + EOF, then column defs + EOF.
        for _ in 0..num_params {
            self.read_packet().await?;
        }
        if num_params > 0 {
            self.read_packet().await?; // EOF
        }
        for _ in 0..num_columns {
            self.read_packet().await?;
        }
        if num_columns > 0 {
            self.read_packet().await?; // EOF
        }
        Ok((stmt_id, num_params, num_columns))
    }

    async fn stmt_execute<R, F>(
        &mut self,
        stmt_id: u32,
        num_params: u16,
        params: &[BindValue],
        row: &mut F,
        keep_all: bool,
    ) -> Result<Exec<R>, OrmError>
    where
        F: FnMut(&[ColMeta], &[Cell<'_>]) -> Result<R, OrmError>,
    {
        if params.len() != num_params as usize {
            return Err(OrmError::Query(format!(
                "prepared statement expects {num_params} params, got {}",
                params.len()
            )));
        }
        self.start_command();
        let payload = execute_payload(stmt_id, params, CURSOR_TYPE_NO_CURSOR);
        self.write_packet(&payload).await?;

        let first = self.read_packet().await?;
        if matches!(err_code(&first), Some(ER_UNKNOWN_STMT_HANDLER | ER_NEED_REPREPARE)) {
            return Ok(Exec::Stale);
        }
        self.read_results(first, true, row, keep_all)
            .await
            .map(Exec::Done)
    }

    async fn stmt_close(&mut self, stmt_id: u32) -> Result<(), OrmError> {
        self.start_command();
        let mut payload = Vec::with_capacity(5);
        payload.push(0x19); // COM_STMT_CLOSE
        payload.extend_from_slice(&stmt_id.to_le_bytes());
        self.write_packet(&payload).await // no response packet
    }
}

/// Compression or decompression of more than this many bytes runs on the
/// blocking pool, so a large value never stalls the shard's other connections.
const OFFLOAD_BYTES: usize = 128 * 1024;

/// Inflate one zlib frame of `ulen` plain bytes, appending to `out`.
fn inflate_into(body: &[u8], ulen: usize, out: &mut Vec<u8>) -> Result<(), OrmError> {
    let start = out.len();
    out.reserve(ulen);
    let mut dec = flate2::read::ZlibDecoder::new(body);
    std::io::Read::read_to_end(&mut dec, out).map_err(|e| conn_err(format!("decompress: {e}")))?;
    if out.len() - start != ulen {
        return Err(conn_err(format!("compressed frame inflated to {} bytes, header said {ulen}", out.len() - start)));
    }
    Ok(())
}

/// Wrap `plain` packet bytes into compressed frames appended to `wire`, the
/// first with sequence id `seq`; returns the next sequence id. Each frame
/// carries at most 16 MiB - 1 plain bytes, deflated unless tiny.
fn compress_frames(plain_all: &[u8], mut seq: u8, wire: &mut Vec<u8>) -> Result<u8, OrmError> {
    let mut deflated = Vec::new();
    for plain in plain_all.chunks(0xFF_FFFF) {
        let (body, ulen): (&[u8], usize) = if plain.len() < MIN_COMPRESS_LENGTH {
            (plain, 0)
        } else {
            deflated.clear();
            let mut enc = flate2::write::ZlibEncoder::new(&mut deflated, flate2::Compression::default());
            std::io::Write::write_all(&mut enc, plain).map_err(|e| conn_err(format!("compress: {e}")))?;
            enc.finish().map_err(|e| conn_err(format!("compress: {e}")))?;
            if deflated.len() >= plain.len() {
                (plain, 0) // incompressible: send as is
            } else {
                (&deflated[..], plain.len())
            }
        };
        let clen = body.len();
        wire.extend_from_slice(&[clen as u8, (clen >> 8) as u8, (clen >> 16) as u8, seq, ulen as u8, (ulen >> 8) as u8, (ulen >> 16) as u8]);
        wire.extend_from_slice(body);
        seq = seq.wrapping_add(1);
    }
    Ok(seq)
}

/// Read a configured file (CA, certificate, key) on the blocking pool.
async fn read_file(path: &std::path::Path, what: &'static str) -> Result<Vec<u8>, OrmError> {
    let p = path.to_path_buf();
    rt_core::spawn_blocking(move || std::fs::read(&p).map_err(|e| conn_err(format!("read {what} {}: {e}", p.display()))))
        .await
        .ok_or_else(|| conn_err(format!("read {what}: worker panicked")))?
}

/// The address of `host:port`: an IP literal is parsed in place; a name is
/// resolved on the blocking pool (the system resolver blocks).
async fn resolve(host: &str, port: u16) -> Result<std::net::SocketAddr, OrmError> {
    let literal = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    if let Ok(ip) = literal.parse::<std::net::IpAddr>() {
        return Ok(std::net::SocketAddr::new(ip, port));
    }
    let name = host.to_string();
    rt_core::spawn_blocking(move || {
        (name.as_str(), port)
            .to_socket_addrs()
            .map_err(|e| conn_err(format!("resolve {name}:{port} failed: {e}")))?
            .next()
            .ok_or_else(|| conn_err(format!("no address for {name}:{port}")))
    })
    .await
    .ok_or_else(|| conn_err("resolve: worker panicked"))?
}

/// Open the server's local channel.
#[cfg(unix)]
async fn open_local(path: &std::path::Path) -> std::io::Result<BufStream> {
    rt_net::AsyncUnixStream::connect(path).await.map(BufStream::new_unix)
}

#[cfg(windows)]
async fn open_local(name: &std::path::Path) -> std::io::Result<BufStream> {
    rt_net::AsyncNamedPipe::connect(&name.to_string_lossy()).await.map(BufStream::new_pipe)
}

#[cfg(not(any(unix, windows)))]
async fn open_local(_: &std::path::Path) -> std::io::Result<BufStream> {
    Err(std::io::Error::other("no local socket / named pipe on this platform"))
}

/// `COM_STMT_EXECUTE` flags: run to completion, or open a read-only cursor.
const CURSOR_TYPE_NO_CURSOR: u8 = 0x00;
const CURSOR_TYPE_READ_ONLY: u8 = 0x01;
/// Status flags of a cursor: one is open / its last row has been sent.
const SERVER_STATUS_CURSOR_EXISTS: u16 = 0x0040;
const SERVER_STATUS_LAST_ROW_SENT: u16 = 0x0080;
/// Rows per pipelined flight of [`Conn::execute_pipelined`]: their `OK`
/// replies (a few dozen bytes each) must fit in the socket buffers while the
/// flight is still being written, or both sides would wait on each other.
const PIPELINE_ROWS: usize = 500;
/// …and at most this many request bytes per flight.
const PIPELINE_BYTES: usize = 4 << 20;

/// A `COM_STMT_EXECUTE` payload binding `params`.
fn execute_payload(stmt_id: u32, params: &[BindValue], flags: u8) -> Vec<u8> {
    let mut payload = Vec::with_capacity(16 + params.len() * 8);
    payload.push(0x17); // COM_STMT_EXECUTE
    payload.extend_from_slice(&stmt_id.to_le_bytes());
    payload.push(flags);
    payload.extend_from_slice(&1u32.to_le_bytes()); // iteration count
    if !params.is_empty() {
        // NULL bitmap.
        let mut bitmap = vec![0u8; params.len().div_ceil(8)];
        for (i, p) in params.iter().enumerate() {
            if matches!(p, BindValue::Null) {
                bitmap[i / 8] |= 1 << (i % 8);
            }
        }
        payload.extend_from_slice(&bitmap);
        payload.push(0x01); // new-params-bound flag
        for p in params {
            let (type_code, unsigned) = p.type_code();
            payload.push(type_code);
            payload.push(if unsigned { 0x80 } else { 0x00 });
        }
        for p in params {
            p.encode(&mut payload);
        }
    }
    payload
}

/// A server-side cursor (`COM_STMT_EXECUTE` with `CURSOR_TYPE_READ_ONLY`):
/// rows stay on the server until fetched in batches, and the connection is
/// free for other statements in between.
#[derive(Debug)]
pub(crate) struct CursorState {
    stmt_id: u32,
    metas: Vec<ColMeta>,
    done: bool,
}

impl CursorState {
    /// Column definitions of the cursor's result set.
    pub(crate) fn metas(&self) -> &[ColMeta] {
        &self.metas
    }

    /// Whether every row has been fetched.
    pub fn is_done(&self) -> bool {
        self.done
    }
}

/// Outcome of one pipelined flight of [`Conn::execute_pipelined`].
#[derive(Debug)]
pub(crate) enum Pipelined {
    /// Every row ran; total rows affected.
    Done(u64),
    /// Row `index` (into the slice given) failed with `error`; rows after it
    /// in the same flight were still run by the server.
    Failed {
        /// Position of the first failing row.
        index: usize,
        /// Its error.
        error: OrmError,
    },
    /// The server no longer knew the cached statement (reset, or DDL forced a
    /// re-prepare); it was dropped from the cache and nothing ran
    /// successfully that the caller has to keep. Run the rows again.
    Stale,
}

impl Conn {
    /// Open a server-side cursor over `sql` (a dedicated prepared statement,
    /// closed with the cursor). `None` — without running it — when the
    /// statement has no result set.
    pub(crate) async fn stmt_cursor_open(&mut self, sql: &str, params: &[BindValue]) -> Result<Option<CursorState>, OrmError> {
        let (stmt_id, num_params, num_columns) = self.stmt_prepare(sql).await?;
        if num_columns == 0 {
            // Refused before it runs: a cursor needs a result set.
            self.stmt_close(stmt_id).await?;
            return Ok(None);
        }
        if params.len() != num_params as usize {
            self.stmt_close(stmt_id).await?;
            return Err(OrmError::Query(format!(
                "prepared statement expects {num_params} params, got {}",
                params.len()
            )));
        }
        self.start_command();
        self.write_packet(&execute_payload(stmt_id, params, CURSOR_TYPE_READ_ONLY)).await?;
        let first = self.read_packet().await?;
        match first.first().copied() {
            Some(0xFF) => {
                let err = parse_err_packet(&first);
                self.stmt_close(stmt_id).await?;
                Err(err)
            }
            Some(0x00) => {
                self.status = ok_status(&first);
                self.stmt_close(stmt_id).await?;
                Ok(None)
            }
            _ => {
                let (col_count, _) = read_lenenc_int(&first, 0)
                    .ok_or_else(|| OrmError::Query("bad column count packet".into()))?;
                let mut metas = Vec::with_capacity(col_count as usize);
                for _ in 0..col_count {
                    metas.push(parse_col_meta(self.read_packet_ref().await?));
                }
                let eof = self.read_packet().await?;
                if !is_eof(&eof) {
                    return Err(conn_err("expected EOF after the cursor's column definitions"));
                }
                self.status = eof_status(&eof);
                // A server that could not open a cursor sends the rows at once.
                if self.status & SERVER_STATUS_CURSOR_EXISTS == 0 {
                    return Err(conn_err("the server did not open a cursor"));
                }
                Ok(Some(CursorState { stmt_id, metas, done: false }))
            }
        }
    }

    /// Fetch up to `max_rows` rows of an open cursor (`COM_STMT_FETCH`),
    /// handing each to `row`. After the last row the cursor is marked done
    /// and its statement closed. An error from `row` is returned once the
    /// batch is read off the wire.
    pub(crate) async fn stmt_cursor_fetch<F>(&mut self, cur: &mut CursorState, max_rows: u32, mut row: F) -> Result<(), OrmError>
    where
        F: FnMut(&[ColMeta], &[Cell<'_>]) -> Result<(), OrmError>,
    {
        if cur.done {
            return Ok(());
        }
        self.start_command();
        let mut payload = Vec::with_capacity(9);
        payload.push(0x1c); // COM_STMT_FETCH
        payload.extend_from_slice(&cur.stmt_id.to_le_bytes());
        payload.extend_from_slice(&max_rows.max(1).to_le_bytes());
        self.write_packet(&payload).await?;
        let mut failed: Option<OrmError> = None;
        let mut scratch: Vec<Cell<'static>> = Vec::with_capacity(cur.metas.len());
        loop {
            let packet = self.read_packet_ref().await?;
            if packet.first() == Some(&0xFF) {
                let err = parse_err_packet(packet);
                self.release_drained();
                return Err(err);
            }
            if is_eof(packet) {
                let status = eof_status(packet);
                self.status = status;
                cur.done = status & SERVER_STATUS_LAST_ROW_SENT != 0;
                break;
            }
            if failed.is_none() {
                let mut cells = recycle(std::mem::take(&mut scratch));
                let outcome = decode_row(packet, true, &cur.metas, &mut cells).and_then(|()| row(&cur.metas, &cells));
                scratch = recycle(cells);
                if let Err(e) = outcome {
                    failed = Some(e);
                }
            }
        }
        self.release_drained();
        if cur.done {
            self.stmt_close(cur.stmt_id).await?;
        }
        failed.map_or(Ok(()), Err)
    }

    /// Close a cursor before its end (closes its statement on the server).
    pub(crate) async fn stmt_cursor_close(&mut self, cur: CursorState) -> Result<(), OrmError> {
        if cur.done {
            return Ok(());
        }
        self.stmt_close(cur.stmt_id).await
    }

    /// Prepare (or reuse) `sql` and report whether it is DML-shaped (no
    /// result set) — the statements [`execute_pipelined`](Self::execute_pipelined) accepts.
    pub(crate) async fn prepare_for_batch(&mut self, sql: &str) -> Result<bool, OrmError> {
        let stmt = match self.stmts.get(sql) {
            Some(s) => s,
            None => self.prepare_cached(sql).await?,
        };
        Ok(stmt.num_columns == 0)
    }

    /// Run the prepared `sql` once per row of `rows`, **pipelined**: a flight
    /// of executes is written in one go, then their replies are read, so a
    /// batch costs one round trip per [`PIPELINE_ROWS`] rows instead of one
    /// per row. The statement must have no result set (see
    /// [`prepare_for_batch`](Self::prepare_for_batch)). The server runs every
    /// row it was sent even after one fails; the caller decides what to keep
    /// (see `SqlConn::execute_many`, which wraps this in a savepoint).
    pub(crate) async fn execute_pipelined(&mut self, sql: &str, rows: &[Vec<BindValue>]) -> Result<Pipelined, OrmError> {
        let stmt = match self.stmts.get(sql) {
            Some(s) => s,
            None => self.prepare_cached(sql).await?,
        };
        if stmt.num_columns != 0 {
            return Err(OrmError::Query("execute_pipelined: the statement returns rows".into()));
        }
        if let Some(bad) = rows.iter().find(|r| r.len() != stmt.num_params as usize) {
            return Err(OrmError::Query(format!(
                "prepared statement expects {} params, got {}",
                stmt.num_params,
                bad.len()
            )));
        }
        let mut total = 0u64;
        let mut first_error: Option<(usize, OrmError)> = None;
        let mut stale = false;
        let mut start = 0;
        // Flights stop after the one where a row failed.
        while start < rows.len() && first_error.is_none() && !stale {
            // Write one flight.
            let mut end = start;
            let mut bytes = 0;
            self.io.write_buf();
            while end < rows.len() && end - start < PIPELINE_ROWS && bytes < PIPELINE_BYTES {
                let payload = execute_payload(stmt.id, &rows[end], CURSOR_TYPE_NO_CURSOR);
                bytes += payload.len();
                self.queue_packet(&payload).await?;
                end += 1;
            }
            self.io.flush().await.map_err(|e| conn_err(format!("write packet: {e}")))?;
            // Read its replies, in order.
            for i in start..end {
                let reply = self.read_packet().await?;
                match reply.first().copied() {
                    Some(0x00) => {
                        self.status = ok_status(&reply);
                        if first_error.is_none() {
                            if let Fetched::Ok { affected_rows, .. } = ok_fetched::<()>(&reply) {
                                total += affected_rows;
                            }
                        }
                    }
                    Some(0xFF) if matches!(err_code(&reply), Some(ER_UNKNOWN_STMT_HANDLER | ER_NEED_REPREPARE)) => {
                        stale = true;
                    }
                    Some(0xFF) => {
                        if first_error.is_none() {
                            first_error = Some((i, parse_err_packet(&reply)));
                        }
                    }
                    _ => return Err(conn_err("unexpected reply to a pipelined execute")),
                }
            }
            start = end;
        }
        if stale {
            self.forget_stmt(sql);
            return Ok(Pipelined::Stale);
        }
        Ok(match first_error {
            Some((index, error)) => Pipelined::Failed { index, error },
            None => Pipelined::Done(total),
        })
    }

    /// Compress the plain bytes in `z.out` into frames appended to the write
    /// buffer — on the blocking pool when they are large.
    async fn compress_queued(&mut self) -> Result<(), OrmError> {
        let Some(z) = self.z.as_deref_mut() else { return Ok(()) };
        if z.out.len() <= OFFLOAD_BYTES {
            z.seq = compress_frames(&z.out, z.seq, self.io.write_buf_append())?;
            return Ok(());
        }
        let plain = std::mem::take(&mut z.out);
        let seq = z.seq;
        let (wire, seq, plain) = rt_core::spawn_blocking(move || {
            let mut wire = Vec::with_capacity(plain.len() / 2);
            compress_frames(&plain, seq, &mut wire).map(|seq| (wire, seq, plain))
        })
        .await
        .ok_or_else(|| conn_err("compress: worker panicked"))??;
        let z = self.z.as_deref_mut().expect("compression is on");
        z.seq = seq;
        z.out = plain;
        self.io.write_buf_append().extend_from_slice(&wire);
        Ok(())
    }

    /// Append one command packet (sequence id 0) to the write buffer without
    /// sending it — for pipelined flights.
    async fn queue_packet(&mut self, payload: &[u8]) -> Result<(), OrmError> {
        self.start_command();
        let mut rest = payload;
        loop {
            let len = rest.len().min(0xFF_FFFF);
            let header = [len as u8, (len >> 8) as u8, (len >> 16) as u8, self.seq];
            match self.z.as_deref_mut() {
                None => {
                    let wire = self.io.write_buf_append();
                    wire.extend_from_slice(&header);
                    wire.extend_from_slice(&rest[..len]);
                }
                Some(z) => {
                    z.out.clear();
                    z.out.extend_from_slice(&header);
                    z.out.extend_from_slice(&rest[..len]);
                    self.compress_queued().await?;
                }
            }
            self.seq = self.seq.wrapping_add(1);
            rest = &rest[len..];
            if len < 0xFF_FFFF {
                return Ok(());
            }
        }
    }
}

// --- Handshake parsing & response -----------------------------------------

/// What the server's HandshakeV10 tells us.
struct Hello {
    /// The 20-byte auth nonce.
    salt: Vec<u8>,
    /// The server's default auth plugin (empty if it named none).
    plugin: String,
    /// Server capability flags (lower and upper halves joined).
    capabilities: u32,
}

/// Parse HandshakeV10.
fn parse_handshake(p: &[u8]) -> Result<Hello, OrmError> {
    if p.first() != Some(&0x0a) {
        if p.first() == Some(&0xFF) {
            return Err(parse_err_packet(p));
        }
        return Err(conn_err("unsupported handshake protocol version (need v10)"));
    }
    let mut i = 1usize;
    // server version (NUL-terminated string)
    let vend = p[i..]
        .iter()
        .position(|&b| b == 0)
        .map(|k| i + k)
        .ok_or_else(|| conn_err("bad server version"))?;
    i = vend + 1;
    i += 4; // connection id
    let mut salt = Vec::with_capacity(20);
    salt.extend_from_slice(p.get(i..i + 8).ok_or_else(|| conn_err("short handshake (salt1)"))?);
    i += 8;
    i += 1; // filler
    let caps_lower = p
        .get(i..i + 2)
        .map(|b| u32::from(u16::from_le_bytes([b[0], b[1]])))
        .ok_or_else(|| conn_err("short handshake (capabilities)"))?;
    i += 2;
    if p.len() <= i {
        return Ok(Hello { salt, plugin: String::new(), capabilities: caps_lower });
    }
    i += 1; // charset
    i += 2; // status flags
    let caps_upper = p
        .get(i..i + 2)
        .map(|b| u32::from(u16::from_le_bytes([b[0], b[1]])))
        .unwrap_or(0);
    i += 2;
    i += 1; // length of auth-plugin-data
    i += 10; // reserved
    // salt part 2 — up to 12 bytes then a NUL.
    let salt2: Vec<u8> = p
        .get(i..)
        .unwrap_or(&[])
        .iter()
        .copied()
        .take_while(|&b| b != 0)
        .take(12)
        .collect();
    i += salt2.len() + 1;
    salt.extend_from_slice(&salt2);
    // auth plugin name (NUL-terminated), if present.
    let plugin = if i < p.len() {
        let end = p[i..].iter().position(|&b| b == 0).map(|k| i + k).unwrap_or(p.len());
        String::from_utf8_lossy(&p[i..end]).to_string()
    } else {
        String::new()
    };
    Ok(Hello { salt, plugin, capabilities: caps_lower | caps_upper << 16 })
}

/// The handshake response's fixed 32-byte header: capabilities, max packet,
/// charset, reserved. On its own (with `CLIENT_SSL`) it is the `SSLRequest`.
fn handshake_header(cfg: &MysqlConfig, extra_caps: u32) -> Vec<u8> {
    let mut caps = CLIENT_LONG_PASSWORD
        | CLIENT_FOUND_ROWS
        | CLIENT_LONG_FLAG
        | CLIENT_PROTOCOL_41
        | CLIENT_TRANSACTIONS
        | CLIENT_SECURE_CONNECTION
        | CLIENT_PLUGIN_AUTH
        | CLIENT_MULTI_RESULTS
        | CLIENT_PS_MULTI_RESULTS
        | CLIENT_LOCAL_FILES
        | extra_caps;
    if cfg.database.is_some() {
        caps |= CLIENT_CONNECT_WITH_DB;
    }
    let mut out = Vec::with_capacity(96);
    out.extend_from_slice(&caps.to_le_bytes());
    out.extend_from_slice(&MAX_PACKET.to_le_bytes());
    out.push(UTF8MB4_GENERAL_CI);
    out.extend_from_slice(&[0u8; 23]); // reserved
    out
}

fn build_handshake_response(
    cfg: &MysqlConfig,
    auth: &[u8],
    plugin: &str,
    extra_caps: u32,
) -> Vec<u8> {
    let mut out = handshake_header(cfg, extra_caps);
    out.extend_from_slice(cfg.user.as_bytes());
    out.push(0);
    // CLIENT_SECURE_CONNECTION: 1-byte length-prefixed auth response.
    out.push(auth.len() as u8);
    out.extend_from_slice(auth);
    if let Some(db) = &cfg.database {
        out.extend_from_slice(db.as_bytes());
        out.push(0);
    }
    out.extend_from_slice(plugin.as_bytes());
    out.push(0);
    out
}

/// AuthSwitchRequest (`0xFE`, plugin name NUL-terminated, auth data) → the
/// plugin name and its auth data (the new nonce, trailing NUL dropped).
fn parse_auth_switch(packet: &[u8]) -> (String, Vec<u8>) {
    let body = packet.get(1..).unwrap_or(&[]);
    let name_end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
    let name = String::from_utf8_lossy(&body[..name_end]).to_string();
    let mut data = body.get(name_end + 1..).unwrap_or(&[]).to_vec();
    if data.last() == Some(&0) {
        data.pop();
    }
    (name, data)
}

/// The scrambled password for `plugin` against the server nonce `salt`.
fn auth_response(plugin: &str, password: &[u8], salt: &[u8]) -> Vec<u8> {
    match plugin {
        "caching_sha2_password" => scramble_sha256(password, salt),
        _ => scramble_native(password, salt),
    }
}

/// `caching_sha2_password` fast-auth scramble:
/// `SHA256(pw) XOR SHA256(SHA256(SHA256(pw)) + nonce)`.
fn scramble_sha256(password: &[u8], nonce: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    if password.is_empty() {
        return Vec::new();
    }
    let hash1 = Sha256::digest(password);
    let hash2 = Sha256::digest(hash1);
    let mut h = Sha256::new();
    h.update(hash2);
    h.update(nonce);
    let seed = h.finalize();
    hash1.iter().zip(seed.iter()).map(|(a, b)| a ^ b).collect()
}

/// `SHA1(pw) XOR SHA1(salt + SHA1(SHA1(pw)))`.
fn scramble_native(password: &[u8], salt: &[u8]) -> Vec<u8> {
    use sha1::{Digest, Sha1};
    if password.is_empty() {
        return Vec::new();
    }
    let hash1 = Sha1::digest(password);
    let hash2 = Sha1::digest(hash1);
    let mut h = Sha1::new();
    h.update(salt);
    h.update(hash2);
    let seed = h.finalize();
    hash1.iter().zip(seed.iter()).map(|(a, b)| a ^ b).collect()
}

// --- Packet helpers --------------------------------------------------------

fn is_eof(p: &[u8]) -> bool {
    // EOF: 0xFE and short (< 9 bytes). A 0xFE that is long is a lenenc value.
    p.first() == Some(&0xFE) && p.len() < 9
}

/// The first result of a response (an empty `OK` if, oddly, there was none).
fn first_result<R>(results: Vec<Fetched<R>>) -> Fetched<R> {
    results
        .into_iter()
        .next()
        .unwrap_or(Fetched::Ok { affected_rows: 0, last_insert_id: 0 })
}

/// Server status flags of an `OK` packet (after its two length-encoded ints).
fn ok_status(p: &[u8]) -> u16 {
    let mut i = 1usize;
    for _ in 0..2 {
        match read_lenenc_int(p, i) {
            Some((_, n)) => i += n,
            None => return 0,
        }
    }
    p.get(i..i + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
}

/// Server status flags of an `EOF` packet (`0xFE`, warnings, status).
fn eof_status(p: &[u8]) -> u16 {
    p.get(3..5).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]))
}

/// `CALL name(?, …)` for `args` arguments. `name` must be an identifier,
/// optionally schema-qualified; each part is backquoted.
pub(crate) fn call_sql(name: &str, args: usize) -> Result<String, OrmError> {
    let parts: Vec<&str> = name.split('.').collect();
    let valid = parts.len() <= 2
        && parts.iter().all(|p| {
            !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
        });
    if !valid {
        return Err(OrmError::Query(format!("invalid procedure name '{name}'")));
    }
    let quoted: Vec<String> = parts.iter().map(|p| format!("`{p}`")).collect();
    Ok(format!("CALL {}({})", quoted.join("."), vec!["?"; args].join(", ")))
}

/// An `OK` packet as a row-less [`Fetched`].
fn ok_fetched<R>(p: &[u8]) -> Fetched<R> {
    match parse_ok_packet(p) {
        QueryResult::Ok { affected_rows, last_insert_id } => {
            Fetched::Ok { affected_rows, last_insert_id }
        }
        QueryResult::Rows { .. } => unreachable!("an OK packet has no rows"),
    }
}

fn parse_ok_packet(p: &[u8]) -> QueryResult {
    // 0x00, lenenc affected_rows, lenenc last_insert_id, …
    let mut i = 1usize;
    let (affected_rows, n1) = read_lenenc_int(p, i).unwrap_or((0, 1));
    i += n1;
    let (last_insert_id, _n2) = read_lenenc_int(p, i).unwrap_or((0, 1));
    QueryResult::Ok { affected_rows, last_insert_id }
}

/// Outcome of executing a (possibly cached) prepared statement.
enum Exec<R> {
    /// The statement ran; here are its results.
    Done(Vec<Fetched<R>>),
    /// The server no longer knows this statement id — re-prepare and retry.
    Stale,
}

/// The MySQL error code of an `ERR` packet, if it is long enough to carry one.
fn err_code(p: &[u8]) -> Option<u16> {
    (p.first() == Some(&0xFF) && p.len() >= 3).then(|| (p[1] as u16) | (p[2] as u16) << 8)
}

fn parse_err_packet(p: &[u8]) -> OrmError {
    // 0xFF, 2-byte code, '#', 5-byte sqlstate, message.
    if p.len() < 3 {
        return OrmError::Query("MySQL error (truncated packet)".into());
    }
    let code = (p[1] as u16) | (p[2] as u16) << 8;
    let mut i = 3usize;
    if p.get(i) == Some(&b'#') {
        i += 6; // '#' + 5-byte sqlstate
    }
    let msg = String::from_utf8_lossy(p.get(i..).unwrap_or(&[])).to_string();
    // Map the common constraint codes onto the shared vocabulary.
    match code {
        1062 => {
            // ER_DUP_ENTRY — "...for key 'table.column'".
            let field = msg
                .rsplit('\'')
                .nth(1)
                .and_then(|k| k.rsplit('.').next())
                .unwrap_or("unknown")
                .to_string();
            OrmError::UniqueViolation { field }
        }
        1451 | 1452 => OrmError::ForeignKeyViolation,
        _ => OrmError::Query(format!("MySQL error {code}: {msg}")),
    }
}

/// Decode one result row (text or binary protocol) into `cells`.
fn decode_row<'a>(
    p: &'a [u8],
    binary: bool,
    metas: &[ColMeta],
    cells: &mut Vec<Cell<'a>>,
) -> Result<(), OrmError> {
    if binary {
        decode_binary_cells(p, metas, cells)
    } else {
        decode_text_cells(p, metas.len(), cells)
    }
}

/// Decode a text-protocol row into `cells`, borrowing each value from `p`.
fn decode_text_cells<'a>(
    p: &'a [u8],
    col_count: usize,
    cells: &mut Vec<Cell<'a>>,
) -> Result<(), OrmError> {
    let mut i = 0usize;
    for _ in 0..col_count {
        if p.get(i) == Some(&0xFB) {
            cells.push(Cell::Null);
            i += 1;
        } else {
            let (bytes, next) =
                read_lenenc_str(p, i).ok_or_else(|| OrmError::Query("truncated text row".into()))?;
            cells.push(Cell::Bytes(bytes));
            i = next;
        }
    }
    Ok(())
}

/// A text-protocol row as owned strings (tests and the [`QueryResult`] shape).
#[cfg(test)]
fn parse_text_row(p: &[u8], col_count: usize) -> Result<Vec<Option<String>>, OrmError> {
    let mut cells = Vec::with_capacity(col_count);
    decode_text_cells(p, col_count, &mut cells)?;
    cells_to_text(&cells)
}

/// Read a length-encoded integer at `i`; returns (value, bytes_consumed).
fn read_lenenc_int(p: &[u8], i: usize) -> Option<(u64, usize)> {
    let first = *p.get(i)?;
    match first {
        0xFB => Some((0, 1)), // NULL in a value context; 0 length here
        0xFC => {
            let b = p.get(i + 1..i + 3)?;
            Some(((b[0] as u64) | (b[1] as u64) << 8, 3))
        }
        0xFD => {
            let b = p.get(i + 1..i + 4)?;
            Some(((b[0] as u64) | (b[1] as u64) << 8 | (b[2] as u64) << 16, 4))
        }
        0xFE => {
            let b = p.get(i + 1..i + 9)?;
            let mut v = 0u64;
            for (k, byte) in b.iter().enumerate() {
                v |= (*byte as u64) << (8 * k);
            }
            Some((v, 9))
        }
        n => Some((n as u64, 1)),
    }
}

/// Read a length-encoded string at `i`; returns (bytes, next_index).
fn read_lenenc_str(p: &[u8], i: usize) -> Option<(&[u8], usize)> {
    let (len, consumed) = read_lenenc_int(p, i)?;
    let start = i + consumed;
    let end = start + len as usize;
    let bytes = p.get(start..end)?;
    Some((bytes, end))
}

/// Escape a string for a single-quoted MySQL literal.
///
/// Handles the characters MySQL treats specially in string literals. Used by the
/// text-protocol path until prepared statements land.
pub fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\0' => out.push_str("\\0"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            '\u{1a}' => out.push_str("\\Z"),
            other => out.push(other),
        }
    }
    out
}

// --- Prepared-statement binary codec --------------------------------------

/// A value bound to a prepared statement, sent in the binary protocol (no text
/// escaping). The server coerces to the actual column type.
#[derive(Debug, Clone)]
pub enum BindValue {
    /// SQL NULL.
    Null,
    /// Signed integer (sent as `LONGLONG`).
    Int(i64),
    /// Unsigned integer (sent as unsigned `LONGLONG`).
    UInt(u64),
    /// Double-precision float (sent as `DOUBLE`).
    Float(f64),
    /// UTF-8 text (sent as `VAR_STRING`).
    Str(String),
    /// Binary data (sent as `BLOB`).
    Bytes(Vec<u8>),
}

const MYSQL_TYPE_TINY: u8 = 0x01;
const MYSQL_TYPE_SHORT: u8 = 0x02;
const MYSQL_TYPE_LONG: u8 = 0x03;
const MYSQL_TYPE_FLOAT: u8 = 0x04;
const MYSQL_TYPE_DOUBLE: u8 = 0x05;
const MYSQL_TYPE_NULL: u8 = 0x06;
const MYSQL_TYPE_TIMESTAMP: u8 = 0x07;
const MYSQL_TYPE_LONGLONG: u8 = 0x08;
const MYSQL_TYPE_INT24: u8 = 0x09;
const MYSQL_TYPE_DATE: u8 = 0x0a;
const MYSQL_TYPE_TIME: u8 = 0x0b;
const MYSQL_TYPE_DATETIME: u8 = 0x0c;
const MYSQL_TYPE_YEAR: u8 = 0x0d;
const MYSQL_TYPE_BLOB: u8 = 0xfc;
/// `NEWDATE` (internal, may appear in metadata for DATE).
const MYSQL_TYPE_NEWDATE: u8 = 0x0e;
/// The `binary` character set: a byte string, not text.
const BINARY_CHARSET: u16 = 63;
const MYSQL_TYPE_VAR_STRING: u8 = 0xfd;

impl BindValue {
    /// `(type_code, unsigned_flag)` for the param-type block.
    fn type_code(&self) -> (u8, bool) {
        match self {
            BindValue::Null => (MYSQL_TYPE_NULL, false),
            BindValue::Int(_) => (MYSQL_TYPE_LONGLONG, false),
            BindValue::UInt(_) => (MYSQL_TYPE_LONGLONG, true),
            BindValue::Float(_) => (MYSQL_TYPE_DOUBLE, false),
            BindValue::Str(_) => (MYSQL_TYPE_VAR_STRING, false),
            BindValue::Bytes(_) => (MYSQL_TYPE_BLOB, false),
        }
    }

    /// Append this value's binary encoding (nothing for NULL — it lives in the
    /// null bitmap).
    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            BindValue::Null => {}
            BindValue::Int(v) => out.extend_from_slice(&v.to_le_bytes()),
            BindValue::UInt(v) => out.extend_from_slice(&v.to_le_bytes()),
            BindValue::Float(v) => out.extend_from_slice(&v.to_le_bytes()),
            BindValue::Str(s) => {
                write_lenenc_int(out, s.len() as u64);
                out.extend_from_slice(s.as_bytes());
            }
            BindValue::Bytes(b) => {
                write_lenenc_int(out, b.len() as u64);
                out.extend_from_slice(b);
            }
        }
    }
}

fn write_lenenc_int(out: &mut Vec<u8>, v: u64) {
    if v < 251 {
        out.push(v as u8);
    } else if v < 0x1_0000 {
        out.push(0xFC);
        out.extend_from_slice(&(v as u16).to_le_bytes());
    } else if v < 0x100_0000 {
        out.push(0xFD);
        out.extend_from_slice(&(v as u32).to_le_bytes()[..3]);
    } else {
        out.push(0xFE);
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// Column metadata needed to decode a binary-protocol row.
/// A result column's definition, as far as decoding needs it.
#[derive(Debug, Clone, Default)]
pub(crate) struct ColMeta {
    /// The column label.
    pub(crate) name: String,
    /// The protocol type code (`MYSQL_TYPE_*`).
    pub(crate) col_type: u8,
    /// Integer column declared `UNSIGNED`.
    pub(crate) unsigned: bool,
    /// Binary character set (63) with the BINARY flag: bytes, not text.
    pub(crate) binary: bool,
    /// Binary character set without the BINARY flag — how MySQL describes a
    /// procedure's character `OUT` parameters: text if it is valid UTF-8.
    pub(crate) maybe_text: bool,
    /// Fractional-second digits of a temporal column (0–6; 31 = not fixed).
    pub(crate) decimals: u8,
}

fn parse_col_meta(def: &[u8]) -> ColMeta {
    // 6 lenenc strings: catalog, schema, table, org_table, name, org_name.
    let mut i = 0usize;
    let mut name = String::new();
    for field in 0..6 {
        match read_lenenc_str(def, i) {
            Some((bytes, next)) => {
                if field == 4 {
                    name = String::from_utf8_lossy(bytes).to_string();
                }
                i = next;
            }
            None => return ColMeta { name, col_type: MYSQL_TYPE_VAR_STRING, ..ColMeta::default() },
        }
    }
    // lenenc fixed-length (0x0c), charset(2), column_length(4), type(1),
    // flags(2), decimals(1).
    let (_fixed, consumed) = read_lenenc_int(def, i).unwrap_or((0, 1));
    i += consumed;
    let charset = def.get(i..i + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
    i += 2 + 4;
    let col_type = *def.get(i).unwrap_or(&MYSQL_TYPE_VAR_STRING);
    let flags = def
        .get(i + 1..i + 3)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0);
    let decimals = *def.get(i + 3).unwrap_or(&0);
    let binary_charset = charset == BINARY_CHARSET;
    let binary_flag = flags & 0x0080 != 0;
    ColMeta {
        name,
        col_type,
        unsigned: flags & 0x0020 != 0,
        binary: binary_charset && binary_flag,
        maybe_text: binary_charset && !binary_flag,
        decimals,
    }
}

/// Decode a binary-protocol row into `cells`; strings borrow from `packet`.
fn decode_binary_cells<'a>(
    packet: &'a [u8],
    metas: &[ColMeta],
    cells: &mut Vec<Cell<'a>>,
) -> Result<(), OrmError> {
    // 0x00 header, then a null bitmap of (num_cols + 7 + 2) / 8 bytes (offset 2).
    let n = metas.len();
    let bitmap_len = (n + 7 + 2) / 8;
    let bitmap = packet
        .get(1..1 + bitmap_len)
        .ok_or_else(|| OrmError::Query("short binary row".into()))?;
    let mut i = 1 + bitmap_len;
    for (col, meta) in metas.iter().enumerate() {
        let is_null = bitmap[(col + 2) / 8] & (1 << ((col + 2) % 8)) != 0;
        if is_null {
            cells.push(Cell::Null);
            continue;
        }
        let (value, next) = decode_binary_value(packet, i, meta)?;
        cells.push(value);
        i = next;
    }
    Ok(())
}

/// Decode one binary value. Numbers stay numbers (no text round trip); strings
/// and blobs borrow from the packet; temporal values are formatted as text.
fn decode_binary_value<'a>(p: &'a [u8], i: usize, meta: &ColMeta) -> Result<(Cell<'a>, usize), OrmError> {
    let short = || OrmError::Query("truncated binary value".into());
    let int = |raw: u64, signed: i64| if meta.unsigned { Cell::UInt(raw) } else { Cell::Int(signed) };
    match meta.col_type {
        MYSQL_TYPE_TINY => {
            let b = *p.get(i).ok_or_else(short)?;
            Ok((int(u64::from(b), i64::from(b as i8)), i + 1))
        }
        MYSQL_TYPE_SHORT | MYSQL_TYPE_YEAR => {
            let b = p.get(i..i + 2).ok_or_else(short)?;
            let raw = u16::from_le_bytes([b[0], b[1]]);
            Ok((int(u64::from(raw), i64::from(raw as i16)), i + 2))
        }
        MYSQL_TYPE_LONG | MYSQL_TYPE_INT24 => {
            let b = p.get(i..i + 4).ok_or_else(short)?;
            let raw = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            Ok((int(u64::from(raw), i64::from(raw as i32)), i + 4))
        }
        MYSQL_TYPE_LONGLONG => {
            let b = p.get(i..i + 8).ok_or_else(short)?;
            let raw = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            Ok((int(raw, raw as i64), i + 8))
        }
        MYSQL_TYPE_FLOAT => {
            let b = p.get(i..i + 4).ok_or_else(short)?;
            Ok((Cell::F32(f32::from_le_bytes([b[0], b[1], b[2], b[3]])), i + 4))
        }
        MYSQL_TYPE_DOUBLE => {
            let b = p.get(i..i + 8).ok_or_else(short)?;
            let f = f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            Ok((Cell::F64(f), i + 8))
        }
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE | MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP => {
            decode_datetime(p, i, meta).map(|(s, next)| (Cell::Owned(s), next))
        }
        MYSQL_TYPE_TIME => decode_time(p, i, meta).map(|(s, next)| (Cell::Owned(s), next)),
        // DECIMAL / NEWDECIMAL / VARCHAR / VAR_STRING / STRING / BLOB / JSON / BIT / …
        _ => {
            let (bytes, next) = read_lenenc_str(p, i).ok_or_else(short)?;
            Ok((Cell::Bytes(bytes), next))
        }
    }
}

/// `.ffffff` cut to the column's fractional digits, as the text protocol
/// prints it (`decimals` 31 = not fixed: all six digits when non-zero).
fn fraction(micros: u32, decimals: u8) -> String {
    let digits = match decimals {
        0 => return String::new(),
        d @ 1..=6 => usize::from(d),
        _ if micros == 0 => return String::new(),
        _ => 6,
    };
    format!(".{micros:06}")[..=digits].to_string()
}

/// A binary DATE / DATETIME / TIMESTAMP in the text protocol's format.
fn decode_datetime(p: &[u8], i: usize, meta: &ColMeta) -> Result<(String, usize), OrmError> {
    let short = || OrmError::Query("short datetime".into());
    let len = *p.get(i).ok_or_else(short)? as usize;
    let d = p.get(i + 1..i + 1 + len).ok_or_else(short)?;
    let at = |k: usize| d.get(k).copied().unwrap_or(0);
    let year = if len >= 2 { u16::from_le_bytes([d[0], d[1]]) } else { 0 };
    let date = format!("{year:04}-{:02}-{:02}", at(2), at(3));
    let s = if matches!(meta.col_type, MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE) {
        date
    } else {
        let micros = if len >= 11 { u32::from_le_bytes([d[7], d[8], d[9], d[10]]) } else { 0 };
        format!("{date} {:02}:{:02}:{:02}{}", at(4), at(5), at(6), fraction(micros, meta.decimals))
    };
    Ok((s, i + 1 + len))
}

/// A binary TIME (a duration: may be negative or exceed 24 hours).
fn decode_time(p: &[u8], i: usize, meta: &ColMeta) -> Result<(String, usize), OrmError> {
    let short = || OrmError::Query("short time".into());
    let len = *p.get(i).ok_or_else(short)? as usize;
    let d = p.get(i + 1..i + 1 + len).ok_or_else(short)?;
    let s = if len < 8 {
        format!("00:00:00{}", fraction(0, meta.decimals))
    } else {
        let neg = d[0] == 1;
        let hours = u32::from_le_bytes([d[1], d[2], d[3], d[4]]) * 24 + u32::from(d[5]);
        let micros = if len >= 12 { u32::from_le_bytes([d[8], d[9], d[10], d[11]]) } else { 0 };
        format!(
            "{}{hours:02}:{:02}:{:02}{}",
            if neg { "-" } else { "" },
            d[6],
            d[7],
            fraction(micros, meta.decimals)
        )
    };
    Ok((s, i + 1 + len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_blocks_injection() {
        assert_eq!(escape_string("a'b"), "a\\'b");
        assert_eq!(escape_string("x\\y"), "x\\\\y");
        assert_eq!(escape_string("line\nbreak"), "line\\nbreak");
        // A classic injection attempt stays inert inside the quotes.
        assert_eq!(escape_string("'; DROP TABLE t; --"), "\\'; DROP TABLE t; --");
    }

    #[test]
    fn lenenc_int_widths() {
        assert_eq!(read_lenenc_int(&[0x05], 0), Some((5, 1)));
        assert_eq!(read_lenenc_int(&[0xFC, 0x10, 0x27], 0), Some((10000, 3)));
        assert_eq!(read_lenenc_int(&[0xFD, 0x01, 0x00, 0x00], 0), Some((1, 4)));
        let eight = [0xFE, 1, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(read_lenenc_int(&eight, 0), Some((1, 9)));
    }

    #[test]
    fn lenenc_str_reads_payload() {
        // length 3 ("abc")
        let buf = [0x03, b'a', b'b', b'c', b'X'];
        let (bytes, next) = read_lenenc_str(&buf, 0).unwrap();
        assert_eq!(bytes, b"abc");
        assert_eq!(next, 4);
    }

    #[test]
    fn text_row_handles_null_and_values() {
        // col0 = "hi" (lenenc), col1 = NULL (0xFB)
        let packet = [0x02, b'h', b'i', 0xFB];
        let row = parse_text_row(&packet, 2).unwrap();
        assert_eq!(row, vec![Some("hi".to_string()), None]);
    }

    #[test]
    fn url_parsing() {
        let c = MysqlConfig::from_url("mysql://root:secret@127.0.0.1:3307/shop").unwrap();
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 3307);
        assert_eq!(c.user, "root");
        assert_eq!(c.password, "secret");
        assert_eq!(c.database.as_deref(), Some("shop"));

        let d = MysqlConfig::from_url("mysql://app@db.local/mydb").unwrap();
        assert_eq!(d.port, 3306);
        assert_eq!(d.password, "");
        assert_eq!(d.host, "db.local");

        assert!(MysqlConfig::from_url("postgres://x").is_err());
    }

    #[test]
    fn native_scramble_shape() {
        // Empty password → empty response (MySQL convention).
        assert!(scramble_native(b"", &[0u8; 20]).is_empty());
        // Non-empty → 20-byte SHA-1-sized response, deterministic.
        let salt: Vec<u8> = (1..=20).collect();
        let a = scramble_native(b"secret", &salt);
        let b = scramble_native(b"secret", &salt);
        assert_eq!(a.len(), 20);
        assert_eq!(a, b);
        // Different salt → different scramble.
        let c = scramble_native(b"secret", &[0u8; 20]);
        assert_ne!(a, c);
    }

    #[test]
    fn ok_and_err_packets() {
        // OK: 0x00, affected=1 (lenenc), last_insert_id=42 (lenenc), status, warnings
        let ok = [0x00, 0x01, 0x2A, 0x00, 0x00, 0x00, 0x00];
        match parse_ok_packet(&ok) {
            QueryResult::Ok { affected_rows, last_insert_id } => {
                assert_eq!(affected_rows, 1);
                assert_eq!(last_insert_id, 42);
            }
            _ => panic!("expected Ok"),
        }
        // ERR 1062 (dup entry) → UniqueViolation with the key column.
        let mut err = vec![0xFF, 0x26, 0x04, b'#', b'2', b'3', b'0', b'0', b'0'];
        err.extend_from_slice(b"Duplicate entry 'x' for key 'users.email'");
        match parse_err_packet(&err) {
            OrmError::UniqueViolation { field } => assert_eq!(field, "email"),
            other => panic!("expected UniqueViolation, got {other:?}"),
        }
    }

    // --- Against a real server (skip unless KERNWAY_MYSQL_URL is set) -------

    /// Run `body` on a fresh connection, or skip when no server is configured.
    fn live<F, Fut>(name: &str, body: F)
    where
        F: FnOnce(Conn) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let Ok(url) = std::env::var("KERNWAY_MYSQL_URL") else {
            eprintln!("skip {name}: KERNWAY_MYSQL_URL not set");
            return;
        };
        let cfg = MysqlConfig::from_url(&url).unwrap();
        rt_core::Executor::new()
            .unwrap()
            .block_on(async move { body(Conn::connect(&cfg).await.expect("connect")).await })
            .unwrap();
    }

    fn one_value(r: QueryResult) -> Option<String> {
        match r {
            QueryResult::Rows { mut rows, .. } => rows.remove(0).remove(0),
            QueryResult::Ok { .. } => panic!("expected a row"),
        }
    }

    #[test]
    fn a_cached_statement_runs_in_one_round_trip_and_survives_a_server_reset() {
        live("stmt_cache_reset", |mut conn| async move {
            let sql = "SELECT ? + 1";
            let r = conn.exec_prepared(sql, &[BindValue::Int(1)]).await.unwrap();
            assert_eq!(one_value(r).as_deref(), Some("2"));
            assert_eq!(conn.cached_statements(), 1);

            // Same SQL again: served from the cache, nothing new prepared.
            let r = conn.exec_prepared(sql, &[BindValue::Int(41)]).await.unwrap();
            assert_eq!(one_value(r).as_deref(), Some("42"));
            assert_eq!(conn.cached_statements(), 1);

            // COM_RESET_CONNECTION drops every server-side statement, so the
            // cached id is now unknown (ER_UNKNOWN_STMT_HANDLER) — the driver must
            // re-prepare transparently instead of failing the query.
            conn.seq = 0;
            conn.write_packet(&[0x1f]).await.unwrap();
            let ok = conn.read_packet().await.unwrap();
            assert_eq!(ok.first(), Some(&0x00), "reset acknowledged");

            let r = conn.exec_prepared(sql, &[BindValue::Int(9)]).await.unwrap();
            assert_eq!(one_value(r).as_deref(), Some("10"));
            assert_eq!(conn.cached_statements(), 1);
        });
    }

    #[test]
    fn a_full_statement_cache_evicts_and_closes_the_oldest() {
        live("stmt_cache_evict", |mut conn| async move {
            let extra = 20;
            for i in 0..STMT_CACHE_CAP + extra {
                let sql = format!("SELECT ? + {i}");
                conn.exec_prepared(&sql, &[BindValue::Int(0)]).await.unwrap();
            }
            assert_eq!(conn.cached_statements(), STMT_CACHE_CAP);
            // The oldest were evicted; the newest are still cached.
            assert!(!conn.stmts.contains("SELECT ? + 0"));
            assert!(conn.stmts.contains(&format!("SELECT ? + {}", STMT_CACHE_CAP + extra - 1)));

            // Evicted statements were really closed on the server: this session
            // sent exactly one COM_STMT_CLOSE per eviction (session status, no
            // privileges needed).
            let r = conn
                .query("SHOW SESSION STATUS LIKE 'Com_stmt_close'")
                .await
                .unwrap();
            let closed = match r {
                QueryResult::Rows { rows, .. } => rows[0][1].clone(),
                QueryResult::Ok { .. } => panic!("expected a status row"),
            };
            assert_eq!(closed.as_deref(), Some(extra.to_string().as_str()));
            // And an evicted statement still works (it is simply prepared again).
            let r = conn.exec_prepared("SELECT ? + 0", &[BindValue::Int(5)]).await.unwrap();
            assert_eq!(one_value(r).as_deref(), Some("5"));
        });
    }

    #[test]
    fn a_large_packet_grows_the_read_buffer_then_it_shrinks_back() {
        live("large_packet", |mut conn| async move {
            let big = 1024 * 1024 + 7;
            let r = conn.query(&format!("SELECT REPEAT('x', {big})")).await.unwrap();
            assert_eq!(one_value(r).map(|v| v.len()), Some(big));
            assert!(conn.io.read_buffer_len() <= READ_RETAIN, "buffer shrunk once drained");

            // Many small rows in one response, then a normal query: the stream
            // stays in sync across buffer refills.
            conn.query("SET SESSION cte_max_recursion_depth = 10000").await.unwrap();
            let r = conn
                .query("SELECT seq FROM (WITH RECURSIVE s(seq) AS (SELECT 1 UNION ALL \
                        SELECT seq + 1 FROM s WHERE seq < 5000) SELECT seq FROM s) t")
                .await
                .unwrap();
            match r {
                QueryResult::Rows { rows, .. } => {
                    assert_eq!(rows.len(), 5000);
                    assert_eq!(rows[4999][0].as_deref(), Some("5000"));
                }
                QueryResult::Ok { .. } => panic!("expected rows"),
            }
            let r = conn.exec_prepared("SELECT ?", &[BindValue::Str("ok".into())]).await.unwrap();
            assert_eq!(one_value(r).as_deref(), Some("ok"));
        });
    }

    // --- TLS + auth plugins against a real server ---------------------------
    //
    // KERNWAY_MYSQL_URL       a mysql_native_password account (required)
    // KERNWAY_MYSQL_CA        PEM of the server's CA (verify_ca / verify_identity)
    // KERNWAY_MYSQL_SHA2_URL  a caching_sha2_password account
    // KERNWAY_MYSQL_ROOT_URL  an account allowed to FLUSH PRIVILEGES (empties the
    //                         sha2 auth cache, forcing a full authentication)

    fn env_cfg(var: &str) -> Option<MysqlConfig> {
        std::env::var(var).ok().map(|u| MysqlConfig::from_url(&u).unwrap())
    }

    fn block<F: std::future::Future<Output = ()>>(f: F) {
        rt_core::Executor::new().unwrap().block_on(f).unwrap();
    }

    /// `Ssl_cipher` of the session as the server sees it ("" = plaintext).
    async fn server_cipher(conn: &mut Conn) -> String {
        match conn.query("SHOW SESSION STATUS LIKE 'Ssl_cipher'").await.unwrap() {
            QueryResult::Rows { rows, .. } => rows[0][1].clone().unwrap_or_default(),
            QueryResult::Ok { .. } => panic!("expected a status row"),
        }
    }

    fn with_mode(mut cfg: MysqlConfig, mode: SslMode, ca: Option<&str>) -> MysqlConfig {
        cfg.ssl_mode = mode;
        cfg.ssl_ca = ca.map(std::path::PathBuf::from);
        cfg
    }

    #[test]
    fn url_options_set_the_ssl_mode_and_ca() {
        let cfg = MysqlConfig::from_url("mysql://u:p@h:3307/db?ssl-mode=VERIFY_IDENTITY&ssl-ca=/x/ca.pem")
            .unwrap();
        assert_eq!(cfg.database.as_deref(), Some("db"));
        assert_eq!(cfg.ssl_mode, SslMode::VerifyIdentity);
        assert_eq!(cfg.ssl_ca.as_deref(), Some(std::path::Path::new("/x/ca.pem")));
        assert_eq!(MysqlConfig::from_url("mysql://u@h").unwrap().ssl_mode, SslMode::Preferred);
        assert_eq!(
            MysqlConfig::from_url("mysql://u@h/db?sslmode=disabled").unwrap().ssl_mode,
            SslMode::Disabled
        );
        assert!(MysqlConfig::from_url("mysql://u@h/db?ssl-mode=maybe").is_err());
        assert!(MysqlConfig::from_url("mysql://u@h/db?bogus=1").is_err());
    }

    #[test]
    fn sha256_scramble_matches_the_reference_construction() {
        use sha2::{Digest, Sha256};
        let (pw, nonce) = (b"secret".as_slice(), [7u8; 20]);
        let got = scramble_sha256(pw, &nonce);
        // XOR-ing the scramble with SHA256(SHA256(SHA256(pw)) + nonce) must give
        // back SHA256(pw) — exactly what the server checks.
        let mut h = Sha256::new();
        h.update(Sha256::digest(Sha256::digest(pw)));
        h.update(nonce);
        let seed = h.finalize();
        let back: Vec<u8> = got.iter().zip(seed.iter()).map(|(a, b)| a ^ b).collect();
        assert_eq!(back, Sha256::digest(pw).to_vec());
        assert!(scramble_sha256(b"", &nonce).is_empty(), "empty password → empty response");
    }

    #[test]
    fn ssl_modes_against_a_live_server() {
        let Some(base) = env_cfg("KERNWAY_MYSQL_URL") else {
            eprintln!("skip ssl_modes: KERNWAY_MYSQL_URL not set");
            return;
        };
        block(async move {
            let mut plain = Conn::connect(&with_mode(base.clone(), SslMode::Disabled, None)).await.unwrap();
            assert!(!plain.io.is_tls());
            assert_eq!(server_cipher(&mut plain).await, "");

            for mode in [SslMode::Preferred, SslMode::Required] {
                let mut c = Conn::connect(&with_mode(base.clone(), mode, None)).await.unwrap();
                assert!(c.io.is_tls(), "{mode:?} must use TLS");
                assert_ne!(server_cipher(&mut c).await, "", "{mode:?}: server sees TLS");
                // Prepared statements and the read buffer work over TLS too.
                let r = c.exec_prepared("SELECT ?", &[BindValue::Str("tls".into())]).await.unwrap();
                assert_eq!(one_value(r).as_deref(), Some("tls"));
            }

            // Mozilla's roots do not include the server's private CA.
            let err = Conn::connect(&with_mode(base.clone(), SslMode::VerifyCa, None))
                .await
                .err()
                .expect("a private CA must not verify against public roots");
            assert!(err.to_string().contains("TLS handshake"), "{err}");

            let Ok(ca) = std::env::var("KERNWAY_MYSQL_CA") else {
                eprintln!("skip verify_ca/verify_identity: KERNWAY_MYSQL_CA not set");
                return;
            };
            let mut c = Conn::connect(&with_mode(base.clone(), SslMode::VerifyCa, Some(&ca)))
                .await
                .unwrap();
            assert_ne!(server_cipher(&mut c).await, "");

            // MySQL's auto-generated server certificate names no host, so a
            // strict identity check against an IP must refuse it.
            let err = Conn::connect(&with_mode(base.clone(), SslMode::VerifyIdentity, Some(&ca)))
                .await
                .err()
                .expect("verify_identity must reject a certificate not naming the host");
            assert!(err.to_string().contains("TLS handshake"), "{err}");
        });
    }

    #[test]
    fn caching_sha2_password_full_and_fast_auth() {
        let Some(sha2) = env_cfg("KERNWAY_MYSQL_SHA2_URL") else {
            eprintln!("skip caching_sha2: KERNWAY_MYSQL_SHA2_URL not set");
            return;
        };
        let root = env_cfg("KERNWAY_MYSQL_ROOT_URL");
        block(async move {
            // Empty the server's auth cache when we can, so the next login must
            // be a full authentication.
            if let Some(root) = &root {
                let mut r = Conn::connect(root).await.unwrap();
                r.query("FLUSH PRIVILEGES").await.unwrap();

                // Full auth without TLS and without the server's RSA key:
                // refused with a clear message, the password never sent.
                let err = Conn::connect(&with_mode(sha2.clone(), SslMode::Disabled, None))
                    .await
                    .err()
                    .expect("full auth must not run without TLS or an RSA key");
                assert!(err.to_string().contains("server-public-key"), "{err}");
            }

            // Over TLS: full authentication (password sent encrypted) succeeds…
            let mut c = Conn::connect(&with_mode(sha2.clone(), SslMode::Required, None)).await.unwrap();
            let r = c.query("SELECT CURRENT_USER()").await.unwrap();
            assert!(one_value(r).unwrap().starts_with("kw_sha2@"));

            // …and primes the cache, so a plaintext login now passes on the
            // SHA-256 scramble alone (fast auth).
            let mut c = Conn::connect(&with_mode(sha2.clone(), SslMode::Disabled, None)).await.unwrap();
            assert_eq!(server_cipher(&mut c).await, "");
            let r = c.exec_prepared("SELECT ? * 2", &[BindValue::Int(21)]).await.unwrap();
            assert_eq!(one_value(r).as_deref(), Some("42"));

            // A wrong password fails either way. Over TLS the server checks it and
            // answers 1045; in plaintext the scramble cannot match the cache, so the
            // server asks for a full auth — which is refused without sending it
            // (no RSA key configured; see tests/auth.rs for the RSA path).
            let mut bad = sha2.clone();
            bad.password = "nope".into();
            let err = Conn::connect(&with_mode(bad.clone(), SslMode::Required, None))
                .await
                .err()
                .expect("wrong password must fail");
            assert!(err.to_string().contains("1045"), "{err}");
            let err = Conn::connect(&with_mode(bad, SslMode::Disabled, None))
                .await
                .err()
                .expect("wrong password must fail");
            assert!(err.to_string().contains("server-public-key"), "{err}");
        });
    }

    #[test]
    fn binary_cells_keep_numbers_as_numbers() {
        // Header 0x00, null bitmap (offset 2) marking column 2 NULL, then an
        // unsigned TINY 200, a signed LONGLONG -5, and a VAR_STRING "hi".
        let metas = [
            ColMeta { name: "a".into(), col_type: MYSQL_TYPE_TINY, unsigned: true, ..ColMeta::default() },
            ColMeta { name: "b".into(), col_type: MYSQL_TYPE_LONGLONG, unsigned: false, ..ColMeta::default() },
            ColMeta { name: "c".into(), col_type: MYSQL_TYPE_LONG, unsigned: false, ..ColMeta::default() },
            ColMeta { name: "d".into(), col_type: MYSQL_TYPE_VAR_STRING, unsigned: false, ..ColMeta::default() },
        ];
        let mut p = vec![0x00, 0b0001_0000];
        p.push(200);
        p.extend_from_slice(&(-5i64).to_le_bytes());
        p.extend_from_slice(&[2, b'h', b'i']);
        let mut cells = Vec::new();
        decode_binary_cells(&p, &metas, &mut cells).unwrap();
        assert_eq!(
            cells,
            [Cell::UInt(200), Cell::Int(-5), Cell::Null, Cell::Bytes(b"hi")]
        );
        // The text view matches what the text protocol would have sent.
        assert_eq!(cells_to_text(&cells).unwrap(), [
            Some("200".to_string()), Some("-5".to_string()), None, Some("hi".to_string())
        ]);
        assert_eq!(Cell::F32(0.1).to_text().as_deref(), Some("0.1"), "FLOAT keeps f32 formatting");
    }

    #[test]
    fn call_sql_quotes_identifiers_and_refuses_anything_else() {
        assert_eq!(call_sql("p", 0).unwrap(), "CALL `p`()");
        assert_eq!(call_sql("db.p_1$", 2).unwrap(), "CALL `db`.`p_1$`(?, ?)");
        for bad in ["", "a.b.c", "p;DROP", "p`x", "p x", ".p", "p."] {
            assert!(call_sql(bad, 0).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn status_flags_are_read_from_ok_and_eof_packets() {
        // OK: 0x00, affected=1, last_id=0, status=0x000A (MORE_RESULTS | AUTOCOMMIT).
        assert_eq!(ok_status(&[0x00, 0x01, 0x00, 0x0A, 0x00, 0x00, 0x00]), 0x000A);
        // EOF: 0xFE, warnings=0, status=0x0008.
        assert_eq!(eof_status(&[0xFE, 0x00, 0x00, 0x08, 0x00]) & SERVER_MORE_RESULTS_EXISTS, 0x0008);
        assert_eq!(ok_status(&[0x00]), 0, "a truncated OK reads as no flags");
    }
}
