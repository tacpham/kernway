//! Connection options: `amqp://` / `amqps://` URIs (the RabbitMQ URI spec).

use kernway_messaging::MsgError;
use std::path::PathBuf;
use std::time::Duration;

/// SASL mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mechanism {
    /// `PLAIN`: user and password.
    #[default]
    Plain,
    /// `AMQPLAIN`: user and password in a field table (old brokers).
    AmqPlain,
    /// `EXTERNAL`: the TLS client certificate is the identity.
    External,
}

/// TLS settings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TlsOptions {
    /// CA certificates (PEM); system roots when unset.
    pub ca_file: Option<PathBuf>,
    /// Client certificate (PEM) for mutual TLS / EXTERNAL.
    pub cert_file: Option<PathBuf>,
    /// Its key.
    pub key_file: Option<PathBuf>,
    /// Do not check the server certificate (testing).
    pub insecure: bool,
    /// The name to check the certificate against (default: the host).
    pub server_name: Option<String>,
}

/// Client options.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// Servers to try in order (`host`, `port`).
    pub servers: Vec<(String, u16)>,
    /// User.
    pub user: String,
    /// Password.
    pub password: String,
    /// Virtual host.
    pub vhost: String,
    /// SASL mechanism.
    pub mechanism: Mechanism,
    /// TLS (`amqps://`).
    pub tls: Option<TlsOptions>,
    /// Heartbeat interval asked for (0: none); the server may lower it.
    pub heartbeat: Duration,
    /// Largest frame asked for (bytes; the server may lower it).
    pub frame_max: u32,
    /// Most channels asked for.
    pub channel_max: u16,
    /// Connect + handshake timeout.
    pub connect_timeout: Duration,
    /// Synchronous method / confirm timeout.
    pub request_timeout: Duration,
    /// Connection name (shown in the management UI).
    pub name: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            servers: vec![("127.0.0.1".into(), 5672)],
            user: "guest".into(),
            password: "guest".into(),
            vhost: "/".into(),
            mechanism: Mechanism::Plain,
            tls: None,
            heartbeat: Duration::from_secs(60),
            frame_max: 131_072,
            channel_max: 2047,
            connect_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(10),
            name: None,
        }
    }
}

fn bad(m: impl Into<String>) -> MsgError {
    MsgError::Config(format!("AMQP URI: {}", m.into()))
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

impl Options {
    /// Parse `amqp[s]://[user[:pass]@]host[:port][,host2[:port]…][/vhost][?query]`.
    ///
    /// The vhost is the percent-decoded path (`/%2F` is `/`; an empty path
    /// means `/`). Query: `heartbeat` (s), `frame_max`, `channel_max`,
    /// `connection_timeout` (ms), `request_timeout` (s), `auth_mechanism`
    /// (`plain`, `amqplain`, `external`), `cacertfile`, `certfile`,
    /// `keyfile`, `verify` (`verify_none` / `verify_peer`),
    /// `server_name_indication`, `name`.
    pub fn parse(uri: &str) -> Result<Self, MsgError> {
        let (tls, rest) = if let Some(r) = uri.strip_prefix("amqps://") {
            (true, r)
        } else if let Some(r) = uri.strip_prefix("amqp://") {
            (false, r)
        } else {
            return Err(bad("starts with amqp:// or amqps://"));
        };
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], Some(&rest[i + 1..])),
            None => (rest, None),
        };
        let (cred, hosts) = match authority.rsplit_once('@') {
            Some((c, h)) => (Some(c), h),
            None => (None, authority),
        };
        let default_port = if tls { 5671 } else { 5672 };
        let mut o = Options { servers: Vec::new(), ..Options::default() };
        for h in hosts.split(',').filter(|h| !h.is_empty()) {
            o.servers.push(host_port(h, default_port)?);
        }
        if o.servers.is_empty() {
            o.servers.push(("127.0.0.1".into(), default_port));
        }
        if let Some(c) = cred {
            let (u, p) = c.split_once(':').unwrap_or((c, ""));
            o.user = decode(u)?;
            o.password = decode(p)?;
        }
        o.vhost = match path {
            None | Some("") => "/".into(),
            Some(p) => decode(p)?,
        };
        if tls {
            o.tls = Some(TlsOptions::default());
        }
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').ok_or_else(|| bad(format!("option {pair:?} has no value")))?;
            let v = decode(v)?;
            let num = |v: &str| v.parse::<u64>().map_err(|_| bad(format!("{k} must be a number")));
            match k {
                "heartbeat" => o.heartbeat = Duration::from_secs(num(&v)?),
                "frame_max" => o.frame_max = u32::try_from(num(&v)?).map_err(|_| bad("frame_max too large"))?,
                "channel_max" => o.channel_max = u16::try_from(num(&v)?).map_err(|_| bad("channel_max too large"))?,
                "connection_timeout" => o.connect_timeout = Duration::from_millis(num(&v)?),
                "request_timeout" => o.request_timeout = Duration::from_secs(num(&v)?),
                "name" => o.name = Some(v),
                "auth_mechanism" => {
                    o.mechanism = match v.to_ascii_lowercase().as_str() {
                        "plain" => Mechanism::Plain,
                        "amqplain" => Mechanism::AmqPlain,
                        "external" => Mechanism::External,
                        m => return Err(bad(format!("unknown auth_mechanism {m:?}"))),
                    }
                }
                "cacertfile" => tls_mut(&mut o, k)?.ca_file = Some(v.into()),
                "certfile" => tls_mut(&mut o, k)?.cert_file = Some(v.into()),
                "keyfile" => tls_mut(&mut o, k)?.key_file = Some(v.into()),
                "verify" => {
                    let insecure = match v.as_str() {
                        "verify_none" => true,
                        "verify_peer" => false,
                        _ => return Err(bad("verify is verify_none or verify_peer")),
                    };
                    tls_mut(&mut o, k)?.insecure = insecure;
                }
                "server_name_indication" => tls_mut(&mut o, k)?.server_name = Some(v),
                _ => return Err(bad(format!("unknown option {k:?}"))),
            }
        }
        if let Some(t) = &o.tls {
            if t.cert_file.is_some() != t.key_file.is_some() {
                return Err(bad("certfile and keyfile go together"));
            }
        }
        if o.mechanism == Mechanism::External && o.tls.as_ref().is_none_or(|t| t.cert_file.is_none()) {
            return Err(bad("auth_mechanism=external needs a client certificate (certfile / keyfile)"));
        }
        Ok(o)
    }
}

/// The TLS settings a TLS-only option changes (an `amqp://` URI has none).
fn tls_mut<'a>(o: &'a mut Options, option: &str) -> Result<&'a mut TlsOptions, MsgError> {
    o.tls.as_mut().ok_or_else(|| bad(format!("{option} needs amqps://")))
}

fn host_port(h: &str, default: u16) -> Result<(String, u16), MsgError> {
    if let Some(rest) = h.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or_else(|| bad("unclosed ["))?;
        let port = after.strip_prefix(':').map_or(Ok(default), |p| p.parse().map_err(|_| bad(format!("bad port in {h:?}"))))?;
        return Ok((host.to_string(), port));
    }
    match h.rsplit_once(':') {
        Some((host, p)) => Ok((decode(host)?, p.parse().map_err(|_| bad(format!("bad port in {h:?}")))?)),
        None => Ok((decode(h)?, default)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris() {
        let o = Options::parse("amqp://u:p%40ss@rmq.example:5673/pos?heartbeat=10&frame_max=65536&name=kw").unwrap();
        assert_eq!(o.servers, [("rmq.example".to_string(), 5673)]);
        assert_eq!((o.user.as_str(), o.password.as_str(), o.vhost.as_str()), ("u", "p@ss", "pos"));
        assert_eq!((o.heartbeat, o.frame_max, o.name.as_deref()), (Duration::from_secs(10), 65536, Some("kw")));
        assert_eq!(Options::parse("amqp://h").unwrap().vhost, "/");
        assert_eq!(Options::parse("amqp://h/").unwrap().vhost, "/");
        assert_eq!(Options::parse("amqp://h/%2F").unwrap().vhost, "/");
        assert_eq!(Options::parse("amqp://h/a%2Fb").unwrap().vhost, "a/b");
        let o = Options::parse("amqps://[::1],b?cacertfile=/ca.pem&certfile=/c.pem&keyfile=/k.pem&auth_mechanism=external").unwrap();
        assert_eq!(o.servers, [("::1".to_string(), 5671), ("b".to_string(), 5671)]);
        assert_eq!(o.mechanism, Mechanism::External);
        assert_eq!(o.tls.unwrap().ca_file, Some("/ca.pem".into()));
        for bad in [
            "http://h",
            "amqp://h:x",
            "amqp://h?nope=1",
            "amqp://h?cacertfile=/ca.pem",
            "amqps://h?certfile=/c.pem",
            "amqps://h?auth_mechanism=external",
            "amqp://h?auth_mechanism=scram",
            "amqp://h?frame_max=99999999999",
        ] {
            assert!(Options::parse(bad).is_err(), "{bad}");
        }
    }
}
