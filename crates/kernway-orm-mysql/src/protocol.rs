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
//!   [`Conn::call`] / [`Conn::query_multi`]; every response is read to its end,
//!   so an error part-way never leaves the connection out of sync.
//!
//! ## Not yet
//!
//! - `caching_sha2_password` full auth **without** TLS (needs the server's RSA
//!   key exchange): use TLS, or rely on the server's auth cache (fast auth).
//! - Client certificates (mutual TLS).

use kernway_orm_core::error::OrmError;
use kernway_tls::{AsyncTlsStream, CertCheck};
use rt_net::AsyncTcpStream;
use std::collections::{HashMap, VecDeque};
use std::net::ToSocketAddrs;

// --- Capability flags (only the ones we assert) ---
const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
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
        for pair in options.split('&').filter(|p| !p.is_empty()) {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key.to_ascii_lowercase().replace('_', "-").as_str() {
                "ssl-mode" | "sslmode" => ssl_mode = SslMode::parse(value)?,
                "ssl-ca" => ssl_ca = Some(std::path::PathBuf::from(value)),
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
        Ok(Self { host, port, user, password, database, ssl_mode, ssl_ca })
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

/// One result-set cell, borrowed from the packet it arrived in where possible —
/// decoding a row allocates nothing for its cells.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell<'a> {
    /// SQL `NULL`.
    Null,
    /// A text-protocol value, or a binary string / decimal / blob / JSON value:
    /// the raw bytes as sent.
    Bytes(&'a [u8]),
    /// A binary signed integer (`TINY` … `LONGLONG`).
    Int(i64),
    /// A binary unsigned integer.
    UInt(u64),
    /// A binary `FLOAT`.
    F32(f32),
    /// A binary `DOUBLE`.
    F64(f64),
    /// A binary `DATE` / `DATETIME` / `TIMESTAMP` / `TIME`, already formatted.
    Owned(String),
}

impl Cell<'_> {
    /// The cell as text, as the text protocol would have sent it (`None` = NULL).
    pub fn to_text(&self) -> Option<String> {
        match self {
            Cell::Null => None,
            Cell::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            Cell::Int(i) => Some(i.to_string()),
            Cell::UInt(u) => Some(u.to_string()),
            Cell::F32(f) => Some(f.to_string()),
            Cell::F64(f) => Some(f.to_string()),
            Cell::Owned(s) => Some(s.clone()),
        }
    }
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
            Fetched::Rows { columns, rows } => QueryResult::Rows { columns, rows },
        }
    }
}

/// A row as text cells — the [`QueryResult`] shape.
fn cells_to_text(cells: &[Cell<'_>]) -> Result<Vec<Option<String>>, OrmError> {
    Ok(cells.iter().map(Cell::to_text).collect())
}

/// Re-type an **empty** cell vector to a new lifetime, keeping its allocation
/// (an in-place collect: same layout, no element is ever mapped). Lets one
/// scratch vector serve every row although each row borrows a different packet.
fn recycle<'b>(mut cells: Vec<Cell<'_>>) -> Vec<Cell<'b>> {
    cells.clear();
    cells.into_iter().map(|_| unreachable!("the vector is empty")).collect()
}

/// Bytes requested from the socket per read: one syscall drains many small
/// packets (a result set is one packet per row) instead of two reads per packet.
const READ_CHUNK: usize = 16 * 1024;
/// A read buffer grown past this (a large packet) is shrunk back once drained,
/// so one big row does not pin megabytes per idle pooled connection.
const READ_RETAIN: usize = 256 * 1024;
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
}

/// The socket under a connection: plaintext, or TLS once upgraded.
enum Stream {
    Plain(AsyncTcpStream),
    Tls(Box<AsyncTlsStream>),
    /// Transient, only while the plaintext socket is handed to the TLS handshake.
    Upgrading,
}

impl Stream {
    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf).await,
            Stream::Tls(s) => s.read(buf).await,
            Stream::Upgrading => Err(std::io::Error::other("connection lost during TLS upgrade")),
        }
    }

    async fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Stream::Plain(s) => s.write_all(buf).await,
            Stream::Tls(s) => s.write_all(buf).await,
            Stream::Upgrading => Err(std::io::Error::other("connection lost during TLS upgrade")),
        }
    }
}

/// A live, authenticated MySQL connection over the per-core async reactor.
pub struct Conn {
    stream: Stream,
    seq: u8,
    /// Read buffer: bytes `[rpos, rlen)` are received but not yet consumed.
    rbuf: Vec<u8>,
    rpos: usize,
    rlen: usize,
    /// Reused outgoing frame buffer (header + payload).
    wbuf: Vec<u8>,
    /// Assembly area for a payload split over 16 MiB continuation packets.
    big: Vec<u8>,
    /// Prepared statements by SQL text, plus their insertion order for eviction.
    stmts: HashMap<String, Stmt>,
    stmt_order: VecDeque<String>,
}

impl Conn {
    /// Connect, authenticate, and select the default database if one was given.
    pub async fn connect(cfg: &MysqlConfig) -> Result<Self, OrmError> {
        // DNS resolution is blocking, but happens once per connection; for an IP
        // literal it is a parse. (A future refinement: resolve on the blocking
        // pool for slow DNS.)
        let addr = (cfg.host.as_str(), cfg.port)
            .to_socket_addrs()
            .map_err(|e| conn_err(format!("resolve {}:{} failed: {e}", cfg.host, cfg.port)))?
            .next()
            .ok_or_else(|| conn_err(format!("no address for {}:{}", cfg.host, cfg.port)))?;
        let stream = AsyncTcpStream::connect(addr)
            .await
            .map_err(|e| conn_err(format!("connect {}:{} failed: {e}", cfg.host, cfg.port)))?;
        let mut conn = Self {
            stream: Stream::Plain(stream),
            seq: 0,
            rbuf: vec![0; READ_CHUNK],
            rpos: 0,
            rlen: 0,
            wbuf: Vec::with_capacity(READ_CHUNK),
            big: Vec::new(),
            stmts: HashMap::new(),
            stmt_order: VecDeque::new(),
        };
        conn.handshake(cfg).await?;
        Ok(conn)
    }

    /// Make at least `n` unconsumed bytes available in the read buffer, reading
    /// from the socket only when needed and up to [`READ_CHUNK`] (or the rest of
    /// a large packet) at a time — so a burst of small packets costs one syscall.
    async fn fill(&mut self, n: usize) -> Result<(), OrmError> {
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
            let got = self
                .stream
                .read(&mut self.rbuf[self.rlen..])
                .await
                .map_err(|e| conn_err(format!("read: {e}")))?;
            if got == 0 {
                return Err(conn_err("unexpected EOF from MySQL"));
            }
            self.rlen += got;
        }
        Ok(())
    }

    // --- Packet framing ---------------------------------------------------

    /// Once every buffered byte is consumed, rewind the read buffer — and give
    /// back memory a large packet made it (or the assembly area) grow.
    fn release_drained(&mut self) {
        if self.rpos == self.rlen {
            self.rpos = 0;
            self.rlen = 0;
            if self.rbuf.len() > READ_RETAIN {
                self.rbuf.truncate(READ_CHUNK);
                self.rbuf.shrink_to(READ_CHUNK);
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
            let start = self.rpos;
            self.rpos += len;
            return Ok(&self.rbuf[start..start + len]);
        }
        self.big.clear();
        loop {
            self.fill(len).await?;
            self.big
                .extend_from_slice(&self.rbuf[self.rpos..self.rpos + len]);
            self.rpos += len;
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
        let h = &self.rbuf[self.rpos..self.rpos + 4];
        let len = (h[0] as usize) | (h[1] as usize) << 8 | (h[2] as usize) << 16;
        self.seq = h[3].wrapping_add(1);
        self.rpos += 4;
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
        if payload.len() >= 0xFF_FFFF {
            return Err(conn_err("outgoing packet too large (>16 MiB not split)"));
        }
        let len = payload.len();
        self.wbuf.clear();
        self.wbuf
            .extend_from_slice(&[len as u8, (len >> 8) as u8, (len >> 16) as u8, self.seq]);
        self.wbuf.extend_from_slice(payload);
        self.seq = self.seq.wrapping_add(1);
        let result = self
            .stream
            .write_all(&self.wbuf)
            .await
            .map_err(|e| conn_err(format!("write packet: {e}")));
        if self.wbuf.capacity() > READ_RETAIN {
            self.wbuf = Vec::with_capacity(READ_CHUNK);
        }
        result
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
        let use_tls = cfg.ssl_mode != SslMode::Disabled && server_tls;
        if use_tls {
            self.write_packet(&handshake_header(cfg, CLIENT_SSL)).await?;
            self.upgrade_to_tls(cfg).await?;
        }

        // Answer with the plugin the server proposed; anything we do not speak
        // gets a native-password answer, which the server may switch away from.
        let plugin = match hello.plugin.as_str() {
            "caching_sha2_password" => "caching_sha2_password",
            _ => "mysql_native_password",
        };
        let auth = auth_response(plugin, cfg.password.as_bytes(), &hello.salt);
        let extra = if use_tls { CLIENT_SSL } else { 0 };
        self.write_packet(&build_handshake_response(cfg, &auth, plugin, extra))
            .await?;
        self.finish_auth(cfg, plugin, use_tls).await
    }

    /// Swap the plaintext socket for a TLS session over it, checking the server
    /// certificate as `cfg.ssl_mode` says.
    async fn upgrade_to_tls(&mut self, cfg: &MysqlConfig) -> Result<(), OrmError> {
        debug_assert_eq!(self.rpos, self.rlen, "no bytes may be buffered across the upgrade");
        let ca_pem = match &cfg.ssl_ca {
            Some(path) => Some(std::fs::read(path).map_err(|e| {
                conn_err(format!("read ssl-ca {}: {e}", path.display()))
            })?),
            None => None,
        };
        let tls_config = kernway_tls::client_config(ca_pem.as_deref(), cfg.ssl_mode.cert_check())
            .map_err(|e| conn_err(format!("TLS config: {e}")))?;
        let Stream::Plain(tcp) = std::mem::replace(&mut self.stream, Stream::Upgrading) else {
            return Err(conn_err("TLS upgrade on a connection that is not plaintext"));
        };
        let tls = AsyncTlsStream::connect(tcp, tls_config, &cfg.host)
            .await
            .map_err(|e| conn_err(format!("TLS handshake with {}: {e}", cfg.host)))?;
        self.stream = Stream::Tls(Box::new(tls));
        Ok(())
    }

    /// Drive the authentication exchange to `OK`: auth switches (`0xFE`) and the
    /// extra rounds of `caching_sha2_password` (`0x01`: fast-auth success, or a
    /// request for the full password — sent only over TLS).
    async fn finish_auth(
        &mut self,
        cfg: &MysqlConfig,
        mut plugin: &'static str,
        tls: bool,
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
                    Some(0x04) => {
                        return Err(conn_err(
                            "caching_sha2_password needs a full authentication, which this \
                             driver only does over TLS (a wrong password also ends up here, as \
                             the server cannot fast-auth it): connect with ssl-mode=preferred \
                             (or stricter) to a TLS-enabled server, or use mysql_native_password",
                        ))
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
        self.seq = 0;
        let mut payload = Vec::with_capacity(sql.len() + 1);
        payload.push(0x03); // COM_QUERY
        payload.extend_from_slice(sql.as_bytes());
        self.write_packet(&payload).await?;
        let first = self.read_packet().await?;
        let results = self.read_results(first, false, &mut row, false).await?;
        Ok(first_result(results))
    }

    /// Run one textual statement and return **every** result of its response —
    /// e.g. a `CALL` whose procedure runs several `SELECT`s yields one
    /// [`QueryResult::Rows`] per `SELECT`, then the [`QueryResult::Ok`] of the call.
    pub async fn query_multi(&mut self, sql: &str) -> Result<Vec<QueryResult>, OrmError> {
        self.seq = 0;
        let mut payload = Vec::with_capacity(sql.len() + 1);
        payload.push(0x03); // COM_QUERY
        payload.extend_from_slice(sql.as_bytes());
        self.write_packet(&payload).await?;
        let first = self.read_packet().await?;
        let mut row = cells_to_text;
        let results = self.read_results(first, false, &mut row, true).await?;
        Ok(results.into_iter().map(QueryResult::from).collect())
    }

    /// Call stored procedure `name` with bound arguments (a prepared
    /// `CALL name(?, …)`) and return every result: one [`QueryResult::Rows`] per
    /// result set the procedure produces, then the [`QueryResult::Ok`] of the call.
    /// `name` may be schema-qualified (`db.proc`); it must be a plain identifier.
    /// `OUT` parameters are read the MySQL way: pass a user variable in SQL via
    /// [`query_multi`](Self::query_multi) (`CALL p(?, @out)`), then `SELECT @out`.
    pub async fn call(&mut self, name: &str, args: &[BindValue]) -> Result<Vec<QueryResult>, OrmError> {
        let sql = call_sql(name, args.len())?;
        let mut row = cells_to_text;
        let results = self.exec_prepared_inner(&sql, args, &mut row, true).await?;
        Ok(results.into_iter().map(QueryResult::from).collect())
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
        F: FnMut(&[Cell<'_>]) -> Result<R, OrmError>,
    {
        let mut results = Vec::new();
        let mut deferred: Option<OrmError> = None;
        loop {
            let (result, more) = match first.first().copied() {
                Some(0x00) => {
                    let more = ok_status(&first) & SERVER_MORE_RESULTS_EXISTS != 0;
                    (Ok(ok_fetched(&first)), more)
                }
                Some(0xFF) => return Err(parse_err_packet(&first)),
                Some(0xFB) => return Err(OrmError::Query("LOCAL INFILE not supported".into())),
                _ => {
                    let (col_count, _) = read_lenenc_int(&first, 0)
                        .ok_or_else(|| OrmError::Query("bad column count packet".into()))?;
                    // Rows of a result set we are not keeping are read and dropped.
                    if keep_all || results.is_empty() {
                        self.read_rows(col_count as usize, binary, row).await?
                    } else {
                        let (_, more) = self
                            .read_rows(col_count as usize, binary, &mut |_: &[Cell<'_>]| Ok(()))
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
        F: FnMut(&[Cell<'_>]) -> Result<R, OrmError>,
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
            let outcome = decode_row(packet, binary, &metas, &mut cells).and_then(|()| row(&cells));
            *scratch = recycle(cells);
            match outcome {
                Ok(r) => rows.push(r),
                Err(e) => failed = Some(e),
            }
        };
        // EOF after the column definitions (CLIENT_DEPRECATE_EOF is not set). A
        // text result set that skips it starts with its first row instead.
        let after_cols = self.read_packet_ref().await?;
        if !binary && !is_eof(after_cols) {
            take(after_cols, &mut scratch);
        }
        let more = loop {
            let packet = self.read_packet_ref().await?;
            if is_eof(packet) {
                break eof_status(packet) & SERVER_MORE_RESULTS_EXISTS != 0;
            }
            if packet.first() == Some(&0xFF) {
                return Err(parse_err_packet(packet));
            }
            take(packet, &mut scratch);
        };
        self.release_drained();
        let result = match failed {
            Some(e) => Err(e),
            None => Ok(Fetched::Rows { columns, rows }),
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
        F: FnMut(&[Cell<'_>]) -> Result<R, OrmError>,
    {
        let cached = self.stmts.get(sql).copied();
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
        if self.stmts.len() >= STMT_CACHE_CAP {
            if let Some(oldest) = self.stmt_order.pop_front() {
                if let Some(old) = self.stmts.remove(&oldest) {
                    self.stmt_close(old.id).await?;
                }
            }
        }
        let (id, num_params) = self.stmt_prepare(sql).await?;
        let stmt = Stmt { id, num_params };
        self.stmts.insert(sql.to_string(), stmt);
        self.stmt_order.push_back(sql.to_string());
        Ok(stmt)
    }

    /// Drop a cached statement the server no longer knows (no `COM_STMT_CLOSE`:
    /// the id is already gone server-side).
    fn forget_stmt(&mut self, sql: &str) {
        self.stmts.remove(sql);
        self.stmt_order.retain(|s| s != sql);
    }

    /// Number of prepared statements currently cached on this connection.
    #[cfg(test)]
    fn cached_statements(&self) -> usize {
        self.stmts.len()
    }

    async fn stmt_prepare(&mut self, sql: &str) -> Result<(u32, u16), OrmError> {
        self.seq = 0;
        let mut payload = Vec::with_capacity(sql.len() + 1);
        payload.push(0x16); // COM_STMT_PREPARE
        payload.extend_from_slice(sql.as_bytes());
        self.write_packet(&payload).await?;

        let ok = self.read_packet().await?;
        if ok.first() == Some(&0xFF) {
            return Err(parse_err_packet(&ok));
        }
        if ok.first() != Some(&0x00) || ok.len() < 9 {
            return Err(OrmError::Query("bad COM_STMT_PREPARE response".into()));
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
        Ok((stmt_id, num_params))
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
        F: FnMut(&[Cell<'_>]) -> Result<R, OrmError>,
    {
        if params.len() != num_params as usize {
            return Err(OrmError::Query(format!(
                "prepared statement expects {num_params} params, got {}",
                params.len()
            )));
        }
        self.seq = 0;
        let mut payload = Vec::with_capacity(16 + params.len() * 8);
        payload.push(0x17); // COM_STMT_EXECUTE
        payload.extend_from_slice(&stmt_id.to_le_bytes());
        payload.push(0x00); // flags: CURSOR_TYPE_NO_CURSOR
        payload.extend_from_slice(&1u32.to_le_bytes()); // iteration count

        if num_params > 0 {
            // NULL bitmap.
            let bitmap_len = (num_params as usize + 7) / 8;
            let mut bitmap = vec![0u8; bitmap_len];
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
        self.seq = 0;
        let mut payload = Vec::with_capacity(5);
        payload.push(0x19); // COM_STMT_CLOSE
        payload.extend_from_slice(&stmt_id.to_le_bytes());
        self.write_packet(&payload).await // no response packet
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
        | CLIENT_LONG_FLAG
        | CLIENT_PROTOCOL_41
        | CLIENT_TRANSACTIONS
        | CLIENT_SECURE_CONNECTION
        | CLIENT_PLUGIN_AUTH
        | CLIENT_MULTI_RESULTS
        | CLIENT_PS_MULTI_RESULTS
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
fn call_sql(name: &str, args: usize) -> Result<String, OrmError> {
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
struct ColMeta {
    name: String,
    col_type: u8,
    unsigned: bool,
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
            None => return ColMeta { name, col_type: MYSQL_TYPE_VAR_STRING, unsigned: false },
        }
    }
    // lenenc fixed-length (0x0c), charset(2), column_length(4), type(1), flags(2).
    let (_fixed, consumed) = read_lenenc_int(def, i).unwrap_or((0, 1));
    i += consumed + 2 + 4;
    let col_type = *def.get(i).unwrap_or(&MYSQL_TYPE_VAR_STRING);
    let flags = def
        .get(i + 1..i + 3)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .unwrap_or(0);
    ColMeta { name, col_type, unsigned: flags & 0x0020 != 0 }
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
        MYSQL_TYPE_DATE | MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP => {
            decode_datetime(p, i).map(|(s, next)| (Cell::Owned(s), next))
        }
        MYSQL_TYPE_TIME => decode_time(p, i).map(|(s, next)| (Cell::Owned(s), next)),
        // DECIMAL / NEWDECIMAL / VARCHAR / VAR_STRING / STRING / BLOB / JSON / BIT / …
        _ => {
            let (bytes, next) = read_lenenc_str(p, i).ok_or_else(short)?;
            Ok((Cell::Bytes(bytes), next))
        }
    }
}

fn decode_datetime(p: &[u8], i: usize) -> Result<(String, usize), OrmError> {
    let short = || OrmError::Query("short datetime".into());
    let len = *p.get(i).ok_or_else(short)? as usize;
    let d = p.get(i + 1..i + 1 + len).ok_or_else(short)?;
    let s = match len {
        0 => "0000-00-00 00:00:00".to_string(),
        4 => format!("{:04}-{:02}-{:02}", u16::from_le_bytes([d[0], d[1]]), d[2], d[3]),
        7 => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            u16::from_le_bytes([d[0], d[1]]), d[2], d[3], d[4], d[5], d[6]
        ),
        _ => {
            let micros = if d.len() >= 11 { u32::from_le_bytes([d[7], d[8], d[9], d[10]]) } else { 0 };
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:06}",
                u16::from_le_bytes([d[0], d[1]]), d[2], d[3], d[4], d[5], d[6], micros
            )
        }
    };
    Ok((s, i + 1 + len))
}

fn decode_time(p: &[u8], i: usize) -> Result<(String, usize), OrmError> {
    let short = || OrmError::Query("short time".into());
    let len = *p.get(i).ok_or_else(short)? as usize;
    let d = p.get(i + 1..i + 1 + len).ok_or_else(short)?;
    let s = match len {
        0 => "00:00:00".to_string(),
        _ => {
            let neg = d[0] == 1;
            let hours = u32::from_le_bytes([d[1], d[2], d[3], d[4]]) * 24 + d[5] as u32;
            format!("{}{:02}:{:02}:{:02}", if neg { "-" } else { "" }, hours, d[6], d[7])
        }
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
            assert_eq!(conn.stmt_order.len(), STMT_CACHE_CAP);
            // The oldest were evicted; the newest are still cached.
            assert!(!conn.stmts.contains_key("SELECT ? + 0"));
            assert!(conn.stmts.contains_key(&format!("SELECT ? + {}", STMT_CACHE_CAP + extra - 1)));

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
            assert!(conn.rbuf.len() <= READ_RETAIN, "buffer shrunk once drained");

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
            assert!(matches!(plain.stream, Stream::Plain(_)));
            assert_eq!(server_cipher(&mut plain).await, "");

            for mode in [SslMode::Preferred, SslMode::Required] {
                let mut c = Conn::connect(&with_mode(base.clone(), mode, None)).await.unwrap();
                assert!(matches!(c.stream, Stream::Tls(_)), "{mode:?} must use TLS");
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

                // Full auth without TLS: refused with a clear message, and the
                // password never goes out in plaintext.
                let err = Conn::connect(&with_mode(sha2.clone(), SslMode::Disabled, None))
                    .await
                    .err()
                    .expect("full auth must not run without TLS");
                assert!(err.to_string().contains("only does over TLS"), "{err}");
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
            // server asks for a full auth — which is refused without sending it.
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
            assert!(err.to_string().contains("only does over TLS"), "{err}");
        });
    }

    #[test]
    fn recycle_keeps_the_allocation_across_lifetimes() {
        let packet = [1u8, b'x'];
        let mut cells: Vec<Cell<'_>> = Vec::with_capacity(8);
        cells.push(Cell::Bytes(&packet[1..]));
        let ptr = cells.as_ptr() as usize;
        let back: Vec<Cell<'static>> = recycle(cells);
        assert!(back.is_empty());
        assert_eq!(back.capacity(), 8);
        assert_eq!(back.as_ptr() as usize, ptr, "same buffer, no new allocation");
    }

    #[test]
    fn binary_cells_keep_numbers_as_numbers() {
        // Header 0x00, null bitmap (offset 2) marking column 2 NULL, then an
        // unsigned TINY 200, a signed LONGLONG -5, and a VAR_STRING "hi".
        let metas = [
            ColMeta { name: "a".into(), col_type: MYSQL_TYPE_TINY, unsigned: true },
            ColMeta { name: "b".into(), col_type: MYSQL_TYPE_LONGLONG, unsigned: false },
            ColMeta { name: "c".into(), col_type: MYSQL_TYPE_LONG, unsigned: false },
            ColMeta { name: "d".into(), col_type: MYSQL_TYPE_VAR_STRING, unsigned: false },
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
