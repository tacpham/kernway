//! An authenticated Oracle session: connect, protocol / data-type negotiation,
//! O5LOGON authentication, and the request / response plumbing every call uses.

use std::collections::HashMap;

use kernway_orm_core::error::OrmError;
use kernway_wire::BufStream;
use rt_net::AsyncTcpStream;

use crate::auth_crypto::{self, ServerChallenge};
use crate::buf::{ParseError, PResult, Reader, Writer};
use crate::data_types_table::DATA_TYPES;
use crate::tns::{conn_err, packet_type, Tns, PACKET_HEADER_SIZE};

/// TTC message types.
pub(crate) mod msg {
    #![allow(dead_code)]
    pub const PROTOCOL: u8 = 1;
    pub const DATA_TYPES: u8 = 2;
    pub const FUNCTION: u8 = 3;
    pub const ERROR: u8 = 4;
    pub const ROW_HEADER: u8 = 6;
    pub const ROW_DATA: u8 = 7;
    pub const PARAMETER: u8 = 8;
    pub const STATUS: u8 = 9;
    pub const IO_VECTOR: u8 = 11;
    pub const WARNING: u8 = 15;
    pub const DESCRIBE_INFO: u8 = 16;
    pub const PIGGYBACK: u8 = 17;
    pub const FLUSH_OUT_BINDS: u8 = 19;
    pub const BIT_VECTOR: u8 = 21;
    pub const SERVER_SIDE_PIGGYBACK: u8 = 23;
    pub const IMPLICIT_RESULTSET: u8 = 27;
    pub const RENEGOTIATE: u8 = 28;
    pub const END_OF_RESPONSE: u8 = 29;
    pub const TOKEN: u8 = 33;
}

/// TTC function codes.
pub(crate) mod func {
    #![allow(dead_code)]
    pub const AUTH_PHASE_ONE: u8 = 118;
    pub const AUTH_PHASE_TWO: u8 = 115;
    pub const EXECUTE: u8 = 94;
    pub const FETCH: u8 = 5;
    pub const COMMIT: u8 = 14;
    pub const ROLLBACK: u8 = 15;
    pub const LOGOFF: u8 = 9;
    pub const CLOSE_CURSORS: u8 = 105;
}

const DATA_FLAGS_EOF: u16 = 0x0040;
const DATA_FLAGS_END_OF_RESPONSE: u16 = 0x2000;
const ACCEPT_FLAG_HAS_END_OF_RESPONSE: u32 = 0x0200_0000;
const MARKER_BREAK: u8 = 1;
const MARKER_RESET: u8 = 2;
const CHARSET_AL32UTF8: u16 = 873;
/// Highest TTC field version we speak (23.4).
const MAX_FIELD_VERSION: u8 = 24;
const CCAP_FIELD_VERSION: usize = 7;
const CCAP_TTC4: usize = 40;
const CCAP_END_OF_RESPONSE: u8 = 0x20;

/// Compile-time capabilities sent in the DataTypes message (python-oracledb's
/// set; index 7 becomes the negotiated field version, index 40 gains the
/// end-of-response bit when the server offers it).
const COMPILE_CAPS: [u8; 55] = [
    0x06, 0x00, 0x00, 0x00, 0xEA, 0x18, 0x00, 0x18, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x29,
    0x90, 0x03, 0x07, 0x03, 0x00, 0x01, 0x00, 0xCF, 0x00, 0x00, 0x04, 0x01, 0x00, 0x00, 0x00, 0x10,
    0x00, 0x00, 0x0C, 0x20, 0x00, 0xB8, 0x00, 0x08, 0x46, 0x00, 0x05, 0x00, 0x3E, 0x23, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x04,
];
const RUNTIME_CAPS: [u8; 13] = [0x02, 0, 0, 0, 0, 0, 0x05, 0, 0, 0, 0, 0, 0];

/// Connection parameters.
#[derive(Debug, Clone)]
pub struct OracleConfig {
    /// Database host.
    pub host: String,
    /// Listener port (1521 by default).
    pub port: u16,
    /// Service name to connect to (e.g. a PDB such as `FREEPDB1`).
    pub service_name: String,
    /// Account name.
    pub user: String,
    /// Account password.
    pub password: String,
    /// TLS (TCPS) policy (URL: `?ssl-mode=`). Default [`SslMode::Disabled`]:
    /// Oracle serves TCPS on its own listener port (often 2484).
    pub ssl_mode: SslMode,
    /// PEM file with the CA(s) to trust (URL: `?ssl-ca=`); `None` = Mozilla's roots.
    pub ssl_ca: Option<std::path::PathBuf>,
}

/// TCPS policy, named like MySQL's `--ssl-mode` for consistency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SslMode {
    /// Plain TCP.
    #[default]
    Disabled,
    /// TLS, without checking the server certificate (encryption only).
    Required,
    /// TLS, certificate must chain to a trusted CA (any host name).
    VerifyCa,
    /// TLS, trusted chain and a certificate naming the host.
    VerifyIdentity,
}

impl SslMode {
    fn parse(s: &str) -> Result<Self, OrmError> {
        match s.to_ascii_lowercase().replace('-', "_").as_str() {
            "disabled" => Ok(Self::Disabled),
            "required" => Ok(Self::Required),
            "verify_ca" => Ok(Self::VerifyCa),
            "verify_identity" | "verify_full" => Ok(Self::VerifyIdentity),
            other => Err(conn_err(format!("unknown ssl-mode '{other}'"))),
        }
    }
}

impl OracleConfig {
    /// Parse `oracle://user:password@host[:port]/service_name[?ssl-mode=…&ssl-ca=…]`.
    pub fn from_url(url: &str) -> Result<Self, OrmError> {
        let rest = url
            .strip_prefix("oracle://")
            .ok_or_else(|| conn_err("Oracle URL must start with oracle://"))?;
        let (creds, hostpart) = rest.rsplit_once('@').ok_or_else(|| conn_err("Oracle URL needs user@host"))?;
        let (user, password) = creds.split_once(':').unwrap_or((creds, ""));
        let (hostpart, options) = hostpart.split_once('?').unwrap_or((hostpart, ""));
        let mut ssl_mode = SslMode::default();
        let mut ssl_ca = None;
        for pair in options.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k.to_ascii_lowercase().replace('_', "-").as_str() {
                "ssl-mode" | "sslmode" => ssl_mode = SslMode::parse(v)?,
                "ssl-ca" => ssl_ca = Some(std::path::PathBuf::from(v)),
                other => return Err(conn_err(format!("unknown Oracle URL option '{other}'"))),
            }
        }
        let (hostport, service) = hostpart
            .split_once('/')
            .ok_or_else(|| conn_err("Oracle URL needs /service_name"))?;
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().map_err(|_| conn_err("bad port in Oracle URL"))?),
            None => (hostport, 1521),
        };
        Ok(Self {
            host: host.to_string(),
            port,
            service_name: service.to_string(),
            user: user.to_string(),
            password: password.to_string(),
            ssl_mode,
            ssl_ca,
        })
    }

    fn connect_descriptor(&self) -> String {
        format!(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL={})(HOST={})(PORT={}))(CONNECT_DATA=(SERVICE_NAME={})(CID=(PROGRAM=kernway)(HOST=localhost)(USER=kernway))))",
            if self.ssl_mode == SslMode::Disabled { "tcp" } else { "tcps" },
            self.host,
            self.port,
            self.service_name
        )
    }
}

/// An error the server reported (ORA-nnnnn).
#[derive(Debug, Default, Clone)]
pub(crate) struct ServerError {
    pub(crate) code: u32,
    pub(crate) message: String,
    #[allow(dead_code)]
    pub(crate) position: i64,
}

/// What the end-of-call ERROR message carried, also on success.
#[derive(Debug, Default, Clone)]
pub(crate) struct CallStatus {
    pub(crate) error: Option<ServerError>,
    pub(crate) cursor_id: u16,
    pub(crate) row_count: u64,
    pub(crate) call_status: u32,
}

/// A live, authenticated Oracle session.
pub struct Conn {
    pub(crate) tns: Tns,
    seq: u8,
    /// Negotiated TTC field version (24 for a 23ai / 26ai server).
    pub field_version: u8,
    /// Whether responses end with an explicit end-of-response.
    eor: bool,
    /// Session attributes returned at logon (AUTH_*).
    pub session: HashMap<String, String>,
    pub(crate) max_string_size: u32,
    /// Server cursors to close with the next call (close-cursors piggyback).
    pub(crate) pending_close: Vec<u32>,
}

/// Map a parse error to the driver's error type.
pub(crate) fn proto_err(e: ParseError) -> OrmError {
    match e {
        ParseError::Incomplete => conn_err("truncated Oracle response"),
        ParseError::Invalid(m) => conn_err(format!("Oracle protocol error: {m}")),
    }
}

impl Conn {
    /// Open a session: TCP connect, TNS connect, negotiate, authenticate.
    pub async fn connect(cfg: &OracleConfig) -> Result<Self, OrmError> {
        use std::net::ToSocketAddrs;
        let addr = (cfg.host.as_str(), cfg.port)
            .to_socket_addrs()
            .map_err(|e| conn_err(format!("resolve {}:{}: {e}", cfg.host, cfg.port)))?
            .next()
            .ok_or_else(|| conn_err(format!("no address for {}:{}", cfg.host, cfg.port)))?;
        let tcp = AsyncTcpStream::connect(addr)
            .await
            .map_err(|e| conn_err(format!("connect {}:{}: {e}", cfg.host, cfg.port)))?;
        let mut io = BufStream::new(tcp);
        let mut tls_config = None;
        if cfg.ssl_mode != SslMode::Disabled {
            let ca = match &cfg.ssl_ca {
                Some(p) => Some(std::fs::read(p).map_err(|e| conn_err(format!("read ssl-ca {}: {e}", p.display())))?),
                None => None,
            };
            let check = match cfg.ssl_mode {
                SslMode::VerifyIdentity => kernway_tls::CertCheck::Full,
                SslMode::VerifyCa => kernway_tls::CertCheck::CaOnly,
                _ => kernway_tls::CertCheck::None,
            };
            let tls = kernway_tls::client_config(ca.as_deref(), check).map_err(|e| conn_err(format!("TLS config: {e}")))?;
            io.upgrade_tls(tls.clone(), &cfg.host)
                .await
                .map_err(|e| conn_err(format!("TLS handshake with {}: {e}", cfg.host)))?;
            tls_config = Some(tls);
        }
        let mut tns = Tns::new(io);
        tns.connect(&cfg.connect_descriptor(), tls_config.map(|c| (c, cfg.host.as_str()))).await?;
        let mut conn = Self {
            tns,
            seq: 0,
            field_version: MAX_FIELD_VERSION,
            eor: false,
            session: HashMap::new(),
            max_string_size: 4000,
            pending_close: Vec::new(),
        };
        conn.negotiate().await?;
        conn.authenticate(cfg).await?;
        Ok(conn)
    }

    // --- framing -----------------------------------------------------------

    /// Send `payload` as one or more DATA packets (split at the SDU).
    pub(crate) async fn send_data(&mut self, payload: &[u8]) -> Result<(), OrmError> {
        let max = (self.tns.sdu as usize).saturating_sub(PACKET_HEADER_SIZE + 2).max(256);
        if payload.is_empty() {
            return self.tns.send(packet_type::DATA, 0, &[0, 0]).await;
        }
        let mut pkt = Vec::with_capacity(max + 2);
        for chunk in payload.chunks(max) {
            pkt.clear();
            pkt.extend_from_slice(&0u16.to_be_bytes());
            pkt.extend_from_slice(chunk);
            self.tns.send(packet_type::DATA, 0, &pkt).await?;
        }
        Ok(())
    }

    /// Whether responses end with an explicit end-of-response.
    pub(crate) fn end_of_response(&self) -> bool {
        self.eor
    }

    /// The next function sequence number (1..=255, wrapping past 0).
    pub(crate) fn next_seq(&mut self) -> u8 {
        self.seq = self.seq.wrapping_add(1);
        if self.seq == 0 {
            self.seq = 1;
        }
        self.seq
    }

    /// Start a function call: message type, function code, sequence number,
    /// and (field version 18+) the token number.
    pub(crate) fn function_header(&mut self, w: &mut Writer, code: u8) {
        let seq = self.next_seq();
        w.u8(msg::FUNCTION).u8(code).u8(seq);
        if self.field_version >= 18 {
            w.ub(0);
        }
    }

    /// Read one response, re-parsing as packets arrive until `parse` succeeds.
    /// In end-of-response mode the packets are first gathered up to the end
    /// marker. MARKER packets trigger the reset exchange, after which the
    /// response (an error) starts afresh.
    pub(crate) async fn read_response<T>(
        &mut self,
        mut parse: impl FnMut(&[u8]) -> PResult<T>,
    ) -> Result<T, OrmError> {
        let mut bytes: Vec<u8> = Vec::new();
        loop {
            let pkt = self.tns.recv().await?;
            match pkt.packet_type {
                packet_type::DATA => {
                    let p = pkt.payload;
                    if p.len() < 2 {
                        return Err(conn_err("short DATA packet"));
                    }
                    let flags = u16::from_be_bytes([p[0], p[1]]);
                    let body = &p[2..];
                    if flags & DATA_FLAGS_EOF != 0 && body.is_empty() {
                        return Err(conn_err("the server closed the session (ORA-12572)"));
                    }
                    let end = flags & DATA_FLAGS_END_OF_RESPONSE != 0 || body == [msg::END_OF_RESPONSE];
                    bytes.extend_from_slice(body);
                    if self.eor && !end {
                        continue;
                    }
                    match parse(&bytes) {
                        Ok(v) => return Ok(v),
                        Err(ParseError::Incomplete) if !(self.eor && end) => continue,
                        Err(e) => return Err(proto_err(e)),
                    }
                }
                packet_type::MARKER => {
                    let kind = pkt.payload.get(2).copied().unwrap_or(0);
                    self.reset(kind).await?;
                    bytes.clear();
                }
                packet_type::CONTROL => continue,
                other => return Err(conn_err(format!("unexpected TNS packet type {other}"))),
            }
        }
    }

    /// The break/reset exchange: send RESET, wait for the server's RESET, then
    /// skip any further markers. The caller continues with the next DATA packet.
    async fn reset(&mut self, mut kind: u8) -> Result<(), OrmError> {
        self.tns.send(packet_type::MARKER, 0, &[1, 0, MARKER_RESET]).await?;
        while kind != MARKER_RESET {
            let pkt = self.tns.recv().await?;
            if pkt.packet_type == packet_type::MARKER {
                kind = pkt.payload.get(2).copied().unwrap_or(0);
            }
        }
        let _ = MARKER_BREAK;
        Ok(())
    }

    // --- negotiation --------------------------------------------------------

    async fn negotiate(&mut self) -> Result<(), OrmError> {
        // Protocol message.
        let mut w = Writer::new();
        w.u8(msg::PROTOCOL).u8(6).u8(0).raw(b"kernway").u8(0);
        self.send_data(&w.buf).await?;
        let (server_ccaps, server_rcaps) = self
            .read_response(|b| {
                let mut r = Reader::new(b);
                loop {
                    match r.u8()? {
                        msg::PROTOCOL => return parse_protocol_response(&mut r),
                        t => {
                            let mut st = CallStatus::default();
                            if common_message(&mut r, t, 0, &mut st)? {
                                return Err(ParseError::Invalid(format!(
                                    "server error during protocol negotiation: {:?}",
                                    st.error
                                )));
                            }
                        }
                    }
                }
            })
            .await?;
        if let Some(fv) = server_ccaps.get(CCAP_FIELD_VERSION) {
            self.field_version = self.field_version.min(*fv);
        }
        if server_rcaps.get(6).is_some_and(|b| b & 0x04 != 0) {
            self.max_string_size = 32767;
        }

        // DataTypes message.
        let mut ccaps = COMPILE_CAPS;
        ccaps[CCAP_FIELD_VERSION] = self.field_version;
        let server_eor = self.tns.version >= 319 && self.tns.accept_flags2 & ACCEPT_FLAG_HAS_END_OF_RESPONSE != 0;
        if server_eor {
            ccaps[CCAP_TTC4] |= CCAP_END_OF_RESPONSE;
        }
        let mut w = Writer::new();
        w.u8(msg::DATA_TYPES)
            .u16le(CHARSET_AL32UTF8)
            .u16le(CHARSET_AL32UTF8)
            .u8(0x03)
            .bytes_wl(&ccaps)
            .bytes_wl(&RUNTIME_CAPS);
        for &(dt, conv, rep) in DATA_TYPES.iter() {
            w.u16be(dt).u16be(conv).u16be(rep).u16be(0);
        }
        w.u16be(0);
        self.send_data(&w.buf).await?;
        self.read_response(|b| {
            let mut r = Reader::new(b);
            loop {
                match r.u8()? {
                    msg::DATA_TYPES => loop {
                        let dt = r.u16be()?;
                        if dt == 0 {
                            return Ok(());
                        }
                        if r.u16be()? != 0 {
                            r.skip(4)?;
                        }
                    },
                    t => {
                        let mut st = CallStatus::default();
                        if common_message(&mut r, t, 0, &mut st)? {
                            return Err(ParseError::Invalid(format!(
                                "server error during data type negotiation: {:?}",
                                st.error
                            )));
                        }
                    }
                }
            }
        })
        .await?;
        // From here on responses end with an explicit end-of-response.
        self.eor = server_eor;
        Ok(())
    }

    // --- authentication -----------------------------------------------------

    async fn authenticate(&mut self, cfg: &OracleConfig) -> Result<(), OrmError> {
        let pid = std::process::id().to_string();
        let phase_one: Vec<(&str, String, u32)> = vec![
            ("AUTH_TERMINAL", "unknown".into(), 0),
            ("AUTH_PROGRAM_NM", "kernway".into(), 0),
            ("AUTH_MACHINE", "localhost".into(), 0),
            ("AUTH_PID", pid, 0),
            ("AUTH_SID", "kernway".into(), 0),
        ];
        let params = self.auth_round(func::AUTH_PHASE_ONE, &cfg.user, 0x0000_0001, &phase_one).await?;
        self.session.extend(params.iter().map(|(k, v, _)| (k.clone(), v.clone())));

        let verifier_type = params
            .iter()
            .find(|(k, _, _)| k == "AUTH_VFR_DATA")
            .map(|(_, _, f)| *f)
            .ok_or_else(|| conn_err("server sent no password verifier (AUTH_VFR_DATA)"))?;
        let get = |k: &str| self.session.get(k).map(String::as_str);
        let num = |k: &str| get(k).and_then(|v| v.parse::<u32>().ok());
        let challenge = ServerChallenge {
            verifier_type,
            vfr_data: get("AUTH_VFR_DATA").unwrap_or_default(),
            sesskey: get("AUTH_SESSKEY").ok_or_else(|| conn_err("server sent no AUTH_SESSKEY"))?,
            vgen_count: num("AUTH_PBKDF2_VGEN_COUNT"),
            sder_count: num("AUTH_PBKDF2_SDER_COUNT"),
            csk_salt: get("AUTH_PBKDF2_CSK_SALT"),
        };
        let proof = auth_crypto::prove(cfg.password.as_bytes(), &challenge, None).map_err(conn_err)?;

        let mut phase_two: Vec<(&str, String, u32)> = vec![("AUTH_SESSKEY", proof.sesskey.clone(), 1)];
        if let Some(speedy) = &proof.speedy_key {
            phase_two.push(("AUTH_PBKDF2_SPEEDY_KEY", speedy.clone(), 0));
        }
        phase_two.extend([
            ("AUTH_PASSWORD", proof.password.clone(), 0),
            ("SESSION_CLIENT_CHARSET", CHARSET_AL32UTF8.to_string(), 0),
            ("SESSION_CLIENT_DRIVER_NAME", "kernway thn : 0.1.0".into(), 0),
            ("SESSION_CLIENT_VERSION", (1u32 << 20).to_string(), 0),
            ("AUTH_ALTER_SESSION", "ALTER SESSION SET TIME_ZONE='+00:00'\0".into(), 1),
        ]);
        let params = self
            .auth_round(func::AUTH_PHASE_TWO, &cfg.user, 0x0000_0101, &phase_two)
            .await?;
        self.session.extend(params.into_iter().map(|(k, v, _)| (k, v)));
        let response = self
            .session
            .get("AUTH_SVR_RESPONSE")
            .ok_or_else(|| conn_err("server sent no AUTH_SVR_RESPONSE"))?;
        if !auth_crypto::verify_server_response(&proof.session_key, response) {
            return Err(conn_err("server failed to prove it knows the password (AUTH_SVR_RESPONSE)"));
        }
        Ok(())
    }

    /// One auth round trip; returns the server's key/value/flag parameters.
    async fn auth_round(
        &mut self,
        code: u8,
        user: &str,
        mode: u32,
        pairs: &[(&str, String, u32)],
    ) -> Result<Vec<(String, String, u32)>, OrmError> {
        let mut w = Writer::new();
        self.function_header(&mut w, code);
        let has_user = !user.is_empty();
        w.u8(has_user as u8)
            .ub(user.len() as u64)
            .ub(u64::from(mode))
            .u8(1)
            .ub(pairs.len() as u64)
            .u8(1)
            .u8(1);
        if has_user {
            w.bytes_wl(user.as_bytes());
        }
        for (k, v, flags) in pairs {
            w.bytes_2l(k.as_bytes()).bytes_2l(v.as_bytes()).ub(u64::from(*flags));
        }
        self.send_data(&w.buf).await?;
        let fv = self.field_version;
        let (params, status) = self
            .read_response(|b| {
                let mut r = Reader::new(b);
                let mut params = Vec::new();
                let mut st = CallStatus::default();
                loop {
                    let t = r.u8()?;
                    if t == msg::PARAMETER {
                        let n = r.ub2()?;
                        for _ in 0..n {
                            let key = r.str_2l()?.unwrap_or_default();
                            let value = r.str_2l()?.unwrap_or_default();
                            let flags = r.ub4()?;
                            params.push((key, value, flags));
                        }
                    } else if common_message(&mut r, t, fv, &mut st)? {
                        return Ok((params, st));
                    }
                }
            })
            .await?;
        if let Some(e) = status.error {
            return Err(OrmError::Connection(format!("ORA-{:05}: {}", e.code, e.message.trim_start_matches(&format!("ORA-{:05}: ", e.code)))));
        }
        Ok(params)
    }
}

/// The Protocol message response (after its type byte): returns the server's
/// compile and runtime capability arrays.
fn parse_protocol_response(r: &mut Reader<'_>) -> PResult<(Vec<u8>, Vec<u8>)> {
    r.u8()?; // server version
    r.u8()?; // zero
    r.null_terminated()?; // banner
    r.u16le()?; // charset id
    r.u8()?; // server flags
    let elems = r.u16le()? as usize;
    r.skip(elems * 5)?;
    let fdo_len = r.u16be()? as usize;
    r.skip(fdo_len)?;
    let ccaps = r.bytes()?.unwrap_or_default();
    let rcaps = r.bytes()?.unwrap_or_default();
    Ok((ccaps, rcaps))
}

/// Handle a message every response may carry. Returns `true` when it ends the
/// response (ERROR, STATUS, END_OF_RESPONSE). Unknown types are an error.
pub(crate) fn common_message(r: &mut Reader<'_>, t: u8, fv: u8, st: &mut CallStatus) -> PResult<bool> {
    match t {
        msg::ERROR => {
            parse_error(r, fv, st)?;
            Ok(true)
        }
        msg::STATUS => {
            st.call_status = r.ub4()?;
            r.ub2()?; // end-to-end sequence number
            Ok(true)
        }
        msg::END_OF_RESPONSE => Ok(true),
        msg::WARNING => {
            let num = r.ub2()?;
            let len = r.ub2()?;
            r.ub2()?; // flags
            if num != 0 && len > 0 {
                r.skip_bytes()?;
            }
            Ok(false)
        }
        msg::TOKEN => {
            r.ub8()?;
            Ok(false)
        }
        msg::SERVER_SIDE_PIGGYBACK => {
            server_piggyback(r)?;
            Ok(false)
        }
        other => Err(ParseError::Invalid(format!("unexpected TTC message type {other}"))),
    }
}

/// The ERROR message (end of call), on success as well as failure.
fn parse_error(r: &mut Reader<'_>, fv: u8, st: &mut CallStatus) -> PResult<()> {
    st.call_status = r.ub4()?;
    r.skip_ub()?; // end-to-end seq
    r.skip_ub()?; // current row number
    r.skip_ub()?; // error number (old)
    r.skip_ub()?; // array elem error
    r.skip_ub()?; // array elem error
    st.cursor_id = r.ub2()?;
    let position = r.sb()?;
    r.skip(5)?; // sql type, fatal, flags, user cursor options, UPI parameter
    r.u8()?; // flags (compilation-error warning)
    // rowid
    r.skip_ub()?;
    r.skip_ub()?;
    r.u8()?;
    r.skip_ub()?;
    r.skip_ub()?;
    r.skip_ub()?; // OS error
    r.u8()?; // statement number
    r.u8()?; // call number
    r.skip_ub()?; // padding
    r.skip_ub()?; // success iters
    r.skip_bytes_2l()?; // oerrdd
    // batch error codes
    let n = r.ub2()?;
    if n > 0 {
        let first = r.u8()?;
        for _ in 0..n {
            if first == 0xFE {
                r.skip_ub()?;
            }
            r.ub2()?;
        }
        if first == 0xFE {
            r.skip(1)?;
        }
    }
    // batch error offsets
    let n = r.ub4()?;
    if n > 0 {
        let first = r.u8()?;
        for _ in 0..n {
            if first == 0xFE {
                r.skip_ub()?;
            }
            r.ub4()?;
        }
        if first == 0xFE {
            r.skip(1)?;
        }
    }
    // batch error messages
    let n = r.ub2()?;
    if n > 0 {
        r.skip(1)?;
        for _ in 0..n {
            r.skip_ub()?;
            r.skip_bytes()?;
            r.skip(2)?;
        }
    }
    let code = r.ub4()?;
    st.row_count = r.ub8()?;
    if fv >= 14 {
        r.skip_ub()?;
        r.skip_ub()?;
    }
    if code != 0 {
        let message = r.str()?.unwrap_or_default().trim_end().to_string();
        st.error = Some(ServerError { code, message, position });
    }
    Ok(())
}

fn keyword_pairs(r: &mut Reader<'_>, n: u16) -> PResult<()> {
    for _ in 0..n {
        if r.ub2()? > 0 {
            r.skip_bytes()?;
        }
        if r.ub2()? > 0 {
            r.skip_bytes()?;
        }
        r.ub2()?;
    }
    Ok(())
}

fn server_piggyback(r: &mut Reader<'_>) -> PResult<()> {
    match r.u8()? {
        1 | 3 => {}
        2 => {
            r.ub2()?;
            r.skip_bytes()?;
        }
        4 => {
            r.skip_ub()?;
            r.u8()?;
            let n = r.ub2()?;
            if n > 0 {
                r.u8()?;
                for _ in 0..n {
                    if r.ub2()? > 0 {
                        r.skip_bytes()?;
                    }
                    if r.ub2()? > 0 {
                        r.skip_bytes()?;
                    }
                    r.skip_ub()?;
                }
            }
            r.ub4()?;
            r.ub4()?;
            r.ub2()?;
        }
        5 => {
            r.skip_ub()?;
            r.u8()?;
            let n = r.ub2()?;
            r.u8()?;
            keyword_pairs(r, n)?;
            r.skip_ub()?;
        }
        7 => r.skip_bytes_2l()?,
        8 => {
            r.skip_ub()?;
            r.u8()?;
            r.skip_ub()?;
            r.skip_ub()?;
            r.u8()?;
            r.skip_bytes_2l()?;
        }
        9 => {
            r.skip_ub()?;
            r.u8()?;
        }
        10 => {
            r.skip_ub()?;
            r.u8()?;
            r.skip_ub()?;
            r.skip_ub()?;
            r.skip_ub()?;
        }
        op => return Err(ParseError::Invalid(format!("unknown server piggyback opcode {op}"))),
    }
    Ok(())
}
