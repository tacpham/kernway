//! A self-written MySQL client protocol — no external MySQL client crate.
//!
//! This speaks the classic MySQL protocol directly over a blocking
//! [`std::net::TcpStream`]. It is intentionally run on a blocking thread (see
//! [`crate::repository`]), not on the async `rt-net` reactor: `orm-core`'s
//! `BoxFuture` requires `Send`, while `rt-net`'s thread-per-core futures are
//! `!Send`, so the same `spawn_blocking` bridge the SQLite backend uses is the
//! correct fit here.
//!
//! ## Scope (first cut)
//!
//! - Handshake v10 + `mysql_native_password` authentication (SHA-1 scramble).
//! - `COM_QUERY` (text protocol) with values inlined as escaped SQL literals.
//! - Result-set parsing (column count → column defs → text rows → EOF), and
//!   `OK` / `ERR` packets.
//!
//! ## Not yet (documented hardening path)
//!
//! - Prepared statements / binary protocol (safer parameter binding than text
//!   escaping — the next step for money paths).
//! - `caching_sha2_password` (MySQL 8 default): needs TLS or an RSA key
//!   exchange. Until then, the MySQL account must use `mysql_native_password`.
//! - TLS.

use kernway_orm_core::error::OrmError;
use rt_net::AsyncTcpStream;
use std::net::ToSocketAddrs;

// --- Capability flags (only the ones we assert) ---
const CLIENT_LONG_PASSWORD: u32 = 0x0000_0001;
const CLIENT_LONG_FLAG: u32 = 0x0000_0004;
const CLIENT_CONNECT_WITH_DB: u32 = 0x0000_0008;
const CLIENT_PROTOCOL_41: u32 = 0x0000_0200;
const CLIENT_TRANSACTIONS: u32 = 0x0000_2000;
const CLIENT_SECURE_CONNECTION: u32 = 0x0000_8000;
const CLIENT_PLUGIN_AUTH: u32 = 0x0008_0000;

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
}

impl MysqlConfig {
    /// Parse `mysql://user:password@host:port/database`.
    ///
    /// `port` defaults to 3306 and `database` is optional. Credentials and the
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

        // host_part = host[:port][/database]
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
        Ok(Self { host, port, user, password, database })
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

/// A live, authenticated MySQL connection over the per-core async reactor.
pub struct Conn {
    stream: AsyncTcpStream,
    seq: u8,
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
        let mut conn = Self { stream, seq: 0 };
        conn.handshake(cfg).await?;
        Ok(conn)
    }

    /// Read exactly `buf.len()` bytes, parking on the reactor across short reads.
    async fn read_exact_into(&mut self, buf: &mut [u8]) -> Result<(), OrmError> {
        let mut filled = 0;
        while filled < buf.len() {
            let n = self
                .stream
                .read(&mut buf[filled..])
                .await
                .map_err(|e| conn_err(format!("read: {e}")))?;
            if n == 0 {
                return Err(conn_err("unexpected EOF from MySQL"));
            }
            filled += n;
        }
        Ok(())
    }

    // --- Packet framing ---------------------------------------------------

    /// Read one logical packet payload, transparently joining the 16 MiB
    /// continuation packets MySQL splits large payloads into.
    async fn read_packet(&mut self) -> Result<Vec<u8>, OrmError> {
        let mut payload = Vec::new();
        loop {
            let mut header = [0u8; 4];
            self.read_exact_into(&mut header).await?;
            let len = (header[0] as usize) | (header[1] as usize) << 8 | (header[2] as usize) << 16;
            self.seq = header[3].wrapping_add(1);
            let mut chunk = vec![0u8; len];
            self.read_exact_into(&mut chunk).await?;
            payload.extend_from_slice(&chunk);
            if len < 0xFF_FFFF {
                break;
            }
        }
        Ok(payload)
    }

    /// Write one packet, using (and advancing) the current sequence id.
    async fn write_packet(&mut self, payload: &[u8]) -> Result<(), OrmError> {
        if payload.len() >= 0xFF_FFFF {
            return Err(conn_err("outgoing packet too large (>16 MiB not split)"));
        }
        let len = payload.len();
        let mut frame = Vec::with_capacity(4 + len);
        frame.extend_from_slice(&[len as u8, (len >> 8) as u8, (len >> 16) as u8, self.seq]);
        frame.extend_from_slice(payload);
        self.seq = self.seq.wrapping_add(1);
        self.stream
            .write_all(&frame)
            .await
            .map_err(|e| conn_err(format!("write packet: {e}")))
    }

    // --- Connection phase -------------------------------------------------

    async fn handshake(&mut self, cfg: &MysqlConfig) -> Result<(), OrmError> {
        let greeting = self.read_packet().await?;
        let (salt, plugin) = parse_handshake(&greeting)?;
        if plugin != "mysql_native_password" && !plugin.is_empty() {
            // MySQL 8 default is caching_sha2_password. We handle an explicit
            // switch to native below; a fast-auth caching_sha2 start we cannot.
            if plugin == "caching_sha2_password" {
                // Proceed — many servers accept a native-password response and
                // reply with an AuthSwitchRequest we handle below. If not, the
                // error message tells the operator what to change.
            }
        }

        let scramble = scramble_native(cfg.password.as_bytes(), &salt);
        let response = build_handshake_response(cfg, &scramble);
        self.write_packet(&response).await?;

        // Response: OK, ERR, or an AuthSwitchRequest (0xFE).
        let reply = self.read_packet().await?;
        match reply.first().copied() {
            Some(0x00) => Ok(()),
            Some(0xFF) => Err(parse_err_packet(&reply)),
            Some(0xFE) => self.handle_auth_switch(cfg, &reply).await,
            Some(0x01) => Err(conn_err(
                "server requires caching_sha2_password full auth (needs TLS/RSA); \
                 set this account to mysql_native_password for now",
            )),
            other => Err(conn_err(format!("unexpected auth reply byte {other:?}"))),
        }
    }

    async fn handle_auth_switch(&mut self, cfg: &MysqlConfig, packet: &[u8]) -> Result<(), OrmError> {
        // AuthSwitchRequest: 0xFE, plugin name (NUL-terminated), auth data.
        let mut p = 1usize;
        let name_end = packet[p..]
            .iter()
            .position(|&b| b == 0)
            .map(|i| p + i)
            .unwrap_or(packet.len());
        let plugin = String::from_utf8_lossy(&packet[p..name_end]).to_string();
        p = (name_end + 1).min(packet.len());
        let new_salt: Vec<u8> = packet[p..].iter().copied().take_while(|&b| b != 0).collect();

        if plugin != "mysql_native_password" {
            return Err(conn_err(format!(
                "server asked to switch to unsupported auth plugin '{plugin}'; \
                 set this account to mysql_native_password"
            )));
        }
        let scramble = scramble_native(cfg.password.as_bytes(), &new_salt);
        self.write_packet(&scramble).await?;
        let reply = self.read_packet().await?;
        match reply.first().copied() {
            Some(0x00) => Ok(()),
            Some(0xFF) => Err(parse_err_packet(&reply)),
            other => Err(conn_err(format!("unexpected post-switch reply byte {other:?}"))),
        }
    }

    // --- Command phase ----------------------------------------------------

    /// Run one textual statement (`COM_QUERY`) and collect its result.
    pub async fn query(&mut self, sql: &str) -> Result<QueryResult, OrmError> {
        // Each command starts a fresh sequence.
        self.seq = 0;
        let mut payload = Vec::with_capacity(sql.len() + 1);
        payload.push(0x03); // COM_QUERY
        payload.extend_from_slice(sql.as_bytes());
        self.write_packet(&payload).await?;

        let first = self.read_packet().await?;
        match first.first().copied() {
            Some(0x00) => Ok(parse_ok_packet(&first)),
            Some(0xFF) => Err(parse_err_packet(&first)),
            Some(0xFB) => Err(OrmError::Query("LOCAL INFILE not supported".into())),
            _ => {
                let (col_count, _) = read_lenenc_int(&first, 0)
                    .ok_or_else(|| OrmError::Query("bad column count packet".into()))?;
                self.read_result_set(col_count as usize).await
            }
        }
    }

    async fn read_result_set(&mut self, col_count: usize) -> Result<QueryResult, OrmError> {
        // Column definition packets.
        let mut columns = Vec::with_capacity(col_count);
        for _ in 0..col_count {
            let def = self.read_packet().await?;
            columns.push(parse_column_name(&def));
        }
        // EOF after column defs (we do not set CLIENT_DEPRECATE_EOF).
        let after_cols = self.read_packet().await?;
        if !is_eof(&after_cols) {
            // Some servers may send a row straight away; treat it as the first row.
            let mut rows = Vec::new();
            rows.push(parse_text_row(&after_cols, col_count)?);
            self.collect_rows(col_count, &mut rows).await?;
            return Ok(QueryResult::Rows { columns, rows });
        }
        let mut rows = Vec::new();
        self.collect_rows(col_count, &mut rows).await?;
        Ok(QueryResult::Rows { columns, rows })
    }

    async fn collect_rows(
        &mut self,
        col_count: usize,
        rows: &mut Vec<Vec<Option<String>>>,
    ) -> Result<(), OrmError> {
        loop {
            let packet = self.read_packet().await?;
            if is_eof(&packet) {
                break;
            }
            if packet.first() == Some(&0xFF) {
                return Err(parse_err_packet(&packet));
            }
            rows.push(parse_text_row(&packet, col_count)?);
        }
        Ok(())
    }

    // --- Prepared statements (binary protocol) ---------------------------

    /// Prepare, execute with bound parameters, and close — one round of each.
    ///
    /// A statement cache is a later optimisation; this already gives real
    /// server-side parameter binding (no text escaping) for the hot CRUD paths.
    pub async fn exec_prepared(
        &mut self,
        sql: &str,
        params: &[BindValue],
    ) -> Result<QueryResult, OrmError> {
        let (stmt_id, num_params) = self.stmt_prepare(sql).await?;
        let result = self.stmt_execute(stmt_id, num_params, params).await;
        // Always try to free the statement, even on error.
        let _ = self.stmt_close(stmt_id).await;
        result
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

    async fn stmt_execute(
        &mut self,
        stmt_id: u32,
        num_params: u16,
        params: &[BindValue],
    ) -> Result<QueryResult, OrmError> {
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
        match first.first().copied() {
            Some(0x00) => Ok(parse_ok_packet(&first)),
            Some(0xFF) => Err(parse_err_packet(&first)),
            _ => {
                let (col_count, _) = read_lenenc_int(&first, 0)
                    .ok_or_else(|| OrmError::Query("bad column count packet".into()))?;
                self.read_binary_result_set(col_count as usize).await
            }
        }
    }

    async fn read_binary_result_set(&mut self, col_count: usize) -> Result<QueryResult, OrmError> {
        let mut metas = Vec::with_capacity(col_count);
        let mut columns = Vec::with_capacity(col_count);
        for _ in 0..col_count {
            let def = self.read_packet().await?;
            let meta = parse_col_meta(&def);
            columns.push(meta.name.clone());
            metas.push(meta);
        }
        self.read_packet().await?; // EOF after column defs

        let mut rows = Vec::new();
        loop {
            let packet = self.read_packet().await?;
            if is_eof(&packet) {
                break;
            }
            if packet.first() == Some(&0xFF) {
                return Err(parse_err_packet(&packet));
            }
            rows.push(parse_binary_row(&packet, &metas)?);
        }
        Ok(QueryResult::Rows { columns, rows })
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

/// Parse HandshakeV10 → (20-byte salt, auth plugin name).
fn parse_handshake(p: &[u8]) -> Result<(Vec<u8>, String), OrmError> {
    if p.first() != Some(&0x0a) {
        if p.first() == Some(&0xFF) {
            return Err(parse_err_packet(p));
        }
        return Err(conn_err("unsupported handshake protocol version (need v10)"));
    }
    let mut i = 1usize;
    // server version (NUL-terminated string)
    let vend = p[i..].iter().position(|&b| b == 0).map(|k| i + k).ok_or_else(|| conn_err("bad server version"))?;
    i = vend + 1;
    i += 4; // connection id
    let mut salt = Vec::with_capacity(20);
    salt.extend_from_slice(p.get(i..i + 8).ok_or_else(|| conn_err("short handshake (salt1)"))?);
    i += 8;
    i += 1; // filler
    i += 2; // capability flags lower
    if p.len() > i {
        i += 1; // charset
        i += 2; // status flags
        i += 2; // capability flags upper
        let _auth_len = *p.get(i).unwrap_or(&0);
        i += 1; // length of auth-plugin-data
        i += 10; // reserved
        // salt part 2 — up to 12 bytes then a NUL.
        let salt2: Vec<u8> = p.get(i..).unwrap_or(&[]).iter().copied().take_while(|&b| b != 0).take(12).collect();
        i += salt2.len() + 1;
        salt.extend_from_slice(&salt2);
        // auth plugin name (NUL-terminated), if present.
        let plugin = if i < p.len() {
            let end = p[i..].iter().position(|&b| b == 0).map(|k| i + k).unwrap_or(p.len());
            String::from_utf8_lossy(&p[i..end]).to_string()
        } else {
            String::new()
        };
        Ok((salt, plugin))
    } else {
        Ok((salt, String::new()))
    }
}

fn build_handshake_response(cfg: &MysqlConfig, scramble: &[u8]) -> Vec<u8> {
    let mut caps = CLIENT_LONG_PASSWORD
        | CLIENT_LONG_FLAG
        | CLIENT_PROTOCOL_41
        | CLIENT_TRANSACTIONS
        | CLIENT_SECURE_CONNECTION
        | CLIENT_PLUGIN_AUTH;
    if cfg.database.is_some() {
        caps |= CLIENT_CONNECT_WITH_DB;
    }

    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&caps.to_le_bytes());
    out.extend_from_slice(&MAX_PACKET.to_le_bytes());
    out.push(UTF8MB4_GENERAL_CI);
    out.extend_from_slice(&[0u8; 23]); // reserved
    out.extend_from_slice(cfg.user.as_bytes());
    out.push(0);
    // CLIENT_SECURE_CONNECTION: 1-byte length-prefixed auth response.
    out.push(scramble.len() as u8);
    out.extend_from_slice(scramble);
    if let Some(db) = &cfg.database {
        out.extend_from_slice(db.as_bytes());
        out.push(0);
    }
    out.extend_from_slice(b"mysql_native_password");
    out.push(0);
    out
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

fn parse_ok_packet(p: &[u8]) -> QueryResult {
    // 0x00, lenenc affected_rows, lenenc last_insert_id, …
    let mut i = 1usize;
    let (affected_rows, n1) = read_lenenc_int(p, i).unwrap_or((0, 1));
    i += n1;
    let (last_insert_id, _n2) = read_lenenc_int(p, i).unwrap_or((0, 1));
    QueryResult::Ok { affected_rows, last_insert_id }
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

fn parse_column_name(def: &[u8]) -> String {
    // ColumnDefinition41: catalog, schema, table, org_table, name, org_name …
    // all length-encoded strings. The 5th lenenc string is the column name.
    let mut i = 0usize;
    let mut name = String::new();
    for field in 0..5 {
        match read_lenenc_str(def, i) {
            Some((bytes, next)) => {
                if field == 4 {
                    name = String::from_utf8_lossy(bytes).to_string();
                }
                i = next;
            }
            None => break,
        }
    }
    name
}

fn parse_text_row(p: &[u8], col_count: usize) -> Result<Vec<Option<String>>, OrmError> {
    let mut i = 0usize;
    let mut out = Vec::with_capacity(col_count);
    for _ in 0..col_count {
        if p.get(i) == Some(&0xFB) {
            out.push(None);
            i += 1;
        } else {
            let (bytes, next) =
                read_lenenc_str(p, i).ok_or_else(|| OrmError::Query("truncated text row".into()))?;
            out.push(Some(String::from_utf8_lossy(bytes).to_string()));
            i = next;
        }
    }
    Ok(out)
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

fn parse_binary_row(packet: &[u8], metas: &[ColMeta]) -> Result<Vec<Option<String>>, OrmError> {
    // 0x00 header, then a null bitmap of (num_cols + 7 + 2) / 8 bytes (offset 2).
    let n = metas.len();
    let bitmap_len = (n + 7 + 2) / 8;
    let bitmap = packet
        .get(1..1 + bitmap_len)
        .ok_or_else(|| OrmError::Query("short binary row".into()))?;
    let mut i = 1 + bitmap_len;
    let mut out = Vec::with_capacity(n);
    for (col, meta) in metas.iter().enumerate() {
        let is_null = bitmap[(col + 2) / 8] & (1 << ((col + 2) % 8)) != 0;
        if is_null {
            out.push(None);
            continue;
        }
        let (value, next) = decode_binary_value(packet, i, meta)?;
        out.push(Some(value));
        i = next;
    }
    Ok(out)
}

/// Decode one binary value to its text form, so the existing column-typed
/// text→JSON mapping can turn it into the entity field.
fn decode_binary_value(p: &[u8], i: usize, meta: &ColMeta) -> Result<(String, usize), OrmError> {
    let short = || OrmError::Query("truncated binary value".into());
    match meta.col_type {
        MYSQL_TYPE_TINY => {
            let b = *p.get(i).ok_or_else(short)?;
            let s = if meta.unsigned { (b as u64).to_string() } else { (b as i8 as i64).to_string() };
            Ok((s, i + 1))
        }
        MYSQL_TYPE_SHORT | MYSQL_TYPE_YEAR => {
            let b = p.get(i..i + 2).ok_or_else(short)?;
            let raw = u16::from_le_bytes([b[0], b[1]]);
            let s = if meta.unsigned { raw.to_string() } else { (raw as i16 as i64).to_string() };
            Ok((s, i + 2))
        }
        MYSQL_TYPE_LONG | MYSQL_TYPE_INT24 => {
            let b = p.get(i..i + 4).ok_or_else(short)?;
            let raw = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            let s = if meta.unsigned { raw.to_string() } else { (raw as i32 as i64).to_string() };
            Ok((s, i + 4))
        }
        MYSQL_TYPE_LONGLONG => {
            let b = p.get(i..i + 8).ok_or_else(short)?;
            let raw = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            let s = if meta.unsigned { raw.to_string() } else { (raw as i64).to_string() };
            Ok((s, i + 8))
        }
        MYSQL_TYPE_FLOAT => {
            let b = p.get(i..i + 4).ok_or_else(short)?;
            Ok((f32::from_le_bytes([b[0], b[1], b[2], b[3]]).to_string(), i + 4))
        }
        MYSQL_TYPE_DOUBLE => {
            let b = p.get(i..i + 8).ok_or_else(short)?;
            Ok((f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]).to_string(), i + 8))
        }
        MYSQL_TYPE_DATE | MYSQL_TYPE_DATETIME | MYSQL_TYPE_TIMESTAMP => decode_datetime(p, i),
        MYSQL_TYPE_TIME => decode_time(p, i),
        // DECIMAL / NEWDECIMAL / VARCHAR / VAR_STRING / STRING / BLOB / JSON / BIT / …
        _ => {
            let (bytes, next) = read_lenenc_str(p, i).ok_or_else(short)?;
            Ok((String::from_utf8_lossy(bytes).to_string(), next))
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
}
