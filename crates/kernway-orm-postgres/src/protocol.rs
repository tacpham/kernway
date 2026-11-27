//! The PostgreSQL frontend/backend protocol (version 3.0), client side, on
//! the Kernway reactor.
//!
//! Every message after start-up is `tag (1 byte) | length (4, counting
//! itself) | body`, integers big-endian. A connection:
//!
//! 1. **Opens** a TCP or Unix-domain socket; for TLS sends `SSLRequest` and
//!    upgrades the socket when the server answers `S`.
//! 2. **Starts up** (`user`, `database`, session settings) and
//!    **authenticates**: SCRAM-SHA-256 (with `-PLUS` channel binding over
//!    TLS), MD5, clear-text password, or none (trust / certificate).
//! 3. Runs statements with the **extended protocol** — `Parse` (prepared
//!    once, then cached by name), `Bind` (parameters typed by the server's
//!    `ParameterDescription`; numbers / booleans / bytea results asked for
//!    in binary), `Execute`, `Sync` — or the **simple protocol** (`Query`)
//!    for scripts and transaction control.
//! 4. Streams **cursors** (a named portal, `Execute` with a row limit),
//!    **pipelines** batches (many `Bind`/`Execute` before one `Sync`),
//!    **COPY**s in and out, and collects asynchronous **notices** and
//!    **LISTEN / NOTIFY** notifications.

use crate::auth;
use kernway_wire::scram::{self, Binding, Hash, Scram};
use crate::config::{ChannelBinding, Host, PgConfig, SslMode, TargetSession};
use crate::types;
use kernway_orm_core::{decode::Cell, error::OrmError};
use kernway_orm_sql::{Param, Value};
use kernway_tls::rustls::ClientConfig;
use kernway_tls::{CertCheck, ClientIdentity};
use kernway_wire::{BufStream, StmtCache};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;

const PROTOCOL_3_0: i32 = 196_608;
const SSL_REQUEST: i32 = 80_877_103;
const CANCEL_REQUEST: i32 = 80_877_102;
/// Rows of a pipelined batch sent before their replies are read, so neither
/// side's socket buffer can fill up while the other is still writing.
const BATCH_FLIGHT: usize = 256;
/// COPY data is sent in chunks of this size.
const COPY_CHUNK: usize = 64 * 1024;

pub(crate) fn conn_err(m: impl Into<String>) -> OrmError {
    OrmError::Connection(m.into())
}

fn io_err(e: std::io::Error) -> OrmError {
    conn_err(format!("postgres connection: {e}"))
}

fn proto_err(m: impl std::fmt::Display) -> OrmError {
    conn_err(format!("postgres protocol: {m}"))
}

/// An `ErrorResponse` / `NoticeResponse`, field by field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PgError {
    /// `ERROR`, `FATAL`, `PANIC` (notices: `WARNING`, `NOTICE`, `INFO`, …).
    pub severity: String,
    /// The SQLSTATE code (`23505`).
    pub code: String,
    /// The primary message.
    pub message: String,
    /// Detail, when the server gives one.
    pub detail: Option<String>,
    /// A hint.
    pub hint: Option<String>,
    /// 1-based character position of the error in the statement.
    pub position: Option<u32>,
    /// Where it happened (PL/pgSQL call stack).
    pub context: Option<String>,
    /// Schema, table, column, data type, constraint names, when relevant.
    pub schema: Option<String>,
    /// Table name.
    pub table: Option<String>,
    /// Column name.
    pub column: Option<String>,
    /// Constraint name.
    pub constraint: Option<String>,
}

impl PgError {
    fn parse(mut b: &[u8]) -> Self {
        let mut e = PgError::default();
        while let Some((&code, rest)) = b.split_first() {
            if code == 0 {
                break;
            }
            let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
            let v = String::from_utf8_lossy(&rest[..end]).into_owned();
            b = rest.get(end + 1..).unwrap_or(&[]);
            match code {
                b'V' => e.severity = v,
                b'S' if e.severity.is_empty() => e.severity = v,
                b'C' => e.code = v,
                b'M' => e.message = v,
                b'D' => e.detail = Some(v),
                b'H' => e.hint = Some(v),
                b'P' => e.position = v.parse().ok(),
                b'W' => e.context = Some(v),
                b's' => e.schema = Some(v),
                b't' => e.table = Some(v),
                b'c' => e.column = Some(v),
                b'n' => e.constraint = Some(v),
                _ => {}
            }
        }
        e
    }

    /// The error as an [`OrmError`]: unique / foreign-key violations get
    /// their own variants; the rest is a query error carrying the SQLSTATE.
    pub fn to_orm(&self) -> OrmError {
        match self.code.as_str() {
            "23505" => {
                // DETAIL: Key (email)=(a@x) already exists.
                let field = self
                    .detail
                    .as_deref()
                    .and_then(|d| d.strip_prefix("Key ("))
                    .and_then(|d| d.split_once(")="))
                    .map(|(cols, _)| cols.to_string())
                    .or_else(|| self.constraint.clone())
                    .unwrap_or_default();
                OrmError::UniqueViolation { field }
            }
            "23503" => OrmError::ForeignKeyViolation,
            _ => {
                let mut m = format!("{} {}: {}", self.severity, self.code, self.message);
                if let Some(d) = &self.detail {
                    m.push_str(&format!(" (detail: {d})"));
                }
                if let Some(h) = &self.hint {
                    m.push_str(&format!(" (hint: {h})"));
                }
                if let Some(p) = self.position {
                    m.push_str(&format!(" (at character {p})"));
                }
                if self.severity == "FATAL" || self.severity == "PANIC" || self.code.starts_with("08") || self.code.starts_with("57P") {
                    OrmError::Connection(m)
                } else {
                    OrmError::Query(m)
                }
            }
        }
    }
}

/// A `NOTIFY` received on a channel this connection listens to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// Backend process that sent it.
    pub process_id: i32,
    /// The channel.
    pub channel: String,
    /// The payload (may be empty).
    pub payload: String,
}

/// A result column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Column name (alias).
    pub name: String,
    /// Source table OID (0 for an expression).
    pub table_oid: u32,
    /// Column number in that table (0 for an expression).
    pub column_id: i16,
    /// Data type OID.
    pub type_oid: u32,
    /// Type modifier (`varchar(20)` → 24, `numeric(10,2)` …; -1 none).
    pub type_mod: i32,
    /// Wire format of this column in the result (0 text, 1 binary).
    pub format: i16,
}

/// A statement prepared on the server.
#[derive(Debug)]
pub struct Statement {
    /// The server-side name (`""` for the unnamed statement).
    pub name: String,
    /// Parameter types as the server resolved them.
    pub params: Vec<u32>,
    /// Result columns (empty for a statement without rows), with the result
    /// format the driver binds them with.
    pub columns: Vec<Column>,
}

/// One statement's outcome in a simple-protocol [`Conn::query`].
#[derive(Debug, Clone, PartialEq)]
pub enum QueryResult {
    /// A statement that returned rows.
    Rows {
        /// Columns.
        columns: Vec<Column>,
        /// Rows, typed by column.
        rows: Vec<Vec<Value>>,
        /// The command tag (`SELECT 3`).
        tag: String,
    },
    /// A statement without rows.
    Ok {
        /// The command tag (`UPDATE 3`, `CREATE TABLE`).
        tag: String,
        /// Rows affected, from the tag.
        affected: u64,
    },
}

/// What an extended-protocol execution ended with.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Executed {
    /// The command tag (`INSERT 0 1`); empty for an empty query, or when
    /// suspended at the row limit.
    pub tag: String,
    /// Rows affected / returned, from the tag.
    pub affected: u64,
    /// More rows remain in the portal (row limit reached).
    pub suspended: bool,
}

/// Everything needed to cancel the statement a connection is running, from
/// anywhere (another task, another thread).
#[derive(Clone)]
pub struct CancelToken {
    host: Host,
    tls: Option<(Arc<ClientConfig>, String)>,
    process_id: i32,
    secret_key: Vec<u8>,
}

impl std::fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancelToken").field("host", &self.host).field("process_id", &self.process_id).finish()
    }
}

impl CancelToken {
    /// Ask the server to cancel whatever the connection is running now. The
    /// statement then fails with SQLSTATE 57014; nothing happens if it has
    /// already finished.
    pub async fn cancel(&self) -> Result<(), OrmError> {
        let mut io = open_socket(&self.host).await?;
        if let Some((cfg, name)) = &self.tls {
            negotiate_tls(&mut io, cfg.clone(), name, true).await?;
        }
        let w = io.write_buf();
        w.extend_from_slice(&(12 + self.secret_key.len() as i32).to_be_bytes());
        w.extend_from_slice(&CANCEL_REQUEST.to_be_bytes());
        w.extend_from_slice(&self.process_id.to_be_bytes());
        w.extend_from_slice(&self.secret_key);
        io.flush().await.map_err(io_err)?;
        // The server closes the connection without answering.
        let _ = io.fill(1).await;
        Ok(())
    }
}

/// An open, authenticated connection.
pub struct Conn {
    io: BufStream,
    params: HashMap<String, String>,
    cancel: CancelToken,
    tx_status: u8,
    stmts: StmtCache<Rc<Statement>>,
    cache_cap: usize,
    next_name: u64,
    /// Statements evicted from the cache, closed with the next request.
    pending_close: Vec<String>,
    notices: Vec<PgError>,
    notifications: VecDeque<Notification>,
    pub(crate) cursors: HashMap<u64, Portal>,
    pub(crate) next_cursor: u64,
    last_error: Option<PgError>,
    broken: bool,
}

/// An open named portal (a server-side cursor).
pub(crate) struct Portal {
    pub(crate) name: String,
    pub(crate) stmt: Rc<Statement>,
    /// The driver opened a transaction block for it (outside a transaction
    /// a portal ends with the statement).
    pub(crate) own_tx: bool,
}

/// Write one tagged message: `tag`, then the body `f` writes, with the
/// length patched in.
fn msg(w: &mut Vec<u8>, tag: u8, f: impl FnOnce(&mut Vec<u8>)) {
    w.push(tag);
    let at = w.len();
    w.extend_from_slice(&[0; 4]);
    f(w);
    let len = (w.len() - at) as i32;
    w[at..at + 4].copy_from_slice(&len.to_be_bytes());
}

fn cstr(w: &mut Vec<u8>, s: &str) {
    w.extend_from_slice(s.as_bytes());
    w.push(0);
}

/// Reads a message body field by field.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn i16(&mut self) -> Result<i16, OrmError> {
        let (h, t) = self.0.split_at_checked(2).ok_or_else(|| proto_err("short message"))?;
        self.0 = t;
        Ok(i16::from_be_bytes([h[0], h[1]]))
    }
    fn i32(&mut self) -> Result<i32, OrmError> {
        let (h, t) = self.0.split_at_checked(4).ok_or_else(|| proto_err("short message"))?;
        self.0 = t;
        Ok(i32::from_be_bytes([h[0], h[1], h[2], h[3]]))
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], OrmError> {
        let (h, t) = self.0.split_at_checked(n).ok_or_else(|| proto_err("short message"))?;
        self.0 = t;
        Ok(h)
    }
    fn cstr(&mut self) -> Result<String, OrmError> {
        let end = self.0.iter().position(|&c| c == 0).ok_or_else(|| proto_err("unterminated string"))?;
        let s = String::from_utf8_lossy(&self.0[..end]).into_owned();
        self.0 = &self.0[end + 1..];
        Ok(s)
    }
}

/// The cells of a `DataRow`, borrowed from the message.
fn data_row<'a>(body: &'a [u8], out: &mut Vec<Option<&'a [u8]>>) -> Result<(), OrmError> {
    out.clear();
    let mut r = Reader(body);
    let n = r.i16()?;
    for _ in 0..n {
        let len = r.i32()?;
        out.push(if len < 0 { None } else { Some(r.bytes(len as usize)?) });
    }
    Ok(())
}

fn row_description(body: &[u8]) -> Result<Vec<Column>, OrmError> {
    let mut r = Reader(body);
    let n = r.i16()?;
    let mut cols = Vec::with_capacity(n.max(0) as usize);
    for _ in 0..n {
        let name = r.cstr()?;
        let table_oid = r.i32()? as u32;
        let column_id = r.i16()?;
        let type_oid = r.i32()? as u32;
        let _len = r.i16()?;
        let type_mod = r.i32()?;
        let format = r.i16()?;
        cols.push(Column { name, table_oid, column_id, type_oid, type_mod, format });
    }
    Ok(cols)
}

/// Rows counted in a command tag (`INSERT 0 5`, `UPDATE 3`, `SELECT 2`, `COPY 9`).
pub fn tag_count(tag: &str) -> u64 {
    let mut it = tag.split_whitespace();
    match it.next() {
        Some("INSERT" | "UPDATE" | "DELETE" | "SELECT" | "MERGE" | "COPY" | "FETCH" | "MOVE") => {
            tag.rsplit(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0)
        }
        _ => 0,
    }
}

async fn open_socket(host: &Host) -> Result<BufStream, OrmError> {
    match host {
        Host::Tcp(h, p) => kernway_wire::offload::connect_tcp(h, *p)
            .await
            .map(BufStream::new)
            .map_err(|e| conn_err(format!("connect {h}:{p} failed: {e}"))),
        Host::Unix(dir, port) => open_unix(&dir.join(format!(".s.PGSQL.{port}"))).await,
    }
}

#[cfg(unix)]
async fn open_unix(path: &std::path::Path) -> Result<BufStream, OrmError> {
    rt_net::AsyncUnixStream::connect(path)
        .await
        .map(BufStream::new_unix)
        .map_err(|e| conn_err(format!("connect {} failed: {e}", path.display())))
}

#[cfg(not(unix))]
async fn open_unix(path: &std::path::Path) -> Result<BufStream, OrmError> {
    Err(conn_err(format!("connect {}: Unix-domain sockets are not supported on this platform; use TCP", path.display())))
}

/// Send `SSLRequest`; upgrade when the server agrees. Returns whether TLS is on.
async fn negotiate_tls(io: &mut BufStream, cfg: Arc<ClientConfig>, name: &str, required: bool) -> Result<bool, OrmError> {
    let w = io.write_buf();
    w.extend_from_slice(&8i32.to_be_bytes());
    w.extend_from_slice(&SSL_REQUEST.to_be_bytes());
    io.flush().await.map_err(io_err)?;
    io.fill(1).await.map_err(io_err)?;
    match io.take(1)[0] {
        b'S' => {
            io.upgrade_tls(cfg, name).await.map_err(|e| conn_err(format!("TLS handshake with {name} failed: {e}")))?;
            Ok(true)
        }
        b'N' if required => Err(conn_err(format!("{name}: the server does not accept TLS connections, and sslmode requires TLS"))),
        b'N' => Ok(false),
        // An ErrorResponse here is a very old server; nothing else is valid.
        other => Err(proto_err(format!("unexpected answer {other:#x} to SSLRequest"))),
    }
}

impl Conn {
    /// Connect per `cfg`: each host in turn (each address of a name), until
    /// one accepts and matches `target_session_attrs`.
    ///
    /// Nothing blocks the shard: names resolve on the blocking pool,
    /// certificate files are read there, and every socket is non-blocking.
    pub async fn connect(cfg: &PgConfig) -> Result<Self, OrmError> {
        let tls = tls_config(cfg).await?;
        let passes: &[TargetSession] = match cfg.target_session_attrs {
            TargetSession::PreferStandby => &[TargetSession::Standby, TargetSession::Any],
            t => std::slice::from_ref(match t {
                TargetSession::Any => &TargetSession::Any,
                TargetSession::ReadWrite => &TargetSession::ReadWrite,
                TargetSession::ReadOnly => &TargetSession::ReadOnly,
                TargetSession::Primary => &TargetSession::Primary,
                _ => &TargetSession::Standby,
            }),
        };
        let mut errors = Vec::new();
        for want in passes {
            for host in &cfg.hosts {
                let attempt = rt_core::timeout(cfg.connect_timeout, Self::connect_host(cfg, host, tls.clone())).await;
                let conn = match attempt {
                    Ok(Ok(c)) => c,
                    Ok(Err(e)) => {
                        errors.push(format!("{}: {e}", host_label(host)));
                        continue;
                    }
                    Err(_) => {
                        errors.push(format!("{}: timed out after {:?}", host_label(host), cfg.connect_timeout));
                        continue;
                    }
                };
                let mut conn = conn;
                match conn.session_matches(*want).await {
                    Ok(true) => return Ok(conn),
                    Ok(false) => errors.push(format!("{}: not a {want:?} server", host_label(host))),
                    Err(e) => errors.push(format!("{}: {e}", host_label(host))),
                }
            }
        }
        Err(conn_err(format!("postgres: no host accepted the connection: {}", errors.join("; "))))
    }

    async fn connect_host(cfg: &PgConfig, host: &Host, tls: Option<Arc<ClientConfig>>) -> Result<Self, OrmError> {
        let mut io = open_socket(host).await?;
        let mut cancel_tls = None;
        if let (Host::Tcp(name, _), Some(cfg_tls)) = (host, tls) {
            if negotiate_tls(&mut io, cfg_tls.clone(), name, cfg.ssl_mode != SslMode::Prefer).await? {
                cancel_tls = Some((cfg_tls, name.clone()));
            }
        }
        let mut conn = Conn {
            io,
            params: HashMap::new(),
            cancel: CancelToken { host: host.clone(), tls: cancel_tls, process_id: 0, secret_key: Vec::new() },
            tx_status: b'I',
            stmts: StmtCache::new(cfg.statement_cache.max(1)),
            cache_cap: cfg.statement_cache,
            next_name: 1,
            pending_close: Vec::new(),
            notices: Vec::new(),
            notifications: VecDeque::new(),
            cursors: HashMap::new(),
            next_cursor: 1,
            last_error: None,
            broken: false,
        };
        conn.startup(cfg).await?;
        Ok(conn)
    }

    async fn startup(&mut self, cfg: &PgConfig) -> Result<(), OrmError> {
        let w = self.io.write_buf();
        w.extend_from_slice(&[0; 4]);
        w.extend_from_slice(&PROTOCOL_3_0.to_be_bytes());
        let mut kv = |k: &str, v: &str| {
            cstr(w, k);
            cstr(w, v);
        };
        kv("user", &cfg.user);
        kv("database", &cfg.database);
        kv("client_encoding", "UTF8");
        kv("DateStyle", "ISO, YMD");
        kv("IntervalStyle", "postgres");
        kv("extra_float_digits", "3");
        if let Some(app) = &cfg.application_name {
            kv("application_name", app);
        }
        for (k, v) in &cfg.settings {
            kv(k, v);
        }
        w.push(0);
        let len = w.len() as i32;
        w[..4].copy_from_slice(&len.to_be_bytes());
        self.flush().await?;

        let mut scram: Option<Scram> = None;
        let mut bound = false;
        loop {
            let (tag, body) = self.recv_raw().await?;
            let body = body.to_vec();
            let body = &body[..];
            match tag {
                b'R' => {
                    let mut r = Reader(body);
                    let code = r.i32()?;
                    let rest = r.0.to_vec();
                    match code {
                        0 => {
                            if cfg.channel_binding == ChannelBinding::Require && !bound {
                                return Err(conn_err("channel_binding=require, but the server authenticated without SCRAM channel binding"));
                            }
                            if scram.is_some() {
                                // AuthenticationOk after SASLFinal.
                            }
                        }
                        3 => {
                            self.refuse_without_binding(cfg)?;
                            let pw = self.password(cfg)?;
                            msg(self.io.write_buf(), b'p', |w| cstr(w, &pw));
                            self.flush().await?;
                        }
                        5 => {
                            self.refuse_without_binding(cfg)?;
                            let pw = self.password(cfg)?;
                            let salt: [u8; 4] = rest.get(..4).and_then(|s| s.try_into().ok()).ok_or_else(|| proto_err("MD5 salt"))?;
                            let hashed = auth::md5_password(&cfg.user, &pw, salt);
                            msg(self.io.write_buf(), b'p', |w| cstr(w, &hashed));
                            self.flush().await?;
                        }
                        10 => {
                            let mechs: Vec<String> =
                                rest.split(|&b| b == 0).filter(|m| !m.is_empty()).map(|m| String::from_utf8_lossy(m).into_owned()).collect();
                            let plus = mechs.iter().any(|m| m == "SCRAM-SHA-256-PLUS");
                            let cert = self.io.peer_certificate().map(<[u8]>::to_vec);
                            let binding = match (cfg.channel_binding, plus, cert) {
                                (ChannelBinding::Disable, _, _) => Binding::None,
                                (_, true, Some(cert)) => Binding::Bind(scram::tls_server_end_point(&cert)),
                                (ChannelBinding::Require, _, _) => {
                                    return Err(conn_err("channel_binding=require, but the server offers no SCRAM-SHA-256-PLUS over this connection"))
                                }
                                (_, false, Some(_)) => Binding::Supported,
                                _ => Binding::None,
                            };
                            if !mechs.iter().any(|m| m == "SCRAM-SHA-256") && !matches!(binding, Binding::Bind(_)) {
                                return Err(conn_err(format!("no supported SASL mechanism among {mechs:?}")));
                            }
                            bound = matches!(binding, Binding::Bind(_));
                            let pw = self.password(cfg)?;
                            // The user name travels in the start-up message; SCRAM's stays empty.
                            let s = Scram::new(Hash::Sha256, "", pw.as_bytes(), scram::new_nonce().map_err(io_err)?, binding);
                            let first = s.client_first();
                            let mech = if s.is_bound() { "SCRAM-SHA-256-PLUS" } else { "SCRAM-SHA-256" };
                            msg(self.io.write_buf(), b'p', |w| {
                                cstr(w, mech);
                                w.extend_from_slice(&(first.len() as i32).to_be_bytes());
                                w.extend_from_slice(first.as_bytes());
                            });
                            self.flush().await?;
                            scram = Some(s);
                        }
                        11 => {
                            let s = scram.as_mut().ok_or_else(|| proto_err("SASLContinue without SASL"))?;
                            let fin = s.client_final(&String::from_utf8_lossy(&rest)).map_err(conn_err)?;
                            msg(self.io.write_buf(), b'p', |w| w.extend_from_slice(fin.as_bytes()));
                            self.flush().await?;
                        }
                        12 => {
                            let s = scram.as_ref().ok_or_else(|| proto_err("SASLFinal without SASL"))?;
                            s.verify_server(&String::from_utf8_lossy(&rest)).map_err(conn_err)?;
                        }
                        7..=9 => return Err(conn_err("GSSAPI / SSPI authentication is not supported; use SCRAM-SHA-256")),
                        other => return Err(conn_err(format!("unsupported authentication request {other}"))),
                    }
                }
                b'K' => {
                    let mut r = Reader(body);
                    self.cancel.process_id = r.i32()?;
                    self.cancel.secret_key = r.0.to_vec();
                }
                b'Z' => {
                    self.tx_status = *body.first().unwrap_or(&b'I');
                    return Ok(());
                }
                b'E' => {
                    let e = PgError::parse(body);
                    return Err(conn_err(format!("{} {}: {}", e.severity, e.code, e.message)));
                }
                b'v' => {} // NegotiateProtocolVersion: fine at 3.0
                other => return Err(proto_err(format!("unexpected message {:?} during start-up", other as char))),
            }
        }
    }

    fn password(&self, cfg: &PgConfig) -> Result<String, OrmError> {
        cfg.password.clone().ok_or_else(|| conn_err("the server asks for a password, and none is configured"))
    }

    fn refuse_without_binding(&self, cfg: &PgConfig) -> Result<(), OrmError> {
        if cfg.channel_binding == ChannelBinding::Require {
            return Err(conn_err("channel_binding=require, but the server asks for a password without SCRAM"));
        }
        Ok(())
    }

    async fn session_matches(&mut self, want: TargetSession) -> Result<bool, OrmError> {
        let standby = match self.params.get("in_hot_standby") {
            Some(v) => v == "on",
            None if want == TargetSession::Any => false,
            None => self.query_text("SELECT pg_catalog.pg_is_in_recovery()").await? == "t",
        };
        let read_only = self.params.get("default_transaction_read_only").is_some_and(|v| v == "on");
        Ok(match want {
            TargetSession::Any | TargetSession::PreferStandby => true,
            TargetSession::Primary => !standby,
            TargetSession::Standby => standby,
            TargetSession::ReadWrite => !standby && !read_only,
            TargetSession::ReadOnly => standby || read_only,
        })
    }

    async fn query_text(&mut self, sql: &str) -> Result<String, OrmError> {
        let rs = self.query(sql).await?;
        Ok(match rs.into_iter().next() {
            Some(QueryResult::Rows { rows, .. }) => {
                rows.into_iter().next().and_then(|r| r.into_iter().next()).map(|v| match v {
                    Value::Bool(b) => if b { "t" } else { "f" }.to_string(),
                    other => other.to_text().unwrap_or_default(),
                })
                .unwrap_or_default()
            }
            _ => String::new(),
        })
    }

    // ---- reading -------------------------------------------------------

    /// The next message, any kind. The body is valid until the next read.
    async fn recv_raw(&mut self) -> Result<(u8, &[u8]), OrmError> {
        loop {
            let (tag, len) = self.peek_header().await?;
            match tag {
                b'N' | b'A' | b'S' => {
                    let body = self.io.take(len + 5)[5..].to_vec();
                    self.asynchronous(tag, &body)?;
                }
                _ => return Ok((tag, &self.io.take(len + 5)[5..])),
            }
        }
    }

    async fn peek_header(&mut self) -> Result<(u8, usize), OrmError> {
        if self.broken {
            return Err(conn_err("postgres connection is broken (an earlier I/O error)"));
        }
        if let Err(e) = self.io.fill(5).await {
            self.broken = true;
            return Err(io_err(e));
        }
        let h = self.io.peek(5);
        let tag = h[0];
        let len = i32::from_be_bytes([h[1], h[2], h[3], h[4]]);
        if len < 4 {
            self.broken = true;
            return Err(proto_err(format!("message length {len}")));
        }
        let body = len as usize - 4;
        if let Err(e) = self.io.fill(body + 5).await {
            self.broken = true;
            return Err(io_err(e));
        }
        Ok((tag, body))
    }

    fn asynchronous(&mut self, tag: u8, body: &[u8]) -> Result<(), OrmError> {
        match tag {
            b'N' => {
                if self.notices.len() < 1000 {
                    self.notices.push(PgError::parse(body));
                }
            }
            b'A' => {
                let mut r = Reader(body);
                let process_id = r.i32()?;
                let channel = r.cstr()?;
                let payload = r.cstr()?;
                self.notifications.push_back(Notification { process_id, channel, payload });
            }
            _ => {
                let mut r = Reader(body);
                let k = r.cstr()?;
                let v = r.cstr()?;
                self.params.insert(k, v);
            }
        }
        Ok(())
    }

    /// Read up to and including `ReadyForQuery`, after an error was reported
    /// (the server skips the rest of the request until `Sync`).
    async fn drain_to_ready(&mut self) -> Result<(), OrmError> {
        loop {
            let (tag, body) = self.recv_raw().await?;
            if tag == b'Z' {
                self.tx_status = *body.first().unwrap_or(&b'I');
                return Ok(());
            }
        }
    }

    fn error(&mut self, body: &[u8]) -> OrmError {
        let e = PgError::parse(body);
        let orm = e.to_orm();
        if matches!(orm, OrmError::Connection(_)) {
            self.broken = true;
        }
        self.last_error = Some(e);
        orm
    }

    async fn flush(&mut self) -> Result<(), OrmError> {
        if self.broken {
            return Err(conn_err("postgres connection is broken (an earlier I/O error)"));
        }
        let r = self.io.flush().await.map_err(|e| {
            self.broken = true;
            io_err(e)
        });
        // The buffer is sent: requests are appended to an empty one.
        self.io.write_buf();
        r
    }

    /// Queue `Close` for statements evicted from the cache (their
    /// `CloseComplete`s are skipped by the reply loops).
    fn write_pending_closes(&mut self) {
        let names = std::mem::take(&mut self.pending_close);
        let w = self.io.write_buf_append();
        for n in names {
            msg(w, b'C', |w| {
                w.push(b'S');
                cstr(w, &n);
            });
        }
    }

    /// Close statement `name` on the server with the next request.
    pub(crate) fn queue_close(&mut self, name: &str) {
        if !name.is_empty() {
            self.pending_close.push(name.to_string());
        }
    }

    // ---- public surface ------------------------------------------------

    /// A server setting reported to the client (`server_version`,
    /// `TimeZone`, `in_hot_standby`, `application_name`, …).
    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.params.get(name).map(String::as_str)
    }

    /// The server version as `major * 10000 + minor` (`180001`).
    pub fn server_version_num(&self) -> u32 {
        let v = self.parameter("server_version").unwrap_or("0");
        let mut it = v.split(|c: char| !c.is_ascii_digit());
        let major: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        major * 10_000 + minor
    }

    /// The backend process id (`pg_backend_pid()`).
    pub fn process_id(&self) -> i32 {
        self.cancel.process_id
    }

    /// A token that cancels this connection's running statement from elsewhere.
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Whether a transaction block is open (also when it has failed and
    /// waits for `ROLLBACK`).
    pub fn in_transaction(&self) -> bool {
        self.tx_status != b'I'
    }

    /// Whether the open transaction has failed (every statement is refused
    /// until `ROLLBACK`).
    pub fn transaction_failed(&self) -> bool {
        self.tx_status == b'E'
    }

    /// Whether the connection runs over TLS.
    pub fn is_tls(&self) -> bool {
        self.io.is_tls()
    }

    /// Whether an I/O or fatal error has made the connection unusable.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// The full server error behind the last failed statement (SQLSTATE,
    /// detail, hint, position, constraint …).
    pub fn last_error(&self) -> Option<&PgError> {
        self.last_error.as_ref()
    }

    /// Notices (`RAISE NOTICE`, warnings) received so far; taking them clears the list.
    pub fn take_notices(&mut self) -> Vec<PgError> {
        std::mem::take(&mut self.notices)
    }

    /// Notifications received so far (on any channel this session `LISTEN`s
    /// to); taking them clears the queue.
    pub fn take_notifications(&mut self) -> Vec<Notification> {
        self.notifications.drain(..).collect()
    }

    /// Wait for the next notification. The connection must be idle (no
    /// statement running): it only reads. Safe to cancel (wrap it in a
    /// timeout): nothing is consumed until a whole message has arrived.
    pub async fn wait_notification(&mut self) -> Result<Notification, OrmError> {
        loop {
            if let Some(n) = self.notifications.pop_front() {
                return Ok(n);
            }
            let (tag, len) = self.peek_header().await?;
            let body = self.io.take(len + 5)[5..].to_vec();
            match tag {
                b'N' | b'A' | b'S' => self.asynchronous(tag, &body)?,
                b'E' => return Err(self.error(&body)),
                other => return Err(proto_err(format!("unexpected message {:?} while idle", other as char))),
            }
        }
    }

    /// Run `sql` with the **simple protocol**: one or more `;`-separated
    /// statements, no parameters, every result returned (text-format values,
    /// typed by column). Several statements run as one implicit transaction
    /// unless the script has its own `BEGIN` / `COMMIT`. Stops at the first
    /// error.
    pub async fn query(&mut self, sql: &str) -> Result<Vec<QueryResult>, OrmError> {
        self.write_pending_closes();
        msg(self.io.write_buf_append(), b'Q', |w| cstr(w, sql));
        self.flush().await?;
        let mut out = Vec::new();
        let mut columns: Option<Vec<Column>> = None;
        let mut rows = Vec::new();
        let mut failed = None;
        loop {
            let (tag, body) = self.recv_raw().await?;
            match tag {
                b'T' => {
                    columns = Some(row_description(body)?);
                    rows = Vec::new();
                }
                b'D' => {
                    let cols = columns.as_ref().ok_or_else(|| proto_err("DataRow without RowDescription"))?;
                    let mut cells = Vec::with_capacity(cols.len());
                    data_row(body, &mut cells)?;
                    rows.push(cols.iter().zip(&cells).map(|(c, v)| types::value(c.type_oid, c.format, *v)).collect());
                }
                b'C' => {
                    let tag = Reader(body).cstr()?;
                    match columns.take() {
                        Some(columns) => out.push(QueryResult::Rows { columns, rows: std::mem::take(&mut rows), tag }),
                        None => {
                            let affected = tag_count(&tag);
                            out.push(QueryResult::Ok { tag, affected });
                        }
                    }
                }
                b'I' => {}
                b'3' => {}
                b'E' => {
                    let body = body.to_vec();
                    failed = Some(self.error(&body));
                }
                b'G' => {
                    // COPY FROM STDIN through query(): no data to send.
                    msg(self.io.write_buf(), b'f', |w| cstr(w, "COPY FROM STDIN needs copy_in(), not query()"));
                    self.flush().await?;
                }
                b'H' | b'd' | b'c' => {} // COPY TO STDOUT through query(): data dropped
                b'Z' => {
                    self.tx_status = *body.first().unwrap_or(&b'I');
                    return match failed {
                        Some(e) => Err(e),
                        None => Ok(out),
                    };
                }
                other => return Err(proto_err(format!("unexpected message {:?}", other as char))),
            }
        }
    }

    /// Run a simple-protocol statement for its effect, discarding results.
    pub async fn simple(&mut self, sql: &str) -> Result<(), OrmError> {
        self.query(sql).await.map(|_| ())
    }

    /// Prepare `sql` with parameters declared as `oids` (0: the server
    /// infers), or take it from the statement cache. `cache` false uses the
    /// unnamed statement (one-off statements, DDL).
    pub async fn prepare_typed(&mut self, sql: &str, oids: &[u32], cache: bool) -> Result<Rc<Statement>, OrmError> {
        let cache = cache && self.cache_cap > 0;
        let key = cache_key(sql, oids);
        if cache {
            if let Some(s) = self.stmts.get(&key) {
                return Ok(s);
            }
        }
        let name = if cache {
            if let Some(old) = self.stmts.evict_for_insert() {
                self.pending_close.push(old.name.clone());
            }
            let n = format!("kw{}", self.next_name);
            self.next_name += 1;
            n
        } else {
            String::new()
        };
        let stmt = Rc::new(self.parse_describe(name, sql, oids).await?);
        if cache {
            self.stmts.insert(&key, stmt.clone());
        }
        Ok(stmt)
    }

    /// `Parse` + `Describe` `sql` as statement `name` (not cached).
    pub(crate) async fn parse_describe(&mut self, name: String, sql: &str, oids: &[u32]) -> Result<Statement, OrmError> {
        self.write_pending_closes();
        let w = self.io.write_buf_append();
        msg(w, b'P', |w| {
            cstr(w, &name);
            cstr(w, sql);
            w.extend_from_slice(&(oids.len() as i16).to_be_bytes());
            for o in oids {
                w.extend_from_slice(&o.to_be_bytes());
            }
        });
        msg(w, b'D', |w| {
            w.push(b'S');
            cstr(w, &name);
        });
        msg(w, b'S', |_| {});
        self.flush().await?;
        let mut params = Vec::new();
        let mut columns = Vec::new();
        let mut failed = None;
        loop {
            let (tag, body) = self.recv_raw().await?;
            match tag {
                b'1' | b'3' | b'n' => {}
                b't' => {
                    let mut r = Reader(body);
                    let n = r.i16()?;
                    params = (0..n).map(|_| r.i32().map(|o| o as u32)).collect::<Result<_, _>>()?;
                }
                b'T' => columns = row_description(body)?,
                b'E' => {
                    let body = body.to_vec();
                    failed = Some(self.error(&body));
                }
                b'Z' => {
                    self.tx_status = *body.first().unwrap_or(&b'I');
                    break;
                }
                other => return Err(proto_err(format!("unexpected message {:?} preparing", other as char))),
            }
        }
        if let Some(e) = failed {
            return Err(e);
        }
        for c in &mut columns {
            c.format = types::result_format(c.type_oid);
        }
        Ok(Statement { name, params, columns })
    }

    /// Prepare `sql` for `params` (declared by their kinds; see
    /// [`types::param_oid`]), cached.
    pub async fn prepare(&mut self, sql: &str, params: &[Param]) -> Result<Rc<Statement>, OrmError> {
        let oids: Vec<u32> = params.iter().map(types::param_oid).collect();
        self.prepare_typed(sql, &oids, true).await
    }

    /// Drop every cached statement (after schema changes the server rejects
    /// with "cached plan must not change result type").
    pub fn clear_statement_cache(&mut self) {
        for s in self.stmts.drain() {
            self.pending_close.push(s.name.clone());
        }
    }

    fn write_bind(&mut self, portal: &str, stmt: &Statement, params: &[Param]) -> Result<(), OrmError> {
        if params.len() != stmt.params.len() {
            return Err(OrmError::Query(format!(
                "the statement has {} parameter(s), {} given",
                stmt.params.len(),
                params.len()
            )));
        }
        let encoded: Vec<(i16, Option<Vec<u8>>)> = params.iter().zip(&stmt.params).map(|(p, t)| types::encode_param(p, *t)).collect();
        let w = self.io.write_buf_append();
        msg(w, b'B', |w| {
            cstr(w, portal);
            cstr(w, &stmt.name);
            w.extend_from_slice(&(encoded.len() as i16).to_be_bytes());
            for (f, _) in &encoded {
                w.extend_from_slice(&f.to_be_bytes());
            }
            w.extend_from_slice(&(encoded.len() as i16).to_be_bytes());
            for (_, v) in &encoded {
                match v {
                    None => w.extend_from_slice(&(-1i32).to_be_bytes()),
                    Some(b) => {
                        w.extend_from_slice(&(b.len() as i32).to_be_bytes());
                        w.extend_from_slice(b);
                    }
                }
            }
            w.extend_from_slice(&(stmt.columns.len() as i16).to_be_bytes());
            for c in &stmt.columns {
                w.extend_from_slice(&c.format.to_be_bytes());
            }
        });
        Ok(())
    }

    fn write_execute(&mut self, portal: &str, max_rows: i32) {
        msg(self.io.write_buf_append(), b'E', |w| {
            cstr(w, portal);
            w.extend_from_slice(&max_rows.to_be_bytes());
        });
    }

    fn write_sync(&mut self) {
        msg(self.io.write_buf_append(), b'S', |_| {});
    }

    /// Read the replies to one `Bind` / `Execute` (and, with `sync`, up to
    /// `ReadyForQuery`), handing each row's cells to `row`.
    async fn read_execution<F>(&mut self, cols: &[Column], sync: bool, row: &mut F) -> Result<Executed, OrmError>
    where
        F: FnMut(&[Column], &[Cell<'_>]) -> Result<(), OrmError>,
    {
        let mut done = Executed::default();
        let mut failed: Option<OrmError> = None;
        let mut finished = false;
        loop {
            let (tag, body) = self.recv_raw().await?;
            match tag {
                b'2' | b'3' | b'1' | b'n' => {}
                b'D' => {
                    if failed.is_some() {
                        continue;
                    }
                    let mut raw = Vec::with_capacity(cols.len());
                    data_row(body, &mut raw)?;
                    let typed: Vec<Cell<'_>> = cols.iter().zip(&raw).map(|(c, v)| types::cell(c.type_oid, c.format, *v)).collect();
                    if let Err(e) = row(cols, &typed) {
                        // Keep reading to stay in step; report the callback's error.
                        failed = Some(e);
                    }
                }
                b'C' => {
                    let tag = Reader(body).cstr()?;
                    done.affected = tag_count(&tag);
                    done.tag = tag;
                    finished = true;
                }
                b's' => {
                    done.suspended = true;
                    finished = true;
                }
                b'I' => finished = true,
                b'E' => {
                    let body = body.to_vec();
                    let e = self.error(&body);
                    if !sync {
                        return Err(e);
                    }
                    failed = Some(e);
                    finished = true;
                }
                b'Z' => {
                    self.tx_status = *body.first().unwrap_or(&b'I');
                    break;
                }
                b'G' => {
                    msg(self.io.write_buf(), b'f', |w| cstr(w, "COPY FROM STDIN needs copy_in()"));
                    self.flush().await?;
                }
                b'H' | b'd' | b'c' => {}
                other => return Err(proto_err(format!("unexpected message {:?}", other as char))),
            }
            if finished && !sync {
                break;
            }
        }
        match failed {
            Some(e) => Err(e),
            None => Ok(done),
        }
    }

    /// Execute a prepared statement to completion, handing each row to `row`.
    pub async fn execute_prepared<F>(&mut self, stmt: &Statement, params: &[Param], mut row: F) -> Result<Executed, OrmError>
    where
        F: FnMut(&[Column], &[Cell<'_>]) -> Result<(), OrmError>,
    {
        self.write_pending_closes();
        self.write_bind("", stmt, params)?;
        self.write_execute("", 0);
        self.write_sync();
        self.flush().await?;
        self.read_execution(&stmt.columns, true, &mut row).await
    }

    /// Prepare (cached) and run `sql` with `params`, handing each row to
    /// `row`. A cached statement the server no longer accepts after a schema
    /// change is re-prepared once (outside a transaction).
    pub async fn run<F>(&mut self, sql: &str, params: &[Param], mut row: F) -> Result<(Rc<Statement>, Executed), OrmError>
    where
        F: FnMut(&[Column], &[Cell<'_>]) -> Result<(), OrmError>,
    {
        let cache = !params.is_empty() || starts_with_word(sql, &["SELECT", "WITH", "VALUES", "TABLE"]);
        let stmt = if cache { self.prepare(sql, params).await? } else { self.prepare_typed(sql, &[], false).await? };
        match self.execute_prepared(&stmt, params, &mut row).await {
            Err(e) if cache && !self.in_transaction() && self.last_error.as_ref().is_some_and(is_stale_plan) => {
                let _ = e;
                self.forget(sql, params);
                let stmt = self.prepare(sql, params).await?;
                let done = self.execute_prepared(&stmt, params, &mut row).await?;
                Ok((stmt, done))
            }
            Err(e) => {
                if cache && self.last_error.as_ref().is_some_and(is_stale_plan) {
                    self.forget(sql, params);
                }
                Err(e)
            }
            Ok(done) => Ok((stmt, done)),
        }
    }

    fn forget(&mut self, sql: &str, params: &[Param]) {
        let oids: Vec<u32> = params.iter().map(types::param_oid).collect();
        if let Some(s) = self.stmts.remove(&cache_key(sql, &oids)) {
            self.pending_close.push(s.name.clone());
        }
    }

    /// Run `sql` with `params` and collect the rows as typed values.
    pub async fn query_params(&mut self, sql: &str, params: &[Param]) -> Result<(Vec<Column>, Vec<Vec<Value>>, Executed), OrmError> {
        let mut rows = Vec::new();
        let (stmt, done) = self
            .run(sql, params, |cols, cells| {
                rows.push(cols.iter().zip(cells).map(|(c, cell)| cell_value(c, cell)).collect());
                Ok(())
            })
            .await?;
        Ok((stmt.columns.clone(), rows, done))
    }

    /// Run a statement once per parameter row, pipelined: the rows go out in
    /// flights without waiting for each reply, and all of them belong to one
    /// `Sync`. Outside a transaction they commit or roll back together; the
    /// error names the failing row. Returns rows affected, or (index, error).
    pub async fn execute_pipelined(&mut self, stmt: &Statement, rows: &[Vec<Param>]) -> Result<u64, (usize, OrmError)> {
        let mut total = 0;
        let mut done = 0;
        let cols = stmt.columns.clone();
        while done < rows.len() {
            let end = (done + BATCH_FLIGHT).min(rows.len());
            if done == 0 {
                self.write_pending_closes();
            }
            for r in &rows[done..end] {
                self.write_bind("", stmt, r).map_err(|e| (done, e))?;
                self.write_execute("", 0);
            }
            let last = end == rows.len();
            if last {
                self.write_sync();
            } else {
                msg(self.io.write_buf_append(), b'H', |_| {}); // Flush
            }
            self.flush().await.map_err(|e| (done, e))?;
            for i in done..end {
                let mut ignore = |_: &[Column], _: &[Cell<'_>]| Ok(());
                match self.read_execution(&cols, false, &mut ignore).await {
                    Ok(x) => total += x.affected,
                    Err(e) => {
                        // The server skips everything up to Sync.
                        if !matches!(e, OrmError::Connection(_)) {
                            if !last {
                                self.write_sync();
                                self.flush().await.map_err(|e| (i, e))?;
                            }
                            self.drain_to_ready().await.map_err(|e| (i, e))?;
                        }
                        return Err((i, e));
                    }
                }
            }
            if last {
                self.drain_to_ready().await.map_err(|e| (rows.len(), e))?;
            }
            done = end;
        }
        Ok(total)
    }

    /// Bind `stmt` with `params` to the named portal `portal` (inside a
    /// transaction block, it lives until the transaction ends or it is closed).
    pub(crate) async fn bind_portal(&mut self, portal: &str, stmt: &Statement, params: &[Param]) -> Result<(), OrmError> {
        self.write_pending_closes();
        self.write_bind(portal, stmt, params)?;
        self.write_sync();
        self.flush().await?;
        let mut ignore = |_: &[Column], _: &[Cell<'_>]| Ok(());
        self.read_execution(&stmt.columns, true, &mut ignore).await.map(|_| ())
    }

    /// Fetch up to `max_rows` rows from a portal; `suspended` says more remain.
    pub(crate) async fn fetch_portal<F>(&mut self, portal: &str, cols: &[Column], max_rows: usize, mut row: F) -> Result<Executed, OrmError>
    where
        F: FnMut(&[Column], &[Cell<'_>]) -> Result<(), OrmError>,
    {
        self.write_execute(portal, i32::try_from(max_rows.max(1)).unwrap_or(i32::MAX));
        self.write_sync();
        self.flush().await?;
        self.read_execution(cols, true, &mut row).await
    }

    /// Close a portal and / or a statement (`kind` `P` / `S`).
    pub(crate) async fn close(&mut self, kind: u8, name: &str) -> Result<(), OrmError> {
        msg(self.io.write_buf_append(), b'C', |w| {
            w.push(kind);
            cstr(w, name);
        });
        self.write_sync();
        self.flush().await?;
        let mut ignore = |_: &[Column], _: &[Cell<'_>]| Ok(());
        self.read_execution(&[], true, &mut ignore).await.map(|_| ())
    }

    /// `COPY … FROM STDIN`: send `data` (in the format the statement names —
    /// text, CSV or binary) and return the rows copied.
    pub async fn copy_in(&mut self, sql: &str, data: &[u8]) -> Result<u64, OrmError> {
        self.copy_in_chunks(sql, std::iter::once(data)).await
    }

    /// `COPY … FROM STDIN` from a sequence of chunks (generated or read
    /// piece by piece), each sent as it comes.
    pub async fn copy_in_chunks<I, B>(&mut self, sql: &str, chunks: I) -> Result<u64, OrmError>
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        self.write_pending_closes();
        msg(self.io.write_buf_append(), b'Q', |w| cstr(w, sql));
        self.flush().await?;
        let mut failed = None;
        let mut copied = 0;
        let mut chunks = Some(chunks.into_iter());
        loop {
            let (tag, body) = self.recv_raw().await?;
            match tag {
                b'G' => {
                    for chunk in chunks.take().into_iter().flatten() {
                        for piece in chunk.as_ref().chunks(COPY_CHUNK) {
                            msg(self.io.write_buf(), b'd', |w| w.extend_from_slice(piece));
                            self.flush().await?;
                        }
                    }
                    msg(self.io.write_buf(), b'c', |_| {});
                    self.flush().await?;
                }
                b'C' => copied = tag_count(&Reader(body).cstr()?),
                b'E' => {
                    let body = body.to_vec();
                    failed = Some(self.error(&body));
                }
                b'T' | b'D' | b'I' | b'3' | b'H' | b'd' | b'c' => {}
                b'Z' => {
                    self.tx_status = *body.first().unwrap_or(&b'I');
                    return match failed {
                        Some(e) => Err(e),
                        None if chunks.is_some() => Err(OrmError::Query("copy_in: the statement is not COPY … FROM STDIN".into())),
                        None => Ok(copied),
                    };
                }
                other => return Err(proto_err(format!("unexpected message {:?} in COPY", other as char))),
            }
        }
    }

    /// `COPY … TO STDOUT`: hand each data chunk (a row in text / CSV
    /// format) to `each`; returns the rows copied.
    pub async fn copy_out<F>(&mut self, sql: &str, mut each: F) -> Result<u64, OrmError>
    where
        F: FnMut(&[u8]) -> Result<(), OrmError>,
    {
        self.write_pending_closes();
        msg(self.io.write_buf_append(), b'Q', |w| cstr(w, sql));
        self.flush().await?;
        let mut failed = None;
        let mut copied = 0;
        let mut was_copy = false;
        loop {
            let (tag, body) = self.recv_raw().await?;
            match tag {
                b'H' => was_copy = true,
                b'd' => {
                    if failed.is_none() {
                        if let Err(e) = each(body) {
                            failed = Some(e);
                        }
                    }
                }
                b'c' | b'T' | b'D' | b'I' | b'3' => {}
                b'C' => copied = tag_count(&Reader(body).cstr()?),
                b'G' => {
                    msg(self.io.write_buf(), b'f', |w| cstr(w, "copy_out: not a COPY TO STDOUT"));
                    self.flush().await?;
                }
                b'E' => {
                    let body = body.to_vec();
                    failed = Some(self.error(&body));
                }
                b'Z' => {
                    self.tx_status = *body.first().unwrap_or(&b'I');
                    return match failed {
                        Some(e) => Err(e),
                        None if !was_copy => Err(OrmError::Query("copy_out: the statement is not COPY … TO STDOUT".into())),
                        None => Ok(copied),
                    };
                }
                other => return Err(proto_err(format!("unexpected message {:?} in COPY", other as char))),
            }
        }
    }

    /// Say goodbye (`Terminate`) and close the socket.
    pub async fn close_connection(mut self) {
        msg(self.io.write_buf(), b'X', |_| {});
        let _ = self.io.flush().await;
    }
}

fn host_label(h: &Host) -> String {
    match h {
        Host::Tcp(n, p) => format!("{n}:{p}"),
        Host::Unix(d, p) => format!("{}/.s.PGSQL.{p}", d.display()),
    }
}

fn cache_key(sql: &str, oids: &[u32]) -> String {
    let mut k = String::with_capacity(sql.len() + oids.len() * 4 + 1);
    k.push_str(sql);
    k.push('\0');
    for o in oids {
        k.push_str(&o.to_string());
        k.push(',');
    }
    k
}

fn is_stale_plan(e: &PgError) -> bool {
    // feature_not_supported "cached plan must not change result type", or a
    // prepared statement that no longer exists.
    (e.code == "0A000" && e.message.contains("cached plan")) || e.code == "26000"
}

pub(crate) fn starts_with_word(sql: &str, words: &[&str]) -> bool {
    let t = sql.trim_start_matches(|c: char| c.is_whitespace() || c == '(');
    words.iter().any(|w| t.get(..w.len()).is_some_and(|p| p.eq_ignore_ascii_case(w)) && t[w.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric() && c != '_'))
}

/// A cell as a typed [`Value`] (for its column's type).
pub fn cell_value(c: &Column, cell: &Cell<'_>) -> Value {
    match cell {
        Cell::Null => Value::Null,
        Cell::Bool(b) => Value::Bool(*b),
        Cell::Int(i) => Value::Int(*i),
        Cell::UInt(u) => Value::UInt(*u),
        Cell::F64(f) => Value::Float(*f),
        Cell::F32(f) => Value::Float(f64::from(*f)),
        Cell::Bytes(b) if c.format == 1 => Value::Bytes(b.to_vec()),
        Cell::Bytes(b) => types::text_value(c.type_oid, &String::from_utf8_lossy(b)),
        Cell::Owned(s) => types::text_value(c.type_oid, s),
    }
}

/// The TLS client config `sslmode` asks for (`None`: no TLS).
async fn tls_config(cfg: &PgConfig) -> Result<Option<Arc<ClientConfig>>, OrmError> {
    if cfg.ssl_mode == SslMode::Disable {
        return Ok(None);
    }
    let read = |p: &std::path::Path, what: &'static str| {
        let p = p.to_path_buf();
        async move { kernway_wire::offload::read_file(&p).await.map_err(|e| conn_err(format!("read {what} {}: {e}", p.display()))) }
    };
    let ca = match &cfg.ssl_root_cert {
        Some(p) => Some(read(p, "sslrootcert").await?),
        None => None,
    };
    let check = match (cfg.ssl_mode, &ca) {
        (SslMode::VerifyFull, _) => CertCheck::Full,
        (SslMode::VerifyCa, _) | (SslMode::Require, Some(_)) => CertCheck::CaOnly,
        _ => CertCheck::None,
    };
    let identity = match (&cfg.ssl_cert, &cfg.ssl_key) {
        (Some(c), Some(k)) => Some((read(c, "sslcert").await?, read(k, "sslkey").await?)),
        _ => None,
    };
    let id = identity.as_ref().map(|(c, k)| ClientIdentity { cert_pem: c, key_pem: k });
    kernway_tls::client_config_with_identity(ca.as_deref(), check, id)
        .map(Some)
        .map_err(|e| conn_err(format!("TLS configuration: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_count_rows() {
        assert_eq!(tag_count("INSERT 0 5"), 5);
        assert_eq!(tag_count("UPDATE 3"), 3);
        assert_eq!(tag_count("SELECT 12"), 12);
        assert_eq!(tag_count("CREATE TABLE"), 0);
        assert_eq!(tag_count("COPY 9"), 9);
    }

    #[test]
    fn errors_map() {
        let mut body = Vec::new();
        for (c, v) in [(b'S', "ERROR"), (b'V', "ERROR"), (b'C', "23505"), (b'M', "duplicate key"), (b'D', "Key (email)=(a@x) already exists."), (b'n', "m_email_key")] {
            body.push(c);
            body.extend_from_slice(v.as_bytes());
            body.push(0);
        }
        body.push(0);
        let e = PgError::parse(&body);
        assert_eq!(e.code, "23505");
        assert!(matches!(e.to_orm(), OrmError::UniqueViolation { field } if field == "email"));
        let e = PgError { severity: "FATAL".into(), code: "57P01".into(), message: "terminating".into(), ..Default::default() };
        assert!(matches!(e.to_orm(), OrmError::Connection(_)));
        let e = PgError { severity: "ERROR".into(), code: "42601".into(), message: "syntax".into(), position: Some(8), ..Default::default() };
        assert!(matches!(e.to_orm(), OrmError::Query(m) if m.contains("42601") && m.contains("character 8")));
    }

    #[test]
    fn words() {
        assert!(starts_with_word("  select 1", &["SELECT"]));
        assert!(starts_with_word("(SELECT 1)", &["SELECT"]));
        assert!(!starts_with_word("selection", &["SELECT"]));
        assert!(starts_with_word("WITH x AS (…)", &["SELECT", "WITH"]));
    }
}
