//! Client options: `kafka://` / `kafkas://` URLs.

use kernway_messaging::MsgError;
use std::path::PathBuf;
use std::time::Duration;

/// SASL mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sasl {
    /// PLAIN.
    Plain,
    /// SCRAM-SHA-256.
    ScramSha256,
    /// SCRAM-SHA-512.
    ScramSha512,
}

impl Sasl {
    /// The mechanism name.
    pub fn name(self) -> &'static str {
        match self {
            Sasl::Plain => "PLAIN",
            Sasl::ScramSha256 => "SCRAM-SHA-256",
            Sasl::ScramSha512 => "SCRAM-SHA-512",
        }
    }
}

/// TLS settings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TlsOptions {
    /// CA certificates (PEM); system roots when unset.
    pub ca_file: Option<PathBuf>,
    /// Client certificate (PEM) for mutual TLS.
    pub cert_file: Option<PathBuf>,
    /// Its key.
    pub key_file: Option<PathBuf>,
    /// Do not check the server certificate (testing).
    pub insecure: bool,
}

/// Client options.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// Bootstrap brokers (`host`, `port`).
    pub bootstrap: Vec<(String, u16)>,
    /// Client id (shown in broker logs / quotas).
    pub client_id: String,
    /// SASL mechanism, user and password.
    pub sasl: Option<(Sasl, String, String)>,
    /// TLS (`kafkas://`).
    pub tls: Option<TlsOptions>,
    /// Connect timeout.
    pub connect_timeout: Duration,
    /// Request timeout.
    pub request_timeout: Duration,
    /// Consumer group session timeout (a member silent for longer is removed).
    pub session_timeout: Duration,
    /// Partitions of a topic `ensure_topic` creates.
    pub partitions: i32,
    /// Replication factor of a topic `ensure_topic` creates (-1: the broker's default).
    pub replication: i16,
    /// Idempotent producer (no duplicates from retries).
    pub idempotent: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            bootstrap: vec![("127.0.0.1".into(), 9092)],
            client_id: "kernway".into(),
            sasl: None,
            tls: None,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            session_timeout: Duration::from_secs(10),
            partitions: 3,
            replication: -1,
            idempotent: true,
        }
    }
}

fn bad(m: impl Into<String>) -> MsgError {
    MsgError::Config(format!("Kafka URL: {}", m.into()))
}

fn decode(s: &str) -> Result<String, MsgError> {
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
    String::from_utf8(out).map_err(|_| bad("not UTF-8"))
}

fn tls_mut<'a>(o: &'a mut Options, option: &str) -> Result<&'a mut TlsOptions, MsgError> {
    o.tls.as_mut().ok_or_else(|| bad(format!("{option} needs kafkas://")))
}

impl Options {
    /// Parse `kafka[s]://[user:pass@]host[:port][,host2…][?options]`.
    /// Options: `sasl` (`plain`, `scram-sha-256`, `scram-sha-512`; default
    /// `plain` when credentials are given), `client_id`, `ca`, `cert`,
    /// `key`, `insecure`, `request_timeout` / `session_timeout` /
    /// `connect_timeout` (ms), `partitions`, `replication`, `idempotent`.
    pub fn parse(url: &str) -> Result<Self, MsgError> {
        let (tls, rest) = if let Some(r) = url.strip_prefix("kafkas://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("kafka://") {
            (false, r)
        } else {
            return Err(bad("starts with kafka:// or kafkas://"));
        };
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let rest = rest.trim_end_matches('/');
        let (cred, hosts) = match rest.rsplit_once('@') {
            Some((c, h)) => (Some(c), h),
            None => (None, rest),
        };
        let mut o = Options { bootstrap: Vec::new(), ..Options::default() };
        for h in hosts.split(',').filter(|h| !h.is_empty()) {
            o.bootstrap.push(host_port(h)?);
        }
        if o.bootstrap.is_empty() {
            return Err(bad("no broker"));
        }
        if let Some(c) = cred {
            let (u, p) = c.split_once(':').ok_or_else(|| bad("credentials are user:password"))?;
            o.sasl = Some((Sasl::Plain, decode(u)?, decode(p)?));
        }
        if tls {
            o.tls = Some(TlsOptions::default());
        }
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').ok_or_else(|| bad(format!("option {pair:?} has no value")))?;
            let v = decode(v)?;
            let ms = |v: &str| v.parse::<u64>().map(Duration::from_millis).map_err(|_| bad(format!("{k} must be milliseconds")));
            match k {
                "sasl" => {
                    let m = match v.to_ascii_lowercase().as_str() {
                        "plain" => Sasl::Plain,
                        "scram-sha-256" => Sasl::ScramSha256,
                        "scram-sha-512" => Sasl::ScramSha512,
                        m => return Err(bad(format!("unknown sasl mechanism {m:?}"))),
                    };
                    let (_, u, p) = o.sasl.take().ok_or_else(|| bad("sasl needs user:password@"))?;
                    o.sasl = Some((m, u, p));
                }
                "client_id" => o.client_id = v,
                "ca" => tls_mut(&mut o, k)?.ca_file = Some(v.into()),
                "cert" => tls_mut(&mut o, k)?.cert_file = Some(v.into()),
                "key" => tls_mut(&mut o, k)?.key_file = Some(v.into()),
                "insecure" => tls_mut(&mut o, k)?.insecure = v == "true",
                "request_timeout" => o.request_timeout = ms(&v)?,
                "session_timeout" => o.session_timeout = ms(&v)?,
                "connect_timeout" => o.connect_timeout = ms(&v)?,
                "partitions" => o.partitions = v.parse().ok().filter(|&n: &i32| n > 0).ok_or_else(|| bad("partitions must be positive"))?,
                "replication" => o.replication = v.parse().map_err(|_| bad("replication must be a number"))?,
                "idempotent" => o.idempotent = v == "true",
                _ => return Err(bad(format!("unknown option {k:?}"))),
            }
        }
        if let Some(t) = &o.tls {
            if t.cert_file.is_some() != t.key_file.is_some() {
                return Err(bad("cert and key go together"));
            }
        }
        Ok(o)
    }
}

fn host_port(h: &str) -> Result<(String, u16), MsgError> {
    if let Some(rest) = h.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or_else(|| bad("unclosed ["))?;
        let port = after.strip_prefix(':').map_or(Ok(9092), |p| p.parse().map_err(|_| bad(format!("bad port in {h:?}"))))?;
        return Ok((host.to_string(), port));
    }
    match h.rsplit_once(':') {
        Some((host, p)) => Ok((host.to_string(), p.parse().map_err(|_| bad(format!("bad port in {h:?}")))?)),
        None => Ok((h.to_string(), 9092)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let o = Options::parse("kafka://u:p%40ss@a:9093,b?sasl=scram-sha-512&client_id=pos&partitions=6&replication=3&request_timeout=5000").unwrap();
        assert_eq!(o.bootstrap, [("a".to_string(), 9093), ("b".to_string(), 9092)]);
        assert_eq!(o.sasl, Some((Sasl::ScramSha512, "u".into(), "p@ss".into())));
        assert_eq!((o.client_id.as_str(), o.partitions, o.replication), ("pos", 6, 3));
        assert_eq!(o.request_timeout, Duration::from_secs(5));
        let o = Options::parse("kafkas://[::1]:9094?ca=/ca.pem&insecure=true").unwrap();
        assert_eq!(o.bootstrap, [("::1".to_string(), 9094)]);
        assert!(o.tls.as_ref().is_some_and(|t| t.insecure && t.ca_file.is_some()));
        assert_eq!(Options::parse("kafka://u:p@h").unwrap().sasl.map(|s| s.0), Some(Sasl::Plain));
        for bad in ["http://h", "kafka://", "kafka://h:x", "kafka://h?nope=1", "kafka://h?ca=/x", "kafka://h?sasl=plain", "kafka://u:p@h?sasl=gssapi", "kafka://h?partitions=0", "kafkas://h?cert=/c"] {
            assert!(Options::parse(bad).is_err(), "{bad}");
        }
    }
}
