//! How to reach a Redis server: address, TLS, credentials, database,
//! protocol, timeouts — built in code or parsed from a URL.

use crate::error::RedisError;
use kernway_tls::CertCheck;
use std::path::PathBuf;
use std::time::Duration;

/// Where the server listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// A host name or IP literal and a port (names are resolved off the
    /// reactor at connect time).
    Tcp {
        /// Host name, IPv4 or IPv6 literal.
        host: String,
        /// Port.
        port: u16,
    },
    /// A Unix-domain socket file (Unix only; Redis has none on Windows).
    Unix(PathBuf),
    /// Ask Redis Sentinel where the master (or a replica) of a named group
    /// is, then connect there. TLS, credentials, database and timeouts of
    /// these options apply to that data connection.
    Sentinel(SentinelOptions),
}

/// Where the sentinels are and what to ask them for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentinelOptions {
    /// The monitored group's name (`sentinel monitor <name> …`).
    pub master: String,
    /// Sentinel addresses, tried in order.
    pub sentinels: Vec<(String, u16)>,
    /// Connect to a replica instead of the master (reads only).
    pub replica: bool,
    /// ACL user for the sentinels (`None`: default user).
    pub username: Option<String>,
    /// Password for the sentinels (`None`: no authentication).
    pub password: Option<String>,
}

/// The protocol version to speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// RESP2: every server.
    Resp2,
    /// RESP3 (`HELLO 3`, Redis ≥ 6): typed replies (maps, doubles, booleans)
    /// and pushes. Falls back to RESP2 on a server without `HELLO`.
    Resp3,
}

/// TLS settings (`rediss://`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsOptions {
    /// How much of the server certificate to check.
    pub check: CertCheck,
    /// CA bundle (PEM) to trust instead of the public roots.
    pub ca_file: Option<PathBuf>,
    /// Client certificate chain (PEM) for mutual TLS.
    pub cert_file: Option<PathBuf>,
    /// Client private key (PEM) for mutual TLS.
    pub key_file: Option<PathBuf>,
    /// Name to verify the certificate against and send as SNI (default: the host).
    pub server_name: Option<String>,
}

impl Default for TlsOptions {
    fn default() -> Self {
        Self { check: CertCheck::Full, ca_file: None, cert_file: None, key_file: None, server_name: None }
    }
}

/// Everything needed to open and set up a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Where the server is.
    pub endpoint: Endpoint,
    /// TLS, or `None` for plaintext.
    pub tls: Option<TlsOptions>,
    /// ACL user (`None`: the default user).
    pub username: Option<String>,
    /// Password (`None`: no authentication).
    pub password: Option<String>,
    /// Database index selected on connect.
    pub database: u32,
    /// `CLIENT SETNAME`, shown by `CLIENT LIST`.
    pub client_name: Option<String>,
    /// Protocol version.
    pub protocol: Protocol,
    /// Give up opening a connection after this long.
    pub connect_timeout: Duration,
    /// Give up waiting for a reply after this long (`None`: wait). A command
    /// that times out leaves the connection unusable — it is dropped.
    pub command_timeout: Option<Duration>,
}

impl Options {
    /// Plaintext TCP to `host:port`, RESP3, database 0.
    pub fn tcp(host: impl Into<String>, port: u16) -> Self {
        Self {
            endpoint: Endpoint::Tcp { host: host.into(), port },
            tls: None,
            username: None,
            password: None,
            database: 0,
            client_name: None,
            protocol: Protocol::Resp3,
            connect_timeout: Duration::from_secs(10),
            command_timeout: None,
        }
    }

    /// A Unix-domain socket.
    pub fn unix(path: impl Into<PathBuf>) -> Self {
        Self { endpoint: Endpoint::Unix(path.into()), ..Self::tcp("", 0) }
    }

    /// Parse a URL:
    ///
    /// - `redis://[[user]:password@]host[:port][/db][?options]` (port 6379)
    /// - `rediss://…` — the same over TLS
    /// - `redis+unix:///path/to/redis.sock[?db=N&…]` (also `unix://`)
    /// - `redis+sentinel://[[user]:password@]host1[:port1],host2[:port2]/master_name[/db]`
    ///   (sentinel port 26379; `rediss+sentinel://` for TLS) with options
    ///   `replica=true`, `sentinel_username`, `sentinel_password`
    ///
    /// Options: `db`, `protocol=2|3`, `client_name`, `timeout` and
    /// `connect_timeout` (milliseconds), and for TLS `ssl_verify=full|ca|none`,
    /// `ssl_ca`, `ssl_cert`, `ssl_key` (PEM file paths), `ssl_server_name`.
    /// User and password are percent-decoded.
    pub fn from_url(url: &str) -> Result<Self, RedisError> {
        let bad = |m: String| RedisError::Config(format!("redis URL: {m}"));
        let (scheme, rest) = url.split_once("://").ok_or_else(|| bad("missing scheme (redis://, rediss://, redis+unix://)".into()))?;
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let mut o = match scheme {
            "redis" | "rediss" => {
                let (auth, hostpart) = match rest.rsplit_once('@') {
                    Some((a, h)) => (Some(a), h),
                    None => (None, rest),
                };
                let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
                let (host, port) = split_host_port(hostport).map_err(bad)?;
                if host.is_empty() {
                    return Err(bad("missing host".into()));
                }
                let mut o = Self::tcp(host, port.unwrap_or(6379));
                if let Some(auth) = auth {
                    let (user, pass) = auth.split_once(':').unwrap_or(("", auth));
                    if !user.is_empty() {
                        o.username = Some(decode(user).map_err(bad)?);
                    }
                    if !pass.is_empty() {
                        o.password = Some(decode(pass).map_err(bad)?);
                    }
                }
                if !db.is_empty() {
                    o.database = db.parse().map_err(|_| bad(format!("database must be a number, not {db:?}")))?;
                }
                if scheme == "rediss" {
                    o.tls = Some(TlsOptions::default());
                }
                o
            }
            "redis+sentinel" | "rediss+sentinel" => {
                let (auth, rest) = match rest.rsplit_once('@') {
                    Some((a, r)) => (Some(a), r),
                    None => (None, rest),
                };
                let mut parts = rest.split('/');
                let hosts = parts.next().unwrap_or("");
                let master = parts.next().filter(|m| !m.is_empty()).ok_or_else(|| bad("missing master name: redis+sentinel://h:p,h:p/<master>".into()))?;
                let db = parts.next().unwrap_or("");
                let mut sentinels = Vec::new();
                for h in hosts.split(',').filter(|h| !h.is_empty()) {
                    let (host, port) = split_host_port(h).map_err(bad)?;
                    sentinels.push((host, port.unwrap_or(26379)));
                }
                if sentinels.is_empty() {
                    return Err(bad("no sentinel address".into()));
                }
                let mut o = Self::tcp("", 0);
                o.endpoint = Endpoint::Sentinel(SentinelOptions {
                    master: decode(master).map_err(bad)?,
                    sentinels,
                    replica: false,
                    username: None,
                    password: None,
                });
                if let Some(auth) = auth {
                    let (user, pass) = auth.split_once(':').unwrap_or(("", auth));
                    if !user.is_empty() {
                        o.username = Some(decode(user).map_err(bad)?);
                    }
                    if !pass.is_empty() {
                        o.password = Some(decode(pass).map_err(bad)?);
                    }
                }
                if !db.is_empty() {
                    o.database = db.parse().map_err(|_| bad(format!("database must be a number, not {db:?}")))?;
                }
                if scheme == "rediss+sentinel" {
                    o.tls = Some(TlsOptions::default());
                }
                o
            }
            "redis+unix" | "unix" => {
                if rest.is_empty() {
                    return Err(bad("missing socket path".into()));
                }
                Self::unix(decode(rest).map_err(bad)?)
            }
            other => return Err(bad(format!("unknown scheme {other:?}"))),
        };
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').ok_or_else(|| bad(format!("option {pair:?} has no value")))?;
            let v = decode(v).map_err(bad)?;
            let ms = |v: &str| v.parse::<u64>().map(Duration::from_millis).map_err(|_| bad(format!("{k} must be milliseconds, not {v:?}")));
            let tls = |o: &mut Options| -> Result<(), RedisError> {
                if o.tls.is_none() {
                    return Err(bad(format!("{k} needs a rediss:// URL")));
                }
                Ok(())
            };
            match k {
                "db" => o.database = v.parse().map_err(|_| bad(format!("db must be a number, not {v:?}")))?,
                "password" => o.password = Some(v),
                "username" | "user" => o.username = Some(v),
                "protocol" => {
                    o.protocol = match v.as_str() {
                        "2" | "resp2" => Protocol::Resp2,
                        "3" | "resp3" => Protocol::Resp3,
                        _ => return Err(bad(format!("protocol must be 2 or 3, not {v:?}"))),
                    }
                }
                "client_name" => o.client_name = Some(v),
                "timeout" => o.command_timeout = Some(ms(&v)?),
                "connect_timeout" => o.connect_timeout = ms(&v)?,
                "replica" | "sentinel_username" | "sentinel_password" => {
                    let Endpoint::Sentinel(sen) = &mut o.endpoint else {
                        return Err(bad(format!("{k} needs a redis+sentinel:// URL")));
                    };
                    match k {
                        "replica" => sen.replica = matches!(v.as_str(), "true" | "1" | "yes"),
                        "sentinel_username" => sen.username = Some(v),
                        _ => sen.password = Some(v),
                    }
                }
                "ssl_verify" => {
                    tls(&mut o)?;
                    let check = match v.as_str() {
                        "full" => CertCheck::Full,
                        "ca" => CertCheck::CaOnly,
                        "none" => CertCheck::None,
                        _ => return Err(bad(format!("ssl_verify must be full, ca or none, not {v:?}"))),
                    };
                    o.tls.as_mut().expect("checked").check = check;
                }
                "ssl_ca" => {
                    tls(&mut o)?;
                    o.tls.as_mut().expect("checked").ca_file = Some(v.into());
                }
                "ssl_cert" => {
                    tls(&mut o)?;
                    o.tls.as_mut().expect("checked").cert_file = Some(v.into());
                }
                "ssl_key" => {
                    tls(&mut o)?;
                    o.tls.as_mut().expect("checked").key_file = Some(v.into());
                }
                "ssl_server_name" => {
                    tls(&mut o)?;
                    o.tls.as_mut().expect("checked").server_name = Some(v);
                }
                _ => return Err(bad(format!("unknown option {k:?}"))),
            }
        }
        if let Some(t) = &o.tls {
            if t.cert_file.is_some() != t.key_file.is_some() {
                return Err(bad("ssl_cert and ssl_key go together".into()));
            }
        }
        Ok(o)
    }

    /// Use TLS with these settings.
    pub fn tls(mut self, tls: TlsOptions) -> Self {
        self.tls = Some(tls);
        self
    }

    /// Authenticate as `user` (`None`: default user) with `password`.
    pub fn auth(mut self, user: Option<&str>, password: &str) -> Self {
        self.username = user.map(str::to_string);
        self.password = Some(password.to_string());
        self
    }

    /// Select database `db` on connect.
    pub fn database(mut self, db: u32) -> Self {
        self.database = db;
        self
    }

    /// Protocol version.
    pub fn protocol(mut self, p: Protocol) -> Self {
        self.protocol = p;
        self
    }

    /// Reply timeout.
    pub fn command_timeout(mut self, t: Duration) -> Self {
        self.command_timeout = Some(t);
        self
    }

    /// Connect timeout.
    pub fn connect_timeout(mut self, t: Duration) -> Self {
        self.connect_timeout = t;
        self
    }

    /// `CLIENT SETNAME` on connect.
    pub fn client_name(mut self, name: &str) -> Self {
        self.client_name = Some(name.to_string());
        self
    }

    /// `host:port` for messages (or the socket path).
    pub(crate) fn describe(&self) -> String {
        match &self.endpoint {
            Endpoint::Tcp { host, port } => format!("{host}:{port}"),
            Endpoint::Unix(p) => p.display().to_string(),
            Endpoint::Sentinel(s) => format!("sentinel master {}", s.master),
        }
    }
}

impl From<crate::pubsub::RedisConfig> for Options {
    fn from(c: crate::pubsub::RedisConfig) -> Self {
        let mut o = Options::tcp(c.addr.ip().to_string(), c.addr.port());
        o.username = c.user;
        o.password = c.password;
        o
    }
}

/// `host`, `host:port`, `[v6]`, `[v6]:port`.
fn split_host_port(s: &str) -> Result<(String, Option<u16>), String> {
    let port = |p: &str| p.parse::<u16>().map_err(|_| format!("bad port {p:?}"));
    if let Some(rest) = s.strip_prefix('[') {
        let (h, after) = rest.split_once(']').ok_or("unclosed [ in host")?;
        return match after.strip_prefix(':') {
            Some(p) => Ok((h.to_string(), Some(port(p)?))),
            None if after.is_empty() => Ok((h.to_string(), None)),
            None => Err(format!("unexpected {after:?} after host")),
        };
    }
    match s.rsplit_once(':') {
        Some((h, p)) => Ok((h.to_string(), Some(port(p)?))),
        None => Ok((s.to_string(), None)),
    }
}

/// Percent-decode (UTF-8); a malformed escape is an error rather than a
/// silently different password.
fn decode(s: &str) -> Result<String, String> {
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
                _ => return Err(format!("bad %-escape in {s:?}")),
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| format!("{s:?} is not UTF-8 once decoded"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_urls() {
        let o = Options::from_url("redis://localhost").unwrap();
        assert_eq!(o.endpoint, Endpoint::Tcp { host: "localhost".into(), port: 6379 });
        assert_eq!((o.database, o.tls.is_none(), o.protocol), (0, true, Protocol::Resp3));
        let o = Options::from_url("redis://:s%40cret@10.0.0.5:6380/3").unwrap();
        assert_eq!(o.endpoint, Endpoint::Tcp { host: "10.0.0.5".into(), port: 6380 });
        assert_eq!((o.username, o.password.as_deref(), o.database), (None, Some("s@cret"), 3));
        let o = Options::from_url("redis://app:pw@[::1]:7000/1?protocol=2&timeout=250&connect_timeout=1500&client_name=pos").unwrap();
        assert_eq!(o.endpoint, Endpoint::Tcp { host: "::1".into(), port: 7000 });
        assert_eq!((o.username.as_deref(), o.password.as_deref()), (Some("app"), Some("pw")));
        assert_eq!(o.protocol, Protocol::Resp2);
        assert_eq!(o.command_timeout, Some(Duration::from_millis(250)));
        assert_eq!(o.connect_timeout, Duration::from_millis(1500));
        assert_eq!(o.client_name.as_deref(), Some("pos"));
        // A password with @ and : inside, unescaped in the user part split.
        let o = Options::from_url("redis://u:p%3Aw%40d@h").unwrap();
        assert_eq!(o.password.as_deref(), Some("p:w@d"));
    }

    #[test]
    fn sentinel_urls() {
        let o = Options::from_url("redis+sentinel://:pw@s1:26390,s2,[::1]:5/mymaster/2?replica=true&sentinel_password=sp&sentinel_username=su").unwrap();
        let Endpoint::Sentinel(s) = &o.endpoint else { panic!("{o:?}") };
        assert_eq!(s.master, "mymaster");
        assert_eq!(s.sentinels, [("s1".to_string(), 26390), ("s2".to_string(), 26379), ("::1".to_string(), 5)]);
        assert!(s.replica);
        assert_eq!((s.username.as_deref(), s.password.as_deref()), (Some("su"), Some("sp")));
        assert_eq!((o.password.as_deref(), o.database), (Some("pw"), 2));
        assert!(Options::from_url("rediss+sentinel://s1/m").unwrap().tls.is_some());
        assert!(Options::from_url("redis+sentinel://s1").is_err(), "master name required");
        assert!(Options::from_url("redis://h?replica=true").is_err());
    }

    #[test]
    fn tls_and_unix_urls() {
        let o = Options::from_url("rediss://h:6390?ssl_verify=ca&ssl_ca=/etc/ca.pem&ssl_cert=c.pem&ssl_key=k.pem&ssl_server_name=redis.internal").unwrap();
        let t = o.tls.unwrap();
        assert_eq!(t.check, CertCheck::CaOnly);
        assert_eq!(t.ca_file, Some("/etc/ca.pem".into()));
        assert_eq!((t.cert_file.is_some(), t.key_file.is_some()), (true, true));
        assert_eq!(t.server_name.as_deref(), Some("redis.internal"));
        assert_eq!(Options::from_url("rediss://h").unwrap().tls.unwrap().check, CertCheck::Full);
        let o = Options::from_url("redis+unix:///var/run/redis.sock?db=2&password=x").unwrap();
        assert_eq!(o.endpoint, Endpoint::Unix("/var/run/redis.sock".into()));
        assert_eq!((o.database, o.password.as_deref()), (2, Some("x")));
        assert!(matches!(Options::from_url("unix:///tmp/r.sock").unwrap().endpoint, Endpoint::Unix(_)));
    }

    #[test]
    fn bad_urls_say_why() {
        for bad in [
            "localhost:6379",
            "http://h",
            "redis://",
            "redis://h:port",
            "redis://h/x",
            "redis://h?db=x",
            "redis://h?nope=1",
            "redis://h?ssl_verify=full",
            "rediss://h?ssl_cert=c.pem",
            "redis://h?protocol=4",
            "redis://u:%zz@h",
            "redis://[::1",
        ] {
            assert!(matches!(Options::from_url(bad), Err(RedisError::Config(_))), "{bad}");
        }
    }
}
