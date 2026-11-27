//! Connection settings: a `mssql://` URL or an ADO.NET connection string.

use kernway_orm_core::error::OrmError;
use std::path::PathBuf;
use std::time::Duration;

/// How the connection is encrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encrypt {
    /// No TLS at all (the server must allow it: `ENCRYPT_NOT_SUP`).
    NotSupported,
    /// Only the login packet is encrypted (TDS `ENCRYPT_OFF`): the password
    /// never travels in the clear, the rest does.
    LoginOnly,
    /// Everything encrypted (TDS 7.4 `ENCRYPT_ON`; the TLS handshake rides in
    /// PRELOGIN packets).
    On,
    /// TDS 8.0 strict: TLS from the first byte (ALPN `tds/8.0`), like HTTPS.
    Strict,
}

/// Everything needed to open a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MssqlConfig {
    /// Server host name or IP literal.
    pub host: String,
    /// TCP port (1433).
    pub port: u16,
    /// SQL Server login.
    pub user: String,
    /// Password.
    pub password: String,
    /// Initial database (`None`: the login's default).
    pub database: Option<String>,
    /// Encryption.
    pub encrypt: Encrypt,
    /// Accept any server certificate (testing / self-signed servers).
    pub trust_server_certificate: bool,
    /// CA certificate(s) to trust (PEM); system roots when unset.
    pub ca_file: Option<PathBuf>,
    /// The name the certificate must carry (default: `host`).
    pub host_name_in_certificate: Option<String>,
    /// Application name (`APP_NAME()`, `sys.dm_exec_sessions`).
    pub application_name: String,
    /// Requested packet size (512 – 32767; the server may change it).
    pub packet_size: u16,
    /// Connect + login timeout.
    pub connect_timeout: Duration,
    /// Per-statement timeout: the statement is cancelled (ATTENTION) and
    /// the connection stays usable. `None`: no timeout.
    pub command_timeout: Option<Duration>,
}

impl Default for MssqlConfig {
    fn default() -> Self {
        MssqlConfig {
            host: "localhost".into(),
            port: 1433,
            user: String::new(),
            password: String::new(),
            database: None,
            encrypt: Encrypt::On,
            trust_server_certificate: false,
            ca_file: None,
            host_name_in_certificate: None,
            application_name: "kernway".into(),
            packet_size: 4096,
            connect_timeout: Duration::from_secs(15),
            command_timeout: None,
        }
    }
}

fn bad(m: impl Into<String>) -> OrmError {
    OrmError::Connection(format!("SQL Server connection string: {}", m.into()))
}

fn decode(s: &str) -> Result<String, OrmError> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = |c: u8| (c as char).to_digit(16);
            match (b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
                (Some(h), Some(l)) => {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                }
                _ => return Err(bad(format!("bad %-escape in {s:?}"))),
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| bad("not UTF-8 once decoded"))
}

fn boolean(k: &str, v: &str) -> Result<bool, OrmError> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "yes" | "1" => Ok(true),
        "false" | "no" | "0" => Ok(false),
        _ => Err(bad(format!("{k} must be true or false, not {v:?}"))),
    }
}

impl MssqlConfig {
    /// Parse a `mssql://` / `sqlserver://` URL or an ADO.NET connection string.
    ///
    /// URL: `mssql://user:password@host[:port][/database][?options]`.
    /// ADO.NET: `Server=tcp:host,1433;Database=db;User Id=u;Password=p;Encrypt=strict`.
    ///
    /// Options (either form, case-insensitive): `encrypt` (`strict`,
    /// `true` / `mandatory`, `false` / `optional` — login only —, `none`),
    /// `trustServerCertificate`, `caFile`, `hostNameInCertificate`,
    /// `applicationName` / `app`, `packetSize`, `connectTimeout`
    /// (seconds), `commandTimeout` (seconds), `database` / `initial catalog`.
    pub fn parse(s: &str) -> Result<Self, OrmError> {
        let s = s.trim();
        if s.starts_with("mssql://") || s.starts_with("sqlserver://") {
            Self::from_url(s)
        } else {
            Self::from_ado(s)
        }
    }

    fn from_url(url: &str) -> Result<Self, OrmError> {
        let rest = url.split_once("://").map(|x| x.1).unwrap_or_default();
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (auth, rest) = rest.rsplit_once('@').ok_or_else(|| bad("no user (mssql://user:password@host)"))?;
        let (hostport, db) = rest.split_once('/').unwrap_or((rest, ""));
        let mut c = MssqlConfig::default();
        let (u, p) = auth.split_once(':').unwrap_or((auth, ""));
        c.user = decode(u)?;
        c.password = decode(p)?;
        let (h, port) = split_host_port(&decode(hostport)?)?;
        c.host = h;
        if let Some(p) = port {
            c.port = p;
        }
        if !db.is_empty() {
            c.database = Some(decode(db)?);
        }
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').ok_or_else(|| bad(format!("option {pair:?} has no value")))?;
            c.set(k, &decode(v)?)?;
        }
        c.check()
    }

    fn from_ado(s: &str) -> Result<Self, OrmError> {
        let mut c = MssqlConfig::default();
        let mut saw_server = false;
        for part in split_ado(s).iter().map(|p| p.trim()).filter(|p| !p.is_empty()) {
            let (k, v) = part.split_once('=').ok_or_else(|| bad(format!("{part:?} is not key=value")))?;
            let v = unquote(v.trim());
            let v = v.as_str();
            match k.trim().to_ascii_lowercase().as_str() {
                "server" | "data source" | "address" | "addr" | "network address" => {
                    let v = v.strip_prefix("tcp:").unwrap_or(v);
                    if v.contains('\\') {
                        return Err(bad("named instances (host\\instance) need SQL Server Browser; give the instance's port instead (host,port)"));
                    }
                    let (h, p) = match v.split_once(',') {
                        Some((h, p)) => (h.to_string(), Some(p.trim().parse::<u16>().map_err(|_| bad(format!("bad port in {v:?}")))?)),
                        None => (v.to_string(), None),
                    };
                    c.host = if h == "." || h.eq_ignore_ascii_case("(local)") { "localhost".into() } else { h };
                    if let Some(p) = p {
                        c.port = p;
                    }
                    saw_server = true;
                }
                "user id" | "uid" | "user" => c.user = v.to_string(),
                "password" | "pwd" => c.password = v.to_string(),
                "integrated security" | "trusted_connection" => {
                    if boolean(k, v)? {
                        return Err(bad("Windows authentication (Integrated Security) is not supported; use a SQL Server login"));
                    }
                }
                other => c.set(other, v)?,
            }
        }
        if !saw_server {
            return Err(bad("no Server="));
        }
        c.check()
    }

    fn set(&mut self, k: &str, v: &str) -> Result<(), OrmError> {
        match k.to_ascii_lowercase().replace([' ', '_'], "").as_str() {
            "encrypt" => {
                self.encrypt = match v.to_ascii_lowercase().as_str() {
                    "strict" => Encrypt::Strict,
                    "true" | "yes" | "mandatory" | "on" => Encrypt::On,
                    "false" | "no" | "optional" | "off" => Encrypt::LoginOnly,
                    "none" | "notsupported" => Encrypt::NotSupported,
                    _ => return Err(bad(format!("unknown encrypt {v:?} (strict, true, false, none)"))),
                }
            }
            "trustservercertificate" => self.trust_server_certificate = boolean(k, v)?,
            "cafile" | "servercertificate" => self.ca_file = Some(v.into()),
            "hostnameincertificate" => self.host_name_in_certificate = Some(v.to_string()),
            "applicationname" | "app" => self.application_name = v.to_string(),
            "packetsize" => {
                let n: u16 = v.parse().map_err(|_| bad("packetSize must be a number"))?;
                if !(512..=32767).contains(&n) {
                    return Err(bad("packetSize must be 512–32767"));
                }
                self.packet_size = n;
            }
            "connecttimeout" | "connectiontimeout" | "timeout" => {
                self.connect_timeout = Duration::from_secs(v.parse::<u64>().map_err(|_| bad("connectTimeout must be seconds"))?.max(1))
            }
            "commandtimeout" => {
                let s: u64 = v.parse().map_err(|_| bad("commandTimeout must be seconds"))?;
                self.command_timeout = (s > 0).then(|| Duration::from_secs(s));
            }
            "database" | "initialcatalog" => self.database = Some(v.to_string()),
            "multipleactiveresultsets" | "mars" => {
                if boolean(k, v)? {
                    return Err(bad("MultipleActiveResultSets is not supported"));
                }
            }
            "trustedconnection" | "integratedsecurity" => {
                if boolean(k, v)? {
                    return Err(bad("Windows authentication is not supported; use a SQL Server login"));
                }
            }
            other => return Err(bad(format!("unknown option {other:?}"))),
        }
        Ok(())
    }

    fn check(self) -> Result<Self, OrmError> {
        if self.user.is_empty() {
            return Err(bad("no user"));
        }
        if self.user.len() > 128 || self.password.len() > 128 {
            return Err(bad("user and password are at most 128 characters"));
        }
        Ok(self)
    }
}

fn unquote(v: &str) -> String {
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            return v[1..v.len() - 1].replace(&format!("{q}{q}"), &q.to_string());
        }
    }
    v.to_string()
}

/// Split at `;` outside `"…"` / `'…'` quotes (a doubled quote is literal).
fn split_ado(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (None, ';') => out.push(std::mem::take(&mut cur)),
            (None, '"' | '\'') if cur.trim_end().ends_with('=') => {
                quote = Some(c);
                cur.push(c);
            }
            (Some(q), c) if c == q => {
                if chars.peek() == Some(&q) {
                    chars.next();
                    cur.push(q);
                    cur.push(q);
                } else {
                    quote = None;
                    cur.push(c);
                }
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn split_host_port(s: &str) -> Result<(String, Option<u16>), OrmError> {
    let port = |p: &str| p.parse::<u16>().map_err(|_| bad(format!("bad port {p:?}")));
    if let Some(rest) = s.strip_prefix('[') {
        let (h, after) = rest.split_once(']').ok_or_else(|| bad("unclosed ["))?;
        return Ok((h.to_string(), after.strip_prefix(':').map(port).transpose()?));
    }
    match s.rsplit_once(':') {
        Some((h, p)) => Ok((h.to_string(), Some(port(p)?))),
        None if s.is_empty() => Err(bad("no host")),
        None => Ok((s.to_string(), None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_ado_strings() {
        let c = MssqlConfig::parse("mssql://sa:p%40ss@db.example:1444/shop?encrypt=strict&trustServerCertificate=true&applicationName=pos&commandTimeout=30").unwrap();
        assert_eq!((c.host.as_str(), c.port, c.user.as_str(), c.password.as_str()), ("db.example", 1444, "sa", "p@ss"));
        assert_eq!(c.database.as_deref(), Some("shop"));
        assert_eq!(c.encrypt, Encrypt::Strict);
        assert!(c.trust_server_certificate);
        assert_eq!(c.application_name, "pos");
        assert_eq!(c.command_timeout, Some(Duration::from_secs(30)));

        let c = MssqlConfig::parse("Server=tcp:10.0.0.5,1500;Initial Catalog=pos;User ID=app;Password=\"a;b\";Encrypt=false;TrustServerCertificate=yes;Packet Size=8192").unwrap();
        assert_eq!((c.host.as_str(), c.port), ("10.0.0.5", 1500));
        assert_eq!(c.database.as_deref(), Some("pos"));
        assert_eq!(c.encrypt, Encrypt::LoginOnly);
        assert_eq!(c.packet_size, 8192);
        let c = MssqlConfig::parse("Server=.;User Id=sa;Password=x").unwrap();
        assert_eq!(c.host, "localhost");
        assert_eq!(c.encrypt, Encrypt::On, "encrypted by default");
    }

    #[test]
    fn bad_strings_say_why() {
        for bad in [
            "mssql://host/db",
            "mssql://sa:x@host:port",
            "mssql://sa:x@host/?encrypt=maybe",
            "mssql://sa:x@host/?nope=1",
            "Server=h\\SQLEXPRESS;User Id=sa;Password=x",
            "Server=h;Integrated Security=true",
            "Database=x;User Id=sa",
            "Server=h;User Id=sa;MultipleActiveResultSets=true",
            "mssql://sa:x@h/?packetSize=100",
        ] {
            assert!(MssqlConfig::parse(bad).is_err(), "{bad}");
        }
    }
}
