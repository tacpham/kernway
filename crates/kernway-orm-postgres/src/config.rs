//! Connection settings, parsed from a libpq-style URL.

use kernway_orm_core::error::OrmError;
use std::path::PathBuf;
use std::time::Duration;

/// `sslmode`: whether to use TLS and how much of the server to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    /// Never TLS.
    Disable,
    /// TLS if the server offers it, without checking its certificate; else plaintext.
    Prefer,
    /// TLS required; the certificate is checked only if a root certificate
    /// is given (then as `VerifyCa`), as libpq does.
    Require,
    /// TLS, the certificate must chain to a trusted root.
    VerifyCa,
    /// TLS, trusted root and the host name must match.
    VerifyFull,
}

/// `channel_binding`: tie SCRAM to the TLS session (`SCRAM-SHA-256-PLUS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelBinding {
    /// Never.
    Disable,
    /// When TLS is used and the server offers it (the default).
    Prefer,
    /// Refuse to authenticate without it.
    Require,
}

/// `target_session_attrs`: which kind of server among the hosts is wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetSession {
    /// The first host that accepts.
    Any,
    /// A host taking writes (not read-only, not a standby).
    ReadWrite,
    /// A read-only host.
    ReadOnly,
    /// A primary (not in recovery).
    Primary,
    /// A standby (in recovery).
    Standby,
    /// A standby if one is reachable, else any host.
    PreferStandby,
}

/// One place to connect to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Host {
    /// TCP: name or IP literal, and port.
    Tcp(String, u16),
    /// The Unix-domain socket `<dir>/.s.PGSQL.<port>`.
    Unix(PathBuf, u16),
}

/// Everything needed to open a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgConfig {
    /// Hosts, tried in order.
    pub hosts: Vec<Host>,
    /// Role to log in as.
    pub user: String,
    /// Password (`None`: none sent; trust / cert authentication).
    pub password: Option<String>,
    /// Database (default: the user name).
    pub database: String,
    /// `application_name`, shown in `pg_stat_activity`.
    pub application_name: Option<String>,
    /// Extra server settings sent at start-up (`options=-c a=b`, or
    /// individual `key=value` URL parameters libpq passes through).
    pub settings: Vec<(String, String)>,
    /// TLS mode.
    pub ssl_mode: SslMode,
    /// Root certificate(s) to trust (PEM file).
    pub ssl_root_cert: Option<PathBuf>,
    /// Client certificate (PEM) for certificate authentication / mutual TLS.
    pub ssl_cert: Option<PathBuf>,
    /// Client key (PEM).
    pub ssl_key: Option<PathBuf>,
    /// Channel binding.
    pub channel_binding: ChannelBinding,
    /// Which host to settle on.
    pub target_session_attrs: TargetSession,
    /// Per-host connect timeout.
    pub connect_timeout: Duration,
    /// Prepared statements kept per connection (0: none, unnamed only).
    pub statement_cache: usize,
}

impl PgConfig {
    /// Parse `postgres://` / `postgresql://` URLs:
    ///
    /// `postgres://[user[:password]@][host[:port][,host[:port]…]][/database][?options]`
    ///
    /// Options: `sslmode` (disable | allow | prefer | require | verify-ca |
    /// verify-full), `sslrootcert`, `sslcert`, `sslkey`, `channel_binding`
    /// (disable | prefer | require), `target_session_attrs` (any | read-write |
    /// read-only | primary | standby | prefer-standby), `application_name`,
    /// `connect_timeout` (seconds), `options` (`-c name=value …`), `host`
    /// (a Unix-socket directory, or more hosts), `port`, `user`, `password`,
    /// `dbname`, `statement_cache_size`. Any other `key=value` is sent to the
    /// server as a setting (`search_path=app`). Percent-escapes are decoded.
    pub fn from_url(url: &str) -> Result<Self, OrmError> {
        let bad = |m: String| OrmError::Connection(format!("postgres URL: {m}"));
        let rest = url
            .strip_prefix("postgres://")
            .or_else(|| url.strip_prefix("postgresql://"))
            .ok_or_else(|| bad("must start with postgres:// or postgresql://".into()))?;
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (auth, rest) = match rest.rsplit_once('@') {
            Some((a, r)) => (Some(a), r),
            None => (None, rest),
        };
        let (hostlist, db) = rest.split_once('/').unwrap_or((rest, ""));
        let mut cfg = PgConfig {
            hosts: Vec::new(),
            user: String::new(),
            password: None,
            database: decode(db).map_err(bad)?,
            application_name: None,
            settings: Vec::new(),
            ssl_mode: SslMode::Prefer,
            ssl_root_cert: None,
            ssl_cert: None,
            ssl_key: None,
            channel_binding: ChannelBinding::Prefer,
            target_session_attrs: TargetSession::Any,
            connect_timeout: Duration::from_secs(10),
            statement_cache: 100,
        };
        if let Some(a) = auth {
            let (u, p) = match a.split_once(':') {
                Some((u, p)) => (u, Some(p)),
                None => (a, None),
            };
            cfg.user = decode(u).map_err(bad)?;
            cfg.password = p.map(decode).transpose().map_err(bad)?;
        }
        let mut host_specs: Vec<(String, Option<u16>)> = Vec::new();
        for h in hostlist.split(',').filter(|h| !h.is_empty()) {
            host_specs.push(split_host_port(&decode(h).map_err(bad)?).map_err(bad)?);
        }
        let mut port_override: Option<Vec<u16>> = None;
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').ok_or_else(|| bad(format!("option {pair:?} has no value")))?;
            let v = decode(v).map_err(bad)?;
            match k {
                "sslmode" => {
                    cfg.ssl_mode = match v.as_str() {
                        "disable" => SslMode::Disable,
                        // `allow` tries plaintext first; here it behaves as prefer.
                        "allow" | "prefer" => SslMode::Prefer,
                        "require" => SslMode::Require,
                        "verify-ca" => SslMode::VerifyCa,
                        "verify-full" => SslMode::VerifyFull,
                        _ => return Err(bad(format!("unknown sslmode {v:?}"))),
                    }
                }
                "sslrootcert" => cfg.ssl_root_cert = Some(v.into()),
                "sslcert" => cfg.ssl_cert = Some(v.into()),
                "sslkey" => cfg.ssl_key = Some(v.into()),
                "channel_binding" => {
                    cfg.channel_binding = match v.as_str() {
                        "disable" => ChannelBinding::Disable,
                        "prefer" => ChannelBinding::Prefer,
                        "require" => ChannelBinding::Require,
                        _ => return Err(bad(format!("unknown channel_binding {v:?}"))),
                    }
                }
                "target_session_attrs" => {
                    cfg.target_session_attrs = match v.as_str() {
                        "any" => TargetSession::Any,
                        "read-write" => TargetSession::ReadWrite,
                        "read-only" => TargetSession::ReadOnly,
                        "primary" => TargetSession::Primary,
                        "standby" => TargetSession::Standby,
                        "prefer-standby" => TargetSession::PreferStandby,
                        _ => return Err(bad(format!("unknown target_session_attrs {v:?}"))),
                    }
                }
                "application_name" => cfg.application_name = Some(v),
                "connect_timeout" => {
                    let s: u64 = v.parse().map_err(|_| bad(format!("connect_timeout must be seconds, not {v:?}")))?;
                    cfg.connect_timeout = Duration::from_secs(s.max(1));
                }
                "statement_cache_size" => {
                    cfg.statement_cache = v.parse().map_err(|_| bad(format!("statement_cache_size must be a number, not {v:?}")))?
                }
                "options" => {
                    // `-c name=value -c other=value`
                    let mut it = v.split_whitespace();
                    while let Some(tok) = it.next() {
                        let setting = if tok == "-c" { it.next().unwrap_or("") } else { tok.strip_prefix("-c").unwrap_or(tok) };
                        let (n, val) = setting.split_once('=').ok_or_else(|| bad(format!("options: {setting:?} is not name=value")))?;
                        cfg.settings.push((n.to_string(), val.to_string()));
                    }
                }
                "host" => {
                    for h in v.split(',').filter(|h| !h.is_empty()) {
                        host_specs.push(split_host_port(h).map_err(bad)?);
                    }
                }
                "port" => {
                    let ports: Result<Vec<u16>, _> = v.split(',').map(str::parse).collect();
                    port_override = Some(ports.map_err(|_| bad(format!("bad port {v:?}")))?);
                }
                "user" => cfg.user = v,
                "password" => cfg.password = Some(v),
                "dbname" => cfg.database = v,
                _ => cfg.settings.push((k.to_string(), v)),
            }
        }
        if host_specs.is_empty() {
            host_specs.push(("localhost".into(), None));
        }
        for (i, (h, p)) in host_specs.into_iter().enumerate() {
            let port = p
                .or_else(|| port_override.as_ref().and_then(|ps| ps.get(i).or(ps.first()).copied()))
                .unwrap_or(5432);
            cfg.hosts.push(if h.starts_with('/') { Host::Unix(h.into(), port) } else { Host::Tcp(h, port) });
        }
        if cfg.user.is_empty() {
            return Err(bad("no user".into()));
        }
        if cfg.database.is_empty() {
            cfg.database = cfg.user.clone();
        }
        if cfg.ssl_cert.is_some() != cfg.ssl_key.is_some() {
            return Err(bad("sslcert and sslkey go together".into()));
        }
        Ok(cfg)
    }
}

fn split_host_port(s: &str) -> Result<(String, Option<u16>), String> {
    let port = |p: &str| p.parse::<u16>().map_err(|_| format!("bad port {p:?}"));
    if s.starts_with('/') {
        return Ok((s.to_string(), None));
    }
    if let Some(rest) = s.strip_prefix('[') {
        let (h, after) = rest.split_once(']').ok_or("unclosed [ in host")?;
        return match after.strip_prefix(':') {
            Some(p) => Ok((h.to_string(), Some(port(p)?))),
            None => Ok((h.to_string(), None)),
        };
    }
    match s.rsplit_once(':') {
        Some((h, p)) => Ok((h.to_string(), Some(port(p)?))),
        None => Ok((s.to_string(), None)),
    }
}

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
    fn urls() {
        let c = PgConfig::from_url("postgres://app:p%40ss@db.example:6543/shop?sslmode=verify-full&sslrootcert=/ca.pem&application_name=pos&connect_timeout=3&search_path=app").unwrap();
        assert_eq!(c.hosts, [Host::Tcp("db.example".into(), 6543)]);
        assert_eq!((c.user.as_str(), c.password.as_deref(), c.database.as_str()), ("app", Some("p@ss"), "shop"));
        assert_eq!(c.ssl_mode, SslMode::VerifyFull);
        assert_eq!(c.ssl_root_cert, Some("/ca.pem".into()));
        assert_eq!(c.application_name.as_deref(), Some("pos"));
        assert_eq!(c.connect_timeout, Duration::from_secs(3));
        assert_eq!(c.settings, [("search_path".to_string(), "app".to_string())]);

        let c = PgConfig::from_url("postgresql://u@h1:5432,[::1]:5433,h3/db?target_session_attrs=read-write&channel_binding=require&options=-c%20statement_timeout%3D5s").unwrap();
        assert_eq!(c.hosts, [Host::Tcp("h1".into(), 5432), Host::Tcp("::1".into(), 5433), Host::Tcp("h3".into(), 5432)]);
        assert_eq!(c.target_session_attrs, TargetSession::ReadWrite);
        assert_eq!(c.channel_binding, ChannelBinding::Require);
        assert_eq!(c.settings, [("statement_timeout".to_string(), "5s".to_string())]);
        assert_eq!(c.password, None);

        // Unix socket directory via host=, default database = user.
        let c = PgConfig::from_url("postgres://kw@/?host=/var/run/postgresql&port=5433").unwrap();
        assert_eq!(c.hosts, [Host::Unix("/var/run/postgresql".into(), 5433)]);
        assert_eq!(c.database, "kw");
        assert_eq!(c.ssl_mode, SslMode::Prefer);
    }

    #[test]
    fn bad_urls() {
        for bad in [
            "mysql://u@h/db",
            "postgres://h/db",
            "postgres://u@h:port/db",
            "postgres://u@h/db?sslmode=maybe",
            "postgres://u@h/db?sslcert=c.pem",
            "postgres://u@h/db?connect_timeout=soon",
            "postgres://u@h/db?options=-c%20nope",
            "postgres://u:%zz@h/db",
        ] {
            assert!(PgConfig::from_url(bad).is_err(), "{bad}");
        }
    }
}
