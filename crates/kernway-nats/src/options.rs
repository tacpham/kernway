//! Connection options: `nats://` / `tls://` URLs, credentials, NKeys.

use kernway_messaging::MsgError;
use std::path::PathBuf;
use std::time::Duration;

/// How to authenticate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Auth {
    /// None.
    #[default]
    None,
    /// User and password.
    UserPassword(String, String),
    /// A token.
    Token(String),
    /// An NKey seed (`SU…`): the server's nonce is signed with it.
    NKey(String),
    /// A JWT and the NKey seed it was issued for (a `.creds` file's content).
    Jwt {
        /// The user JWT.
        jwt: String,
        /// The NKey seed it was issued for.
        seed: String,
    },
}

/// Client options.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// Servers to try, in order (`host:port`); the cluster's own list is
    /// added from the server's `INFO`.
    pub servers: Vec<(String, u16)>,
    /// Authentication.
    pub auth: Auth,
    /// TLS: `None` none (unless the server requires it); `Some` with checks.
    pub tls: Option<TlsOptions>,
    /// Client name (shown in monitoring).
    pub name: Option<String>,
    /// Connect timeout (per server).
    pub connect_timeout: Duration,
    /// Request (JetStream API) timeout.
    pub request_timeout: Duration,
    /// Ping interval (a missed pong marks the connection dead).
    pub ping_interval: Duration,
    /// Keep reconnecting for this long after a disconnect (`None`: forever).
    pub reconnect_for: Option<Duration>,
    /// JetStream domain / API prefix (`$JS.API` by default).
    pub js_prefix: String,
}

/// TLS settings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TlsOptions {
    /// CA certificates (PEM); system roots when unset.
    pub ca_file: Option<PathBuf>,
    /// Client certificate and key (PEM) for mutual TLS.
    pub cert_file: Option<PathBuf>,
    /// The client key.
    pub key_file: Option<PathBuf>,
    /// Do not check the server certificate (testing).
    pub insecure: bool,
    /// TLS before the server's `INFO` (`tls_first` servers).
    pub handshake_first: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            servers: vec![("127.0.0.1".into(), 4222)],
            auth: Auth::None,
            tls: None,
            name: None,
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
            ping_interval: Duration::from_secs(30),
            reconnect_for: Some(Duration::from_secs(120)),
            js_prefix: "$JS.API".into(),
        }
    }
}

fn bad(m: impl Into<String>) -> MsgError {
    MsgError::Config(format!("NATS URL: {}", m.into()))
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
    /// Parse `nats://[user:pass@|token@]host[:port][,host2…][?options]` or
    /// `tls://…` (TLS required). Options: `name`, `tls_ca`, `tls_cert`,
    /// `tls_key`, `tls_insecure`, `tls_first`, `nkey` (seed),
    /// `connect_timeout` / `request_timeout` (seconds), `js_domain`. A
    /// `.creds` file is not a URL option: pass its content to
    /// [`Options::with_creds`].
    pub fn parse(url: &str) -> Result<Self, MsgError> {
        let (tls, rest) = if let Some(r) = url.strip_prefix("tls://") {
            (true, r)
        } else if let Some(r) = url.strip_prefix("nats://") {
            (false, r)
        } else {
            return Err(bad("starts with nats:// or tls://"));
        };
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let rest = rest.trim_end_matches('/');
        let (auth, hosts) = match rest.rsplit_once('@') {
            Some((a, h)) => (Some(a), h),
            None => (None, rest),
        };
        let mut o = Options { servers: Vec::new(), ..Options::default() };
        for h in hosts.split(',').filter(|h| !h.is_empty()) {
            o.servers.push(host_port(h)?);
        }
        if o.servers.is_empty() {
            o.servers.push(("127.0.0.1".into(), 4222));
        }
        if let Some(a) = auth {
            o.auth = match a.split_once(':') {
                Some((u, p)) => Auth::UserPassword(decode(u)?, decode(p)?),
                None => Auth::Token(decode(a)?),
            };
        }
        if tls {
            o.tls = Some(TlsOptions::default());
        }
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').ok_or_else(|| bad(format!("option {pair:?} has no value")))?;
            let v = decode(v)?;
            let secs = |v: &str| v.parse::<u64>().map(Duration::from_secs).map_err(|_| bad(format!("{k} must be seconds")));
            match k {
                "name" => o.name = Some(v),
                "tls_ca" => o.tls.get_or_insert_with(TlsOptions::default).ca_file = Some(v.into()),
                "tls_cert" => o.tls.get_or_insert_with(TlsOptions::default).cert_file = Some(v.into()),
                "tls_key" => o.tls.get_or_insert_with(TlsOptions::default).key_file = Some(v.into()),
                "tls_insecure" => o.tls.get_or_insert_with(TlsOptions::default).insecure = v == "true",
                "tls_first" => o.tls.get_or_insert_with(TlsOptions::default).handshake_first = v == "true",
                "nkey" => o.auth = Auth::NKey(v),
                "connect_timeout" => o.connect_timeout = secs(&v)?,
                "request_timeout" => o.request_timeout = secs(&v)?,
                "js_domain" => o.js_prefix = format!("$JS.{v}.API"),
                _ => return Err(bad(format!("unknown option {k:?}"))),
            }
        }
        if let Some(t) = &o.tls {
            if t.cert_file.is_some() != t.key_file.is_some() {
                return Err(bad("tls_cert and tls_key go together"));
            }
        }
        Ok(o)
    }

    /// Authenticate with a `.creds` file's content (JWT + NKey seed).
    pub fn with_creds(mut self, creds: &str) -> Result<Self, MsgError> {
        let block = |name: &str| -> Option<String> {
            let begin = creds.find(&format!("-----BEGIN {name}"))?;
            let after = &creds[begin..];
            let body_start = after.find('\n')? + 1;
            let end = after.find("------END").or_else(|| after.find("-----END"))?;
            Some(after[body_start..end].trim().to_string())
        };
        let jwt = block("NATS USER JWT").ok_or_else(|| MsgError::Config("creds: no NATS USER JWT block".into()))?;
        let seed = block("USER NKEY SEED").ok_or_else(|| MsgError::Config("creds: no USER NKEY SEED block".into()))?;
        self.auth = Auth::Jwt { jwt, seed };
        Ok(self)
    }
}

fn host_port(h: &str) -> Result<(String, u16), MsgError> {
    if let Some(rest) = h.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or_else(|| bad("unclosed ["))?;
        let port = after.strip_prefix(':').map_or(Ok(4222), |p| p.parse().map_err(|_| bad(format!("bad port in {h:?}"))))?;
        return Ok((host.to_string(), port));
    }
    match h.rsplit_once(':') {
        Some((host, p)) => Ok((host.to_string(), p.parse().map_err(|_| bad(format!("bad port in {h:?}")))?)),
        None => Ok((h.to_string(), 4222)),
    }
}

/// NKeys: base32 seeds and public keys (prefix byte, CRC-16), ed25519 signing.
pub(crate) mod nkey {
    use kernway_messaging::MsgError;

    const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

    fn b32_decode(s: &str) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        let (mut acc, mut bits) = (0u32, 0);
        for c in s.trim_end_matches('=').bytes() {
            let v = B32.iter().position(|&b| b == c)? as u32;
            acc = (acc << 5) | v;
            bits += 5;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        Some(out)
    }

    fn b32_encode(b: &[u8]) -> String {
        let mut out = String::new();
        let (mut acc, mut bits) = (0u32, 0);
        for &x in b {
            acc = (acc << 8) | u32::from(x);
            bits += 8;
            while bits >= 5 {
                bits -= 5;
                out.push(B32[((acc >> bits) & 31) as usize] as char);
            }
        }
        if bits > 0 {
            out.push(B32[((acc << (5 - bits)) & 31) as usize] as char);
        }
        out
    }

    /// CRC-16/XMODEM, as NKeys use.
    fn crc16(data: &[u8]) -> u16 {
        let mut crc: u16 = 0;
        for &b in data {
            crc ^= u16::from(b) << 8;
            for _ in 0..8 {
                crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
            }
        }
        crc
    }

    /// A decoded seed: the ed25519 key pair.
    pub(crate) struct KeyPair {
        pair: ring::signature::Ed25519KeyPair,
        public: String,
    }

    impl KeyPair {
        /// From an `S…` seed.
        pub(crate) fn from_seed(seed: &str) -> Result<Self, MsgError> {
            let bad = || MsgError::Config("not a valid NKey seed".into());
            let raw = b32_decode(seed.trim()).ok_or_else(bad)?;
            if raw.len() != 36 {
                return Err(bad());
            }
            let (body, crc) = raw.split_at(34);
            if crc16(body).to_le_bytes() != [crc[0], crc[1]] {
                return Err(MsgError::Config("NKey seed checksum does not match".into()));
            }
            // Byte 0: seed prefix (S) in the top 5 bits + the key type's top 3;
            // byte 1: the rest of the type.
            if body[0] >> 3 != 18 {
                return Err(MsgError::Config("not a seed (must start with S)".into()));
            }
            let kind = ((body[0] & 7) << 5) | (body[1] >> 3);
            let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&body[2..34]).map_err(|_| bad())?;
            use ring::signature::KeyPair as _;
            let mut pk = vec![kind];
            pk.extend_from_slice(pair.public_key().as_ref());
            let c = crc16(&pk);
            pk.extend_from_slice(&c.to_le_bytes());
            Ok(KeyPair { pair, public: b32_encode(&pk) })
        }

        /// The public key (`U…`, `A…` …).
        pub(crate) fn public_key(&self) -> &str {
            &self.public
        }

        /// Sign the server's nonce: base64url without padding.
        pub(crate) fn sign(&self, nonce: &[u8]) -> String {
            let sig = self.pair.sign(nonce);
            let std = kernway_wire::scram::b64_encode(sig.as_ref());
            std.trim_end_matches('=').replace('+', "-").replace('/', "_")
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn seeds_decode_to_their_public_key() {
            // From the nkeys reference test vectors (nats-io/nkeys).
            let kp = KeyPair::from_seed("SUAMLK2ZNL35WSMW37E7UD4VZ7ELPKW7DHC3BWBSD2GCZ7IUQQXZIORRBU").unwrap();
            assert!(kp.public_key().starts_with('U'), "{}", kp.public_key());
            assert_eq!(kp.public_key().len(), 56);
            assert_eq!(kp.sign(b"nonce").len(), 86);
            assert!(KeyPair::from_seed("SUAMLK2ZNL35WSMW37E7UD4VZ7ELPKW7DHC3BWBSD2GCZ7IUQQXZIORSBU").is_err(), "checksum");
            assert!(KeyPair::from_seed("hello").is_err());
            assert_eq!(crc16(b"123456789"), 0x31C3);
            // The pair in the NATS NKey authentication docs.
            let doc = KeyPair::from_seed("SUACSSL3UAHUDXKFSNVUZRF5UHPMWZ6BFDTJ7M6USDXIEDNPPQYYYCU3VY").unwrap();
            assert_eq!(doc.public_key(), "UDXU4RCSJNZOIQHZNWXHXORDPRTGNJAHAHFRGZNEEJCPQTT2M7NLCNF4");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let o = Options::parse("nats://u:p%40ss@a.example:4223,b.example?name=pos&request_timeout=2").unwrap();
        assert_eq!(o.servers, [("a.example".to_string(), 4223), ("b.example".to_string(), 4222)]);
        assert_eq!(o.auth, Auth::UserPassword("u".into(), "p@ss".into()));
        assert_eq!(o.name.as_deref(), Some("pos"));
        assert_eq!(o.request_timeout, Duration::from_secs(2));
        let o = Options::parse("tls://s3cr3t@[::1]:4443?tls_ca=/ca.pem&tls_first=true").unwrap();
        assert_eq!(o.auth, Auth::Token("s3cr3t".into()));
        let t = o.tls.unwrap();
        assert_eq!((t.ca_file, t.handshake_first), (Some("/ca.pem".into()), true));
        for bad in ["http://x", "nats://h:port", "nats://h/?nope=1", "nats://h/?tls_cert=c.pem"] {
            assert!(Options::parse(bad).is_err(), "{bad}");
        }
        let creds = "-----BEGIN NATS USER JWT-----\neyJ0eXAi.x.y\n------END NATS USER JWT------\n\n-----BEGIN USER NKEY SEED-----\nSUAM\n------END USER NKEY SEED------\n";
        let o = Options::default().with_creds(creds).unwrap();
        assert_eq!(o.auth, Auth::Jwt { jwt: "eyJ0eXAi.x.y".into(), seed: "SUAM".into() });
    }
}
