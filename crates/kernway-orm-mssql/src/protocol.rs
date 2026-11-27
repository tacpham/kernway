//! The TDS protocol (MS-TDS, version 7.4 and 8.0), client side, on the
//! Kernway reactor.
//!
//! A message is one or more packets: an 8-byte header (type, status — `EOM`
//! on the last —, big-endian length, SPID, packet id, window) and a payload.
//! A connection:
//!
//! 1. sends `PRELOGIN` (version, encryption), and runs TLS as the server
//!    agreed: inside `PRELOGIN` packets (TDS 7.4), for the login only, not
//!    at all — or, with TDS 8.0 `strict`, before any TDS byte;
//! 2. logs in (`LOGIN7`, the password obfuscated as the protocol asks, under
//!    TLS whenever there is TLS), reading `LOGINACK` and environment changes;
//! 3. runs SQL batches and RPCs (`sp_executesql` with typed parameters,
//!    stored procedures with `OUTPUT` parameters, API cursors), reading the
//!    token stream (column metadata, rows, `DONE`s, return values, errors)
//!    packet by packet — a large result streams in constant memory;
//! 4. cancels a statement that outlives `command_timeout` with an
//!    `ATTENTION` packet and reads up to the server's acknowledgement, so
//!    the connection stays usable;
//! 5. tracks the transaction descriptor the server hands out (`ENVCHANGE`)
//!    and sends it with every request.

use crate::config::{Encrypt, MssqlConfig};
use crate::types::{self, Encoded, LenKind, TypeInfo};
use kernway_orm_core::error::OrmError;
use kernway_orm_sql::Value;
use kernway_tls::rustls::{pki_types::ServerName, ClientConfig, ClientConnection};
use kernway_wire::BufStream;
use std::sync::Arc;
use std::time::Instant;

const PKT_SQL_BATCH: u8 = 0x01;
const PKT_RPC: u8 = 0x03;
const PKT_ATTENTION: u8 = 0x06;
const PKT_LOGIN7: u8 = 0x10;
const PKT_PRELOGIN: u8 = 0x12;
const STATUS_EOM: u8 = 0x01;

const DONE_MORE: u16 = 0x01;
const DONE_ERROR: u16 = 0x02;
const DONE_COUNT: u16 = 0x10;
const DONE_ATTN: u16 = 0x20;

const TDS_7_4: u32 = 0x7400_0004;

pub(crate) fn conn_err(m: impl Into<String>) -> OrmError {
    OrmError::Connection(m.into())
}

fn io_err(e: std::io::Error) -> OrmError {
    conn_err(format!("SQL Server connection: {e}"))
}

fn proto(m: impl std::fmt::Display) -> OrmError {
    conn_err(format!("TDS protocol: {m}"))
}

/// A server message (`ERROR` / `INFO` token).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerMessage {
    /// Error number (`2627`: unique key, `547`: constraint, `1205`: deadlock …).
    pub number: i32,
    /// State.
    pub state: u8,
    /// Severity class (≤ 10 informational, 11–16 user errors, ≥ 20 fatal).
    pub class: u8,
    /// The text.
    pub message: String,
    /// Server name.
    pub server: String,
    /// Procedure, when raised in one.
    pub procedure: String,
    /// Line in the batch / procedure.
    pub line: i32,
}

impl ServerMessage {
    /// As an [`OrmError`]: unique-key and foreign-key violations typed,
    /// deadlocks / timeouts as transaction errors, fatal ones as connection errors.
    pub fn to_orm(&self) -> OrmError {
        let text = format!("SQL Server error {} (state {}, class {}): {}", self.number, self.state, self.class, self.message);
        match self.number {
            2627 | 2601 => OrmError::UniqueViolation { field: unique_name(&self.message) },
            547 => OrmError::ForeignKeyViolation,
            1205 | 3960 => OrmError::Transaction(text),
            _ if self.class >= 20 => OrmError::Connection(text),
            _ => OrmError::Query(text),
        }
    }
}

/// The constraint / index a duplicate-key message names (`'UQ__…'`).
fn unique_name(msg: &str) -> String {
    msg.split('\'').nth(1).unwrap_or("").to_string()
}

/// A result column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Name (empty for an unnamed expression).
    pub name: String,
    /// Type.
    pub ti: TypeInfo,
    /// Nullable.
    pub nullable: bool,
    /// A hidden column (an API cursor's `ROWSTAT`, browse-mode keys).
    pub hidden: bool,
}

/// What a request's response told, besides rows.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Response {
    /// Row counts of the statements that reported one, in order.
    pub counts: Vec<u64>,
    /// Column sets of the result sets, in order.
    pub results: Vec<Vec<Column>>,
    /// `OUTPUT` parameter values: (ordinal, name, value).
    pub return_values: Vec<(u16, String, Value)>,
    /// A procedure's `RETURN` status.
    pub return_status: Option<i32>,
}

impl Response {
    /// The total of the row counts.
    pub fn affected(&self) -> u64 {
        self.counts.iter().sum()
    }
}

/// One RPC parameter.
pub(crate) struct RpcParam {
    pub name: String,
    pub output: bool,
    pub enc: Encoded,
}

/// The procedure an RPC runs.
pub(crate) enum Proc<'a> {
    /// A well-known system procedure by id (`sp_executesql` = 10, …).
    Id(u16),
    /// A procedure by name.
    Name(&'a str),
}

pub(crate) const SP_CURSOROPEN: u16 = 2;
pub(crate) const SP_CURSORFETCH: u16 = 7;
pub(crate) const SP_CURSORCLOSE: u16 = 9;
pub(crate) const SP_EXECUTESQL: u16 = 10;

/// An open, logged-in connection.
pub struct Conn {
    io: BufStream,
    packet_size: usize,
    packet_id: u8,
    // The response being read: payload bytes not yet consumed.
    rbuf: Vec<u8>,
    rpos: usize,
    eom: bool,
    txn: u64,
    pub(crate) collation: [u8; 5],
    database: String,
    server_version: (u8, u8, u16),
    messages: Vec<ServerMessage>,
    last_error: Option<ServerMessage>,
    broken: bool,
    command_timeout: Option<std::time::Duration>,
    deadline: Option<Instant>,
    attention_sent: bool,
    pub(crate) describe_cache: kernway_wire::StmtCache<std::rc::Rc<Vec<types::Target>>>,
    pub(crate) cursors: std::collections::HashMap<u64, i32>,
    pub(crate) next_cursor: u64,
    /// Procedures completed in the response being read (pipelined RPCs),
    /// and the one that raised the first error.
    pub(crate) procs_done: usize,
    pub(crate) error_proc: Option<usize>,
}

fn b_varchar(out: &mut Vec<u8>, s: &str) {
    let u = types::to_utf16(s);
    out.push((u.len() / 2) as u8);
    out.extend_from_slice(&u);
}

impl Conn {
    /// Connect and log in per `cfg`. Host names resolve on the blocking
    /// pool (every address in turn), the CA file is read there too.
    pub async fn connect(cfg: &MssqlConfig) -> Result<Self, OrmError> {
        let tls = tls_config(cfg).await?;
        match rt_core::timeout(cfg.connect_timeout, Self::open(cfg, tls)).await {
            Ok(r) => r,
            Err(_) => Err(conn_err(format!("SQL Server {}:{}: connect / login timed out after {:?}", cfg.host, cfg.port, cfg.connect_timeout))),
        }
    }

    async fn open(cfg: &MssqlConfig, tls: Option<Arc<ClientConfig>>) -> Result<Self, OrmError> {
        let tcp = kernway_wire::offload::connect_tcp(&cfg.host, cfg.port)
            .await
            .map_err(|e| conn_err(format!("connect {}:{} failed: {e}", cfg.host, cfg.port)))?;
        let mut c = Conn {
            io: BufStream::new(tcp),
            packet_size: 4096,
            packet_id: 1,
            rbuf: Vec::new(),
            rpos: 0,
            eom: true,
            txn: 0,
            collation: [0x09, 0x04, 0xD0, 0x00, 0x34],
            database: String::new(),
            server_version: (0, 0, 0),
            messages: Vec::new(),
            last_error: None,
            broken: false,
            command_timeout: cfg.command_timeout,
            deadline: None,
            attention_sent: false,
            describe_cache: kernway_wire::StmtCache::new(256),
            cursors: std::collections::HashMap::new(),
            next_cursor: 1,
            procs_done: 0,
            error_proc: None,
        };
        let cert_name = cfg.host_name_in_certificate.clone().unwrap_or_else(|| cfg.host.clone());
        if cfg.encrypt == Encrypt::Strict {
            let tls = tls.clone().ok_or_else(|| conn_err("encrypt=strict needs TLS"))?;
            let mut strict = (*tls).clone();
            strict.alpn_protocols = vec![b"tds/8.0".to_vec()];
            c.io.upgrade_tls(Arc::new(strict), &cert_name).await.map_err(|e| conn_err(format!("TLS (strict) with {}: {e}", cfg.host)))?;
        }
        let server_enc = c.prelogin(cfg.encrypt).await?;
        // ENCRYPT_OFF 0, ON 1, NOT_SUP 2, REQ 3.
        let login_tls = match (cfg.encrypt, server_enc) {
            (Encrypt::Strict, _) => false,
            (Encrypt::NotSupported, 2) => false,
            (Encrypt::NotSupported, _) => return Err(conn_err("the server requires encryption; use encrypt=false (login only) or true")),
            (Encrypt::On, 2) => return Err(conn_err("encrypt=true, but the server does not support encryption")),
            (Encrypt::LoginOnly, 2) => false,
            _ => true,
        };
        let full = cfg.encrypt == Encrypt::On || (cfg.encrypt == Encrypt::LoginOnly && server_enc == 3) || (cfg.encrypt == Encrypt::LoginOnly && server_enc == 1);
        if login_tls {
            let tls = tls.ok_or_else(|| conn_err("TLS is needed, but no TLS configuration"))?;
            c.tls_in_prelogin(tls, &cert_name).await?;
        }
        c.login(cfg).await?;
        if login_tls && !full {
            // Login-only encryption: the rest of the session is plaintext.
            c.io.drop_tls().map_err(io_err)?;
            c.read_login_response().await?;
        } else {
            c.read_login_response().await?;
        }
        // Session settings the driver relies on (as SSMS / ODBC set them),
        // and no truncation of large text.
        c.batch_discard(
            "SET ANSI_NULLS ON; SET ANSI_PADDING ON; SET ANSI_WARNINGS ON; SET ARITHABORT ON; SET CONCAT_NULL_YIELDS_NULL ON; \
             SET QUOTED_IDENTIFIER ON; SET XACT_ABORT OFF; SET DATEFORMAT ymd; SET TEXTSIZE 2147483647",
        )
        .await?;
        Ok(c)
    }

    // ---- packets -------------------------------------------------------

    async fn send(&mut self, ty: u8, payload: &[u8]) -> Result<(), OrmError> {
        if self.broken {
            return Err(conn_err("SQL Server connection is broken (an earlier error)"));
        }
        let max = self.packet_size - 8;
        // Over TLS each packet goes out on its own (its own TLS record):
        // SQL Server does not accept a packet split across records.
        let per_packet = self.io.is_tls();
        let mut chunks = payload.chunks(max).peekable();
        self.io.write_buf();
        if payload.is_empty() {
            self.io.write_buf_append().extend_from_slice(&[ty, STATUS_EOM, 0, 8, 0, 0, 1, 0]);
        }
        while let Some(chunk) = chunks.next() {
            let last = chunks.peek().is_none();
            let len = (chunk.len() + 8) as u16;
            let w = self.io.write_buf_append();
            w.extend_from_slice(&[ty, if last { STATUS_EOM } else { 0 }]);
            w.extend_from_slice(&len.to_be_bytes());
            w.extend_from_slice(&[0, 0, self.packet_id, 0]);
            w.extend_from_slice(chunk);
            self.packet_id = self.packet_id.wrapping_add(1);
            if per_packet && !last {
                let r = self.io.flush().await;
                self.io.write_buf();
                r.map_err(|e| self.fail(e))?;
            }
        }
        let r = self.io.flush().await;
        self.io.write_buf();
        r.map_err(|e| self.fail(e))
    }

    /// Start reading a response message.
    fn begin_response(&mut self) {
        self.rbuf.clear();
        self.rpos = 0;
        self.eom = false;
        self.procs_done = 0;
        self.error_proc = None;
    }

    /// Read the next packet of the response into the buffer.
    async fn next_packet(&mut self) -> Result<(), OrmError> {
        if self.eom {
            if !self.attention_sent {
                return Err(proto("the response ended in the middle of a token"));
            }
            // A cancelled request: its response ended; the acknowledgement
            // (DONE with ATTN) comes as a message of its own.
            self.eom = false;
        }
        // A statement past its timeout is cancelled; reading goes on until
        // the server acknowledges.
        if let (Some(d), false) = (self.deadline, self.attention_sent) {
            // Checked before every packet, not only while waiting: a result
            // that streams without pause must be cut off too.
            let left = d.saturating_duration_since(Instant::now());
            let ready = if left.is_zero() {
                false
            } else {
                match rt_core::timeout(left, self.io.fill(8)).await {
                    Ok(r) => {
                        r.map_err(|e| self.fail(e))?;
                        true
                    }
                    Err(_) => false,
                }
            };
            if !ready {
                self.attention_sent = true;
                let w = self.io.write_buf();
                w.extend_from_slice(&[PKT_ATTENTION, STATUS_EOM, 0, 8, 0, 0, 1, 0]);
                let r = self.io.flush().await;
                self.io.write_buf();
                r.map_err(|e| self.fail(e))?;
                self.io.fill(8).await.map_err(|e| self.fail(e))?;
            }
        } else {
            self.io.fill(8).await.map_err(|e| self.fail(e))?;
        }
        let h = self.io.peek(8);
        let status = h[1];
        let len = u16::from_be_bytes([h[2], h[3]]) as usize;
        if len < 8 {
            self.broken = true;
            return Err(proto(format!("packet length {len}")));
        }
        self.io.fill(len).await.map_err(|e| self.fail(e))?;
        if self.rpos > 0 && self.rpos == self.rbuf.len() {
            self.rbuf.clear();
            self.rpos = 0;
        } else if self.rpos > 65_536 {
            self.rbuf.drain(..self.rpos);
            self.rpos = 0;
        }
        let packet = self.io.take(len);
        self.rbuf.extend_from_slice(&packet[8..]);
        self.eom = status & STATUS_EOM != 0;
        Ok(())
    }

    fn fail(&mut self, e: std::io::Error) -> OrmError {
        self.broken = true;
        io_err(e)
    }

    async fn need(&mut self, n: usize) -> Result<(), OrmError> {
        while self.rbuf.len() - self.rpos < n {
            self.next_packet().await?;
        }
        Ok(())
    }

    async fn bytes(&mut self, n: usize) -> Result<&[u8], OrmError> {
        self.need(n).await?;
        let s = &self.rbuf[self.rpos..self.rpos + n];
        self.rpos += n;
        Ok(s)
    }

    async fn u8(&mut self) -> Result<u8, OrmError> {
        Ok(self.bytes(1).await?[0])
    }
    async fn u16(&mut self) -> Result<u16, OrmError> {
        let b = self.bytes(2).await?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    async fn u32(&mut self) -> Result<u32, OrmError> {
        let b = self.bytes(4).await?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    async fn u64(&mut self) -> Result<u64, OrmError> {
        let b = self.bytes(8).await?;
        Ok(u64::from_le_bytes(b.try_into().unwrap_or([0; 8])))
    }
    /// `B_VARCHAR`: a 1-byte character count, UTF-16.
    async fn b_varchar(&mut self) -> Result<String, OrmError> {
        let n = self.u8().await? as usize;
        Ok(types::utf16(self.bytes(n * 2).await?))
    }
    /// `US_VARCHAR`: a 2-byte character count, UTF-16.
    async fn us_varchar(&mut self) -> Result<String, OrmError> {
        let n = self.u16().await? as usize;
        Ok(types::utf16(self.bytes(n * 2).await?))
    }

    /// Read a whole message (PRELOGIN replies, TLS handshake records).
    async fn read_message(&mut self) -> Result<Vec<u8>, OrmError> {
        self.begin_response();
        loop {
            self.next_packet().await?;
            if self.eom {
                let v = self.rbuf[self.rpos..].to_vec();
                self.rbuf.clear();
                self.rpos = 0;
                return Ok(v);
            }
        }
    }

    // ---- connection set-up --------------------------------------------------

    async fn prelogin(&mut self, encrypt: Encrypt) -> Result<u8, OrmError> {
        let enc: u8 = match encrypt {
            Encrypt::LoginOnly => 0,
            Encrypt::On => 1,
            Encrypt::NotSupported => 2,
            // TDS 8.0: the channel is already encrypted.
            Encrypt::Strict => 0,
        };
        // Options: VERSION, ENCRYPTION, INSTOPT, THREADID, MARS, terminator.
        let opts: [(u8, Vec<u8>); 5] = [
            (0x00, vec![0x10, 0x00, 0x00, 0x00, 0x00, 0x00]),
            (0x01, vec![enc]),
            (0x02, vec![0]),
            (0x03, std::process::id().to_be_bytes().to_vec()),
            (0x04, vec![0]),
        ];
        let header_len = opts.len() * 5 + 1;
        let mut head = Vec::new();
        let mut data = Vec::new();
        for (t, d) in &opts {
            head.push(*t);
            head.extend_from_slice(&((header_len + data.len()) as u16).to_be_bytes());
            head.extend_from_slice(&(d.len() as u16).to_be_bytes());
            data.extend_from_slice(d);
        }
        head.push(0xFF);
        head.extend_from_slice(&data);
        self.send(PKT_PRELOGIN, &head).await?;
        let reply = self.read_message().await?;
        let mut i = 0;
        let mut server_enc = 2;
        while i < reply.len() && reply[i] != 0xFF {
            let t = reply[i];
            let off = u16::from_be_bytes([*reply.get(i + 1).unwrap_or(&0), *reply.get(i + 2).unwrap_or(&0)]) as usize;
            let len = u16::from_be_bytes([*reply.get(i + 3).unwrap_or(&0), *reply.get(i + 4).unwrap_or(&0)]) as usize;
            if t == 0x01 && len >= 1 {
                server_enc = *reply.get(off).ok_or_else(|| proto("PRELOGIN encryption option out of range"))?;
            }
            i += 5;
        }
        Ok(server_enc)
    }

    /// TDS 7.x: the TLS handshake, its records wrapped in PRELOGIN packets.
    async fn tls_in_prelogin(&mut self, cfg: Arc<ClientConfig>, name: &str) -> Result<(), OrmError> {
        let server = ServerName::try_from(name.to_string()).map_err(|_| conn_err(format!("{name:?} is not a valid TLS server name")))?;
        let mut tls = ClientConnection::new(cfg, server).map_err(|e| conn_err(format!("TLS: {e}")))?;
        let fail = |e: &dyn std::fmt::Display| conn_err(format!("TLS handshake with {name}: {e}"));
        loop {
            if tls.wants_write() {
                let mut out = Vec::new();
                while tls.wants_write() {
                    tls.write_tls(&mut out).map_err(|e| fail(&e))?;
                }
                self.send(PKT_PRELOGIN, &out).await?;
            }
            if !tls.is_handshaking() {
                break;
            }
            let msg = self.read_message().await?;
            let mut rd: &[u8] = &msg;
            while !rd.is_empty() {
                tls.read_tls(&mut rd).map_err(|e| fail(&e))?;
                tls.process_new_packets().map_err(|e| fail(&e))?;
            }
        }
        self.io.adopt_tls(tls).map_err(io_err)
    }

    async fn login(&mut self, cfg: &MssqlConfig) -> Result<(), OrmError> {
        let host = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "kernway".into());
        // Password: each byte of its UTF-16 swapped nibbles, XOR 0xA5.
        let pw: Vec<u8> = types::to_utf16(&cfg.password).iter().map(|b| b.rotate_right(4) ^ 0xA5).collect();
        let fields: [Vec<u8>; 9] = [
            types::to_utf16(&host),
            types::to_utf16(&cfg.user),
            pw,
            types::to_utf16(&cfg.application_name),
            types::to_utf16(&cfg.host),
            Vec::new(), // extension
            types::to_utf16("kernway"),
            Vec::new(), // language
            types::to_utf16(cfg.database.as_deref().unwrap_or("")),
        ];
        const FIXED: usize = 94;
        let mut body = vec![0u8; FIXED];
        let mut data = Vec::new();
        let put = |b: &mut Vec<u8>, at: usize, v: &[u8]| b[at..at + v.len()].copy_from_slice(v);
        put(&mut body, 4, &TDS_7_4.to_le_bytes());
        put(&mut body, 8, &u32::from(cfg.packet_size).to_le_bytes());
        put(&mut body, 12, &0x0700_0000u32.to_le_bytes());
        put(&mut body, 16, &std::process::id().to_le_bytes());
        // OptionFlags1: USE_DB_ON | INIT_DB_FATAL | SET_LANG_ON; 2: INIT_LANG_FATAL | ODBC_ON.
        body[24] = 0xE0;
        body[25] = 0x03;
        put(&mut body, 32, &0x0409u32.to_le_bytes());
        let mut at = 36;
        for f in &fields {
            let off = (FIXED + data.len()) as u16;
            put(&mut body, at, &off.to_le_bytes());
            put(&mut body, at + 2, &((f.len() / 2) as u16).to_le_bytes());
            data.extend_from_slice(f);
            at += 4;
        }
        // ClientID (6 bytes) at 72; SSPI, AtchDBFile, ChangePassword empty; cbSSPILong 0.
        for off_at in [78usize, 82, 86] {
            put(&mut body, off_at, &((FIXED + data.len()) as u16).to_le_bytes());
        }
        body.extend_from_slice(&data);
        let len = body.len() as u32;
        put(&mut body, 0, &len.to_le_bytes());
        self.send(PKT_LOGIN7, &body).await
    }

    async fn read_login_response(&mut self) -> Result<(), OrmError> {
        self.begin_response();
        let mut acked = false;
        let mut failure: Option<ServerMessage> = None;
        loop {
            let token = self.u8().await?;
            match token {
                0xAD => {
                    let len = self.u16().await? as usize;
                    let b = self.bytes(len).await?.to_vec();
                    let n = b.get(5).copied().unwrap_or(0) as usize;
                    let v = 6 + n * 2;
                    if let Some(ver) = b.get(v..v + 4) {
                        self.server_version = (ver[0], ver[1], u16::from_be_bytes([ver[2], ver[3]]));
                    }
                    acked = true;
                }
                0xAA | 0xAB => {
                    let m = self.message_token().await?;
                    if token == 0xAA && failure.is_none() {
                        failure = Some(m);
                    } else if token == 0xAB {
                        self.messages.push(m);
                    }
                }
                0xE3 => self.envchange().await?,
                0xAE => {
                    loop {
                        let id = self.u8().await?;
                        if id == 0xFF {
                            break;
                        }
                        let n = self.u32().await? as usize;
                        self.bytes(n).await?;
                    }
                }
                0xFD..=0xFF => {
                    let status = self.u16().await?;
                    self.bytes(10).await?;
                    if status & DONE_MORE == 0 {
                        break;
                    }
                }
                other => return Err(proto(format!("unexpected token {other:#04x} in the login response"))),
            }
        }
        if let Some(f) = failure {
            return Err(conn_err(format!("SQL Server login failed: error {} (state {}): {}", f.number, f.state, f.message)));
        }
        if !acked {
            return Err(conn_err("SQL Server did not acknowledge the login"));
        }
        Ok(())
    }

    async fn message_token(&mut self) -> Result<ServerMessage, OrmError> {
        let _len = self.u16().await?;
        let number = self.u32().await? as i32;
        let state = self.u8().await?;
        let class = self.u8().await?;
        let message = self.us_varchar().await?;
        let server = self.b_varchar().await?;
        let procedure = self.b_varchar().await?;
        let line = self.u32().await? as i32;
        Ok(ServerMessage { number, state, class, message, server, procedure, line })
    }

    async fn envchange(&mut self) -> Result<(), OrmError> {
        let len = self.u16().await? as usize;
        let b = self.bytes(len).await?.to_vec();
        let ty = b.first().copied().unwrap_or(0);
        let rest = &b[1..];
        let b_varchar_at = |r: &[u8]| -> String {
            let n = r.first().copied().unwrap_or(0) as usize;
            r.get(1..1 + n * 2).map(types::utf16).unwrap_or_default()
        };
        match ty {
            1 => self.database = b_varchar_at(rest),
            4 => {
                if let Ok(n) = b_varchar_at(rest).parse::<usize>() {
                    self.packet_size = n.clamp(512, 32767);
                }
            }
            7 => {
                if rest.first().copied().unwrap_or(0) as usize >= 5 {
                    self.collation.copy_from_slice(&rest[1..6]);
                }
            }
            8 => {
                if rest.first() == Some(&8) {
                    self.txn = u64::from_le_bytes(rest[1..9].try_into().unwrap_or([0; 8]));
                }
            }
            9 | 10 | 17 => self.txn = 0,
            _ => {}
        }
        Ok(())
    }

    // ---- requests ---------------------------------------------------------------

    fn all_headers(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&22u32.to_le_bytes());
        out.extend_from_slice(&18u32.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&self.txn.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
    }

    pub(crate) async fn send_batch(&mut self, sql: &str) -> Result<(), OrmError> {
        let mut p = Vec::with_capacity(22 + sql.len() * 2);
        self.all_headers(&mut p);
        p.extend_from_slice(&types::to_utf16(sql));
        self.send(PKT_SQL_BATCH, &p).await
    }

    /// Send RPCs, several in one request (each one's results follow in order).
    pub(crate) async fn send_rpcs(&mut self, calls: &[(Proc<'_>, Vec<RpcParam>)]) -> Result<(), OrmError> {
        let mut p = Vec::with_capacity(256);
        self.all_headers(&mut p);
        for (i, (proc_, params)) in calls.iter().enumerate() {
            if i > 0 {
                p.push(0xFF); // batch separator (TDS 7.2+)
            }
            match proc_ {
                Proc::Id(id) => {
                    p.extend_from_slice(&0xFFFFu16.to_le_bytes());
                    p.extend_from_slice(&id.to_le_bytes());
                }
                Proc::Name(n) => {
                    let u = types::to_utf16(n);
                    p.extend_from_slice(&((u.len() / 2) as u16).to_le_bytes());
                    p.extend_from_slice(&u);
                }
            }
            p.extend_from_slice(&0u16.to_le_bytes());
            for prm in params {
                b_varchar(&mut p, &prm.name);
                p.push(u8::from(prm.output));
                p.extend_from_slice(&prm.enc.type_info);
                p.extend_from_slice(&prm.enc.value);
            }
        }
        self.send(PKT_RPC, &p).await
    }

    async fn type_info(&mut self) -> Result<TypeInfo, OrmError> {
        use types::ty::*;
        let t = self.u8().await?;
        let mut ti = TypeInfo { ty: t, max_len: 0, precision: 0, scale: 0, collation: None };
        match t {
            NULL | INT1 | BIT | INT2 | INT4 | DATETIM4 | FLT4 | MONEY | DATETIME | FLT8 | MONEY4 | INT8 => {}
            DATEN => {}
            TIMEN | DATETIME2N | DATETIMEOFFSETN => ti.scale = self.u8().await?,
            DECIMALN | NUMERICN => {
                ti.max_len = u32::from(self.u8().await?);
                ti.precision = self.u8().await?;
                ti.scale = self.u8().await?;
            }
            GUID | INTN | BITN | FLTN | MONEYN | DATETIMN => ti.max_len = u32::from(self.u8().await?),
            BIGVARBIN | BIGBINARY => ti.max_len = u32::from(self.u16().await?),
            BIGVARCHR | BIGCHAR | NVARCHAR | NCHAR => {
                ti.max_len = u32::from(self.u16().await?);
                let c = self.bytes(5).await?;
                ti.collation = Some([c[0], c[1], c[2], c[3], c[4]]);
            }
            TEXT | NTEXT | IMAGE => {
                ti.max_len = self.u32().await?;
                if t != IMAGE {
                    let c = self.bytes(5).await?;
                    ti.collation = Some([c[0], c[1], c[2], c[3], c[4]]);
                }
                let parts = self.u8().await?;
                for _ in 0..parts {
                    self.us_varchar().await?;
                }
            }
            XML => {
                if self.u8().await? == 1 {
                    self.b_varchar().await?;
                    self.b_varchar().await?;
                    self.us_varchar().await?;
                }
            }
            UDT => {
                ti.max_len = u32::from(self.u16().await?);
                self.b_varchar().await?;
                self.b_varchar().await?;
                self.b_varchar().await?;
                self.us_varchar().await?;
            }
            SSVARIANT => ti.max_len = self.u32().await?,
            other => return Err(proto(format!("unsupported data type {other:#04x}"))),
        }
        Ok(ti)
    }

    async fn column_metadata(&mut self) -> Result<Vec<Column>, OrmError> {
        let n = self.u16().await?;
        if n == 0xFFFF {
            return Ok(Vec::new());
        }
        let mut cols = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let _user_type = self.u32().await?;
            let flags = self.u16().await?;
            let ti = self.type_info().await?;
            let name = self.b_varchar().await?;
            cols.push(Column { name, ti, nullable: flags & 0x01 != 0, hidden: flags & 0x2000 != 0 });
        }
        Ok(cols)
    }

    /// One value's bytes (`None`: NULL).
    async fn value_bytes(&mut self, ti: &TypeInfo) -> Result<Option<Vec<u8>>, OrmError> {
        Ok(match ti.len_kind() {
            LenKind::Fixed(0) => None,
            LenKind::Fixed(n) => Some(self.bytes(n).await?.to_vec()),
            LenKind::Byte => {
                let n = self.u8().await? as usize;
                if n == 0 {
                    // A zero-length char / binary is not NULL — but those use
                    // other prefixes; for these types 0 means NULL.
                    None
                } else {
                    Some(self.bytes(n).await?.to_vec())
                }
            }
            LenKind::UShort => {
                let n = self.u16().await?;
                if n == 0xFFFF {
                    None
                } else {
                    Some(self.bytes(n as usize).await?.to_vec())
                }
            }
            LenKind::Plp => {
                let total = self.u64().await?;
                if total == u64::MAX {
                    None
                } else {
                    let mut out = Vec::with_capacity(if total == u64::MAX - 1 { 0 } else { total.min(1 << 24) as usize });
                    loop {
                        let n = self.u32().await? as usize;
                        if n == 0 {
                            break;
                        }
                        out.extend_from_slice(self.bytes(n).await?);
                    }
                    Some(out)
                }
            }
            LenKind::Long => {
                let ptr = self.u8().await? as usize;
                if ptr == 0 {
                    None
                } else {
                    self.bytes(ptr + 8).await?;
                    let n = self.u32().await? as usize;
                    Some(self.bytes(n).await?.to_vec())
                }
            }
            LenKind::Variant => {
                let n = self.u32().await? as usize;
                if n == 0 {
                    None
                } else {
                    Some(self.bytes(n).await?.to_vec())
                }
            }
        })
    }

    /// Read a response to the end, handing each row to `row` (columns,
    /// values). Errors after the first are collected; the first is returned.
    pub(crate) async fn read_response<F>(&mut self, mut row: F) -> Result<Response, OrmError>
    where
        F: FnMut(&[Column], &[Option<Vec<u8>>]) -> Result<(), OrmError>,
    {
        self.begin_response();
        let mut resp = Response::default();
        let mut cols: Vec<Column> = Vec::new();
        let mut failed: Option<OrmError> = None;
        let mut values: Vec<Option<Vec<u8>>> = Vec::new();
        loop {
            let token = self.u8().await?;
            match token {
                0x81 => {
                    cols = self.column_metadata().await?;
                    resp.results.push(cols.clone());
                }
                0xD1 | 0xD2 => {
                    values.clear();
                    let bitmap = if token == 0xD2 { self.bytes(cols.len().div_ceil(8)).await?.to_vec() } else { Vec::new() };
                    for (i, c) in cols.clone().iter().enumerate() {
                        if token == 0xD2 && bitmap[i / 8] & (1 << (i % 8)) != 0 {
                            values.push(None);
                        } else {
                            values.push(self.value_bytes(&c.ti).await?);
                        }
                    }
                    if failed.is_none() && !self.attention_sent {
                        if let Err(e) = row(&cols, &values) {
                            failed = Some(e);
                        }
                    }
                }
                0xFD..=0xFF => {
                    let status = self.u16().await?;
                    let _cmd = self.u16().await?;
                    let count = self.u64().await?;
                    if token == 0xFE {
                        self.procs_done += 1;
                    }
                    if status & DONE_COUNT != 0 && token != 0xFE {
                        resp.counts.push(count);
                    }
                    if status & DONE_ATTN != 0 {
                        self.attention_sent = false;
                        self.deadline = None;
                        if status & DONE_MORE == 0 {
                            return Err(OrmError::Query(format!(
                                "SQL Server: the statement was cancelled after commandTimeout ({:?})",
                                self.command_timeout.unwrap_or_default()
                            )));
                        }
                    }
                    if status & DONE_ERROR != 0 && failed.is_none() && token == 0xFD && self.last_error.is_none() {
                        failed = Some(OrmError::Query("SQL Server reported an error without a message".into()));
                    }
                    if status & DONE_MORE == 0 && token != 0xFF && !self.attention_sent {
                        break;
                    }
                }
                0xAA => {
                    let m = self.message_token().await?;
                    let e = m.to_orm();
                    if matches!(e, OrmError::Connection(_)) {
                        self.broken = true;
                    }
                    if failed.is_none() {
                        failed = Some(e);
                        self.error_proc = Some(self.procs_done);
                    }
                    self.last_error = Some(m);
                }
                0xAB => {
                    let m = self.message_token().await?;
                    if self.messages.len() < 1000 {
                        self.messages.push(m);
                    }
                }
                0xE3 => self.envchange().await?,
                0x79 => resp.return_status = Some(self.u32().await? as i32),
                0xAC => {
                    let ordinal = self.u16().await?;
                    let name = self.b_varchar().await?;
                    let _status = self.u8().await?;
                    let _user = self.u32().await?;
                    let _flags = self.u16().await?;
                    let ti = self.type_info().await?;
                    let v = self.value_bytes(&ti).await?;
                    resp.return_values.push((ordinal, name, types::value(&ti, v.as_deref())));
                }
                0xA9 | 0xA5 | 0xA4 | 0xED => {
                    let n = self.u16().await? as usize;
                    self.bytes(n).await?;
                }
                0xE4 => {
                    let n = self.u32().await? as usize;
                    self.bytes(n).await?;
                }
                0xAD => {
                    let n = self.u16().await? as usize;
                    self.bytes(n).await?;
                }
                other => {
                    self.broken = true;
                    return Err(proto(format!("unexpected token {other:#04x}")));
                }
            }
        }
        self.deadline = None;
        match failed {
            Some(e) => Err(e),
            None => Ok(resp),
        }
    }

    /// Arm the command timeout for the request about to be read.
    pub(crate) fn arm_timeout(&mut self) {
        self.attention_sent = false;
        self.deadline = self.command_timeout.map(|t| Instant::now() + t);
    }

    async fn batch_discard(&mut self, sql: &str) -> Result<Response, OrmError> {
        self.arm_timeout();
        self.send_batch(sql).await?;
        self.read_response(|_, _| Ok(())).await
    }

    // ---- public surface ---------------------------------------------------------------

    /// Run a T-SQL batch (no parameters, any number of statements) and
    /// collect every result set: (columns, rows of typed values).
    pub async fn query(&mut self, sql: &str) -> Result<(Vec<(Vec<Column>, Vec<Vec<Value>>)>, Response), OrmError> {
        self.arm_timeout();
        self.send_batch(sql).await?;
        let mut sets: Vec<(Vec<Column>, Vec<Vec<Value>>)> = Vec::new();
        let resp = self
            .read_response(|cols, vals| {
                if sets.last().is_none_or(|(c, _)| c.as_slice() != cols) {
                    sets.push((cols.to_vec(), Vec::new()));
                }
                let row = cols.iter().zip(vals).map(|(c, v)| types::value(&c.ti, v.as_deref())).collect();
                sets.last_mut().expect("pushed").1.push(row);
                Ok(())
            })
            .await?;
        // Result sets without rows still appear.
        let mut out = Vec::new();
        let mut it = sets.into_iter().peekable();
        for cols in &resp.results {
            match it.peek() {
                Some((c, _)) if c == cols => out.push(it.next().expect("peeked")),
                _ => out.push((cols.clone(), Vec::new())),
            }
        }
        Ok((out, resp))
    }

    /// Run a batch for its effect.
    pub async fn simple(&mut self, sql: &str) -> Result<Response, OrmError> {
        self.batch_discard(sql).await
    }

    /// Whether a transaction is open (the server handed out a descriptor).
    pub fn in_transaction(&self) -> bool {
        self.txn != 0
    }

    /// The current database.
    pub fn database(&self) -> &str {
        &self.database
    }

    /// The server version (major, minor, build).
    pub fn server_version(&self) -> (u8, u8, u16) {
        self.server_version
    }

    /// Whether the session is encrypted (now: login-only sessions are not).
    pub fn is_tls(&self) -> bool {
        self.io.is_tls()
    }

    /// Whether the connection is unusable.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// The full server message behind the last error.
    pub fn last_error(&self) -> Option<&ServerMessage> {
        self.last_error.as_ref()
    }

    /// `PRINT` / `RAISERROR` (severity ≤ 10) messages received; taking them clears the list.
    pub fn take_messages(&mut self) -> Vec<ServerMessage> {
        std::mem::take(&mut self.messages)
    }

    /// Set the per-statement timeout (`None`: none).
    pub fn set_command_timeout(&mut self, t: Option<std::time::Duration>) {
        self.command_timeout = t;
    }
}

/// The TLS client config the encryption mode asks for (`None`: no TLS).
async fn tls_config(cfg: &MssqlConfig) -> Result<Option<Arc<ClientConfig>>, OrmError> {
    if cfg.encrypt == Encrypt::NotSupported {
        return Ok(None);
    }
    let ca = match &cfg.ca_file {
        Some(p) => Some(kernway_wire::offload::read_file(p).await.map_err(|e| conn_err(format!("read caFile {}: {e}", p.display())))?),
        None => None,
    };
    let check = if cfg.trust_server_certificate { kernway_tls::CertCheck::None } else { kernway_tls::CertCheck::Full };
    kernway_tls::client_config(ca.as_deref(), check).map(Some).map_err(|e| conn_err(format!("TLS configuration: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_map_to_errors() {
        let m = |number, class, msg: &str| ServerMessage { number, class, message: msg.into(), ..Default::default() };
        let e = m(2627, 14, "Violation of UNIQUE KEY constraint 'UQ_members_email'. Cannot insert duplicate key in object 'dbo.members'.").to_orm();
        assert!(matches!(e, OrmError::UniqueViolation { field } if field == "UQ_members_email"));
        assert!(matches!(m(547, 16, "FK").to_orm(), OrmError::ForeignKeyViolation));
        assert!(matches!(m(1205, 13, "deadlock").to_orm(), OrmError::Transaction(_)));
        assert!(matches!(m(4060, 20, "cannot open").to_orm(), OrmError::Connection(_)));
        assert!(matches!(m(208, 16, "Invalid object name").to_orm(), OrmError::Query(q) if q.contains("208")));
    }
}
