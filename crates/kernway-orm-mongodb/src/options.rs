//! Client options, parsed from a MongoDB connection string
//! (<https://www.mongodb.com/docs/manual/reference/connection-string/>).

use crate::bson::{Bson, Document};
use crate::error::{Error, Result};
use std::path::PathBuf;
use std::time::Duration;

/// A server address.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Address {
    /// Host name or IP literal, and port.
    Tcp(String, u16),
    /// A Unix-domain socket file (`/tmp/mongodb-27017.sock`).
    Unix(PathBuf),
}

impl Address {
    /// Parse `host[:port]`, `[v6]:port`, or a `.sock` path (not
    /// percent-encoded). Errors do not repeat the text.
    pub fn parse(s: &str) -> Result<Self> {
        parse_decoded_host(s, "host")
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Address::Tcp(h, p) if h.contains(':') => write!(f, "[{h}]:{p}"),
            Address::Tcp(h, p) => write!(f, "{h}:{p}"),
            Address::Unix(p) => write!(f, "{}", p.display()),
        }
    }
}

/// Authentication mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMechanism {
    /// SCRAM-SHA-1.
    ScramSha1,
    /// SCRAM-SHA-256.
    ScramSha256,
    /// X.509 client certificate (`$external`).
    X509,
    /// SASL PLAIN (LDAP proxy, `$external`).
    Plain,
}

impl AuthMechanism {
    /// The mechanism's canonical name (`SCRAM-SHA-256`, …).
    pub fn name(self) -> &'static str {
        match self {
            AuthMechanism::ScramSha1 => "SCRAM-SHA-1",
            AuthMechanism::ScramSha256 => "SCRAM-SHA-256",
            AuthMechanism::X509 => "MONGODB-X509",
            AuthMechanism::Plain => "PLAIN",
        }
    }
}

/// Credentials. `Debug` shows `<redacted>` for the user name and password.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    /// User name (for X.509 optional: taken from the certificate).
    pub username: Option<String>,
    /// Password.
    pub password: Option<String>,
    /// Database the user is defined in (`authSource`).
    pub source: String,
    /// Mechanism; `None` negotiates SCRAM-SHA-256 or -SHA-1 with the server.
    pub mechanism: Option<AuthMechanism>,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const R: &str = "<redacted>";
        f.debug_struct("Credential")
            .field("username", &self.username.as_ref().map(|_| R))
            .field("password", &self.password.as_ref().map(|_| R))
            .field("source", &self.source)
            .field("mechanism", &self.mechanism)
            .finish()
    }
}

/// Read preference mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadMode {
    /// The primary only.
    Primary,
    /// The primary, else a secondary.
    PrimaryPreferred,
    /// Secondaries only.
    Secondary,
    /// A secondary, else the primary.
    SecondaryPreferred,
    /// Lowest latency, any member.
    Nearest,
}

/// Where reads go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPreference {
    /// The mode.
    pub mode: ReadMode,
    /// Tag sets, tried in order (`readPreferenceTags`); empty = any.
    pub tag_sets: Vec<Vec<(String, String)>>,
    /// Skip secondaries more stale than this (≥ 90 s).
    pub max_staleness: Option<Duration>,
}

impl ReadPreference {
    /// Primary only.
    pub fn primary() -> Self {
        ReadPreference { mode: ReadMode::Primary, tag_sets: Vec::new(), max_staleness: None }
    }

    /// With `mode`, no tags.
    pub fn new(mode: ReadMode) -> Self {
        ReadPreference { mode, tag_sets: Vec::new(), max_staleness: None }
    }

    /// `$readPreference` for a mongos.
    pub fn to_document(&self) -> Document {
        let mode = match self.mode {
            ReadMode::Primary => "primary",
            ReadMode::PrimaryPreferred => "primaryPreferred",
            ReadMode::Secondary => "secondary",
            ReadMode::SecondaryPreferred => "secondaryPreferred",
            ReadMode::Nearest => "nearest",
        };
        let mut d = Document::new().with("mode", mode);
        if !self.tag_sets.is_empty() {
            let sets: Vec<Bson> = self
                .tag_sets
                .iter()
                .map(|set| Bson::Document(set.iter().map(|(k, v)| (k.clone(), Bson::String(v.clone()))).collect()))
                .collect();
            d.insert("tags", Bson::Array(sets));
        }
        if let Some(s) = self.max_staleness {
            d.insert("maxStalenessSeconds", s.as_secs() as i64);
        }
        d
    }
}

/// Write acknowledgement.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WriteConcern {
    /// `w`: a number of members, or a tag set / `"majority"`.
    pub w: Option<Bson>,
    /// `j`: wait for the journal.
    pub journal: Option<bool>,
    /// `wtimeout`.
    pub timeout: Option<Duration>,
}

impl WriteConcern {
    /// `{w: "majority"}`.
    pub fn majority() -> Self {
        WriteConcern { w: Some(Bson::String("majority".into())), ..Default::default() }
    }

    /// Whether nothing is set (the server's default applies).
    pub fn is_default(&self) -> bool {
        self.w.is_none() && self.journal.is_none() && self.timeout.is_none()
    }

    /// Unacknowledged (`w: 0`).
    pub fn is_unacknowledged(&self) -> bool {
        matches!(self.w, Some(Bson::Int32(0)) | Some(Bson::Int64(0))) && self.journal != Some(true)
    }

    /// The `writeConcern` document.
    pub fn to_document(&self) -> Document {
        let mut d = Document::new();
        if let Some(w) = &self.w {
            d.insert("w", w.clone());
        }
        if let Some(j) = self.journal {
            d.insert("j", j);
        }
        if let Some(t) = self.timeout {
            d.insert("wtimeout", t.as_millis() as i64);
        }
        d
    }
}

/// Read isolation (`readConcern.level`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadConcern(pub String);

impl ReadConcern {
    /// `majority`.
    pub fn majority() -> Self {
        ReadConcern("majority".into())
    }
    /// `local`.
    pub fn local() -> Self {
        ReadConcern("local".into())
    }
    /// `snapshot`.
    pub fn snapshot() -> Self {
        ReadConcern("snapshot".into())
    }
}

/// Wire compression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compressor {
    /// Snappy (id 1).
    Snappy,
    /// zlib (id 2), with a level 0–9 (-1: default).
    Zlib(i32),
}

impl Compressor {
    /// The `hello.compression` name.
    pub fn name(self) -> &'static str {
        match self {
            Compressor::Snappy => "snappy",
            Compressor::Zlib(_) => "zlib",
        }
    }
}

/// TLS settings.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TlsOptions {
    /// CA certificates to trust (PEM); system roots when unset.
    pub ca_file: Option<PathBuf>,
    /// Client certificate and key, both in one PEM file.
    pub cert_key_file: Option<PathBuf>,
    /// Accept any certificate (testing only).
    pub allow_invalid_certificates: bool,
    /// Accept a certificate for another name.
    pub allow_invalid_hostnames: bool,
}

/// Everything a client needs.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientOptions {
    /// Seed list.
    pub hosts: Vec<Address>,
    /// From `mongodb+srv://`: the SRV name (hosts are resolved from it).
    pub srv: Option<String>,
    /// `srvMaxHosts`.
    pub srv_max_hosts: usize,
    /// `srvServiceName`.
    pub srv_service_name: String,
    /// Default database (the URL path).
    pub default_database: Option<String>,
    /// Credentials.
    pub credential: Option<Credential>,
    /// TLS, when enabled.
    pub tls: Option<TlsOptions>,
    /// `replicaSet`: the expected set name.
    pub replica_set: Option<String>,
    /// `directConnection`: talk to the one host as is.
    pub direct_connection: Option<bool>,
    /// `loadBalanced`.
    pub load_balanced: bool,
    /// `appName`.
    pub app_name: Option<String>,
    /// Read preference.
    pub read_preference: ReadPreference,
    /// Read concern.
    pub read_concern: Option<ReadConcern>,
    /// Write concern.
    pub write_concern: WriteConcern,
    /// `retryWrites` (default true).
    pub retry_writes: bool,
    /// `retryReads` (default true).
    pub retry_reads: bool,
    /// `compressors`, in preference order.
    pub compressors: Vec<Compressor>,
    /// `connectTimeoutMS` (default 10 s).
    pub connect_timeout: Duration,
    /// `socketTimeoutMS` (`None`: no timeout).
    pub socket_timeout: Option<Duration>,
    /// `serverSelectionTimeoutMS` (default 30 s).
    pub server_selection_timeout: Duration,
    /// `localThresholdMS` (default 15 ms).
    pub local_threshold: Duration,
    /// `heartbeatFrequencyMS` (default 10 s, at least 500 ms).
    pub heartbeat_frequency: Duration,
    /// `maxPoolSize` per server and shard (default 10).
    pub max_pool_size: usize,
    /// `zlibCompressionLevel`.
    pub zlib_level: i32,
    /// What parsing the connection string warned about (an unknown option, a
    /// value of the wrong type, a repeated key…), per the Connection String
    /// and URI Options specs: the option is ignored and parsing goes on. The
    /// client logs each one when it is built.
    pub warnings: Vec<String>,
}

impl Default for ClientOptions {
    fn default() -> Self {
        ClientOptions {
            hosts: vec![Address::Tcp("localhost".into(), 27017)],
            srv: None,
            srv_max_hosts: 0,
            srv_service_name: "mongodb".into(),
            default_database: None,
            credential: None,
            tls: None,
            replica_set: None,
            direct_connection: None,
            load_balanced: false,
            app_name: None,
            read_preference: ReadPreference::primary(),
            read_concern: None,
            write_concern: WriteConcern::default(),
            retry_writes: true,
            retry_reads: true,
            compressors: Vec::new(),
            connect_timeout: Duration::from_secs(10),
            socket_timeout: None,
            server_selection_timeout: Duration::from_secs(30),
            local_threshold: Duration::from_millis(15),
            heartbeat_frequency: Duration::from_secs(10),
            max_pool_size: 10,
            zlib_level: -1,
            warnings: Vec::new(),
        }
    }
}

/// Every URI option the Connection String / URI Options specs define, in its
/// canonical spelling (matched case-insensitively).
const KNOWN_OPTIONS: &[&str] = &[
    "appname", "authMechanism", "authMechanismProperties", "authSource", "compressors", "connectTimeoutMS", "directConnection",
    "enableOverloadRetargeting", "heartbeatFrequencyMS", "journal", "loadBalanced", "localThresholdMS", "maxAdaptiveRetries",
    "maxConnecting", "maxIdleTimeMS", "maxPoolSize", "maxStalenessSeconds", "minPoolSize", "proxyHost", "proxyPassword", "proxyPort",
    "proxyUsername", "readConcernLevel", "readPreference", "readPreferenceTags", "replicaSet", "retryReads", "retryWrites",
    "serverMonitoringMode", "serverSelectionTimeoutMS", "serverSelectionTryOnce", "socketTimeoutMS", "srvAllowedHostsSuffix",
    "srvMaxHosts", "srvServiceName", "ssl", "timeoutMS", "tls", "tlsAllowInvalidCertificates", "tlsAllowInvalidHostnames", "tlsCAFile",
    "tlsCertificateKeyFile", "tlsCertificateKeyFilePassword", "tlsDisableCertificateRevocationCheck", "tlsDisableOCSPEndpointCheck",
    "tlsInsecure", "w", "waitQueueTimeoutMS", "wTimeoutMS", "zlibCompressionLevel",
];

/// Renamed options still accepted under their old name: (old, new).
const DEPRECATED_OPTIONS: &[(&str, &str)] = &[("wtimeout", "wTimeoutMS")];

/// Options whose argument must never appear in a message (URI Options spec,
/// "Sensitive options").
const SENSITIVE_OPTIONS: &[&str] = &["proxyUsername", "proxyPassword", "tlsCertificateKeyFilePassword"];

fn canonical_option(lower: &str) -> Option<&'static str> {
    KNOWN_OPTIONS.iter().copied().find(|k| k.eq_ignore_ascii_case(lower))
}

/// Percent-decoding. Errors name only the component (`what`), never the text:
/// the Connection String spec forbids repeating any part of a URI that did not parse.
fn decode_part(s: &str, what: &str) -> Result<String> {
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
                _ => return Err(Error::config(format!("the {what} holds a malformed percent-escape"))),
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| Error::config(format!("the {what} is not UTF-8 once percent-decoded")))
}

/// One host identifier, still percent-encoded as written in the URI.
fn parse_host(raw: &str, nth: usize) -> Result<Address> {
    let what = format!("host identifier #{nth}");
    if raw.is_empty() {
        return Err(Error::config(format!("{what}: empty")));
    }
    parse_decoded_host(&decode_part(raw, &what)?, &what)
}

fn parse_decoded_host(s: &str, what: &str) -> Result<Address> {
    let bad = |why: &str| Error::config(format!("{what}: {why}"));
    if s.is_empty() {
        return Err(bad("empty"));
    }
    // A socket path is percent-encoded (it holds '/') and ends in ".sock".
    if s.ends_with(".sock") && s.contains('/') {
        return Ok(Address::Unix(PathBuf::from(s.to_string())));
    }
    let port = |p: &str| -> Result<u16> {
        if p.is_empty() || !p.bytes().all(|c| c.is_ascii_digit()) {
            return Err(bad("the port is not a number"));
        }
        match p.parse::<u32>() {
            Ok(n) if (1..=65535).contains(&n) => Ok(n as u16),
            _ => Err(bad("the port is outside 1-65535")),
        }
    };
    if let Some(rest) = s.strip_prefix('[') {
        let (h, after) = rest.split_once(']').ok_or_else(|| bad("an IP literal lacks its closing ']'"))?;
        if h.is_empty() || !h.contains(':') {
            return Err(bad("not an IPv6 literal"));
        }
        let p = match after.strip_prefix(':') {
            Some(p) => port(p)?,
            None if after.is_empty() => 27017,
            None => return Err(bad("unexpected text after the IP literal")),
        };
        return Ok(Address::Tcp(h.to_ascii_lowercase(), p));
    }
    let (h, p) = match s.split_once(':') {
        Some((h, p)) => (h, port(p)?),
        None => (s, 27017),
    };
    if h.is_empty() {
        return Err(bad("no host before the port"));
    }
    if h.contains(['/', '@', '?', '#', '[', ']', ' ']) {
        return Err(bad("holds a character a host name cannot"));
    }
    Ok(Address::Tcp(h.to_ascii_lowercase(), p))
}

/// The option warnings and their wording: canonical names only, never text
/// from the URI (Connection String spec, "Reporting an unsupported key or
/// invalid value"); a parsed integer may be shown.
struct Warn<'a>(&'a mut Vec<String>);

impl Warn<'_> {
    fn invalid(&mut self, name: &str, expected: &str) {
        self.0.push(format!("ignored the invalid value of option '{name}' (expected {expected})"));
    }
    fn out_of_range(&mut self, name: &str, n: i64, expected: &str) {
        self.0.push(format!("ignored option '{name}'={n} (expected {expected})"));
    }
}

impl ClientOptions {
    /// URI options the specs define that this driver does not implement: each
    /// is accepted, warned about and ignored, as the Connection String spec
    /// asks for unsupported keys.
    pub const UNSUPPORTED_OPTIONS: &'static [&'static str] = &[
        "authMechanismProperties",
        "enableOverloadRetargeting",
        "maxAdaptiveRetries",
        "maxConnecting",
        "maxIdleTimeMS",
        "minPoolSize",
        "proxyHost",
        "proxyPassword",
        "proxyPort",
        "proxyUsername",
        "serverMonitoringMode",
        "serverSelectionTryOnce",
        "srvAllowedHostsSuffix",
        "timeoutMS",
        "tlsCertificateKeyFilePassword",
        "tlsDisableCertificateRevocationCheck",
        "tlsDisableOCSPEndpointCheck",
        "waitQueueTimeoutMS",
    ];

    /// Authentication mechanisms the auth spec defines that this driver does
    /// not implement: naming one is an error.
    pub const UNSUPPORTED_MECHANISMS: &'static [&'static str] = &["GSSAPI", "MONGODB-AWS", "MONGODB-OIDC"];

    /// Parse `mongodb://[user:pass@]host1[:port1][,host2…][/[db]][?options]`
    /// or `mongodb+srv://[user:pass@]host[/[db]][?options]`, as the MongoDB
    /// Connection String and URI Options specifications define.
    ///
    /// - Option names are case-insensitive. An unknown option, an option this
    ///   driver does not implement ([`Self::UNSUPPORTED_OPTIONS`]), an invalid
    ///   value, an empty value, a repeated option and a deprecated name are
    ///   each ignored with a warning in [`ClientOptions::warnings`].
    /// - Conflicting TLS options, `directConnection=true` with several hosts
    ///   or SRV, SRV-only options without SRV, and malformed URIs are errors.
    /// - No error or warning repeats text from the URI (user, password, host,
    ///   option names or values), so they are safe to log.
    /// - `connectTimeoutMS=0` means no connect timeout, `maxPoolSize=0` no limit.
    pub fn parse(uri: &str) -> Result<Self> {
        let (srv, rest) = if let Some(r) = uri.strip_prefix("mongodb+srv://") {
            (true, r)
        } else if let Some(r) = uri.strip_prefix("mongodb://") {
            (false, r)
        } else {
            return Err(Error::config("a connection string starts with mongodb:// or mongodb+srv://"));
        };
        let (rest, query) = match rest.split_once('?') {
            Some((r, q)) => (r, Some(q)),
            None => (rest, None),
        };
        let (auth, rest) = match rest.rsplit_once('@') {
            Some((a, r)) => (Some(a), r),
            None => (None, rest),
        };
        let (hostlist, db) = match rest.split_once('/') {
            Some((h, d)) => (h, Some(d)),
            None => (rest, None),
        };
        if query.is_some() && db.is_none() && !srv && hostlist.contains('?') {
            return Err(Error::config("options must follow a '/' after the hosts"));
        }
        let mut o = ClientOptions::default();
        if hostlist.is_empty() {
            return Err(Error::config("no host identifier"));
        }
        if srv {
            if hostlist.contains(',') {
                return Err(Error::config("mongodb+srv:// takes exactly one host name"));
            }
            if hostlist.contains(':') {
                return Err(Error::config("mongodb+srv:// takes a host name without a port"));
            }
            let name = decode_part(hostlist, "SRV host name")?;
            if name.split('.').filter(|p| !p.is_empty()).count() < 2 {
                return Err(Error::config("an SRV host name needs at least two dot-separated parts"));
            }
            o.srv = Some(name.to_ascii_lowercase());
            o.hosts.clear();
            o.tls = Some(TlsOptions::default());
        } else {
            o.hosts = hostlist.split(',').enumerate().map(|(i, h)| parse_host(h, i + 1)).collect::<Result<_>>()?;
        }
        if let Some(db) = db.filter(|d| !d.is_empty()) {
            let db = decode_part(db, "auth database")?;
            if db.contains(['/', '\\', ' ', '"', '$']) {
                return Err(Error::config("the auth database holds a character a database name cannot (/ \\ space \" $)"));
            }
            o.default_database = Some(db);
        }
        let mut cred = match auth {
            None => None,
            Some(a) => {
                let (u, p) = match a.split_once(':') {
                    Some((u, p)) => (u, Some(p)),
                    None => (a, None),
                };
                if a.contains('@') || p.is_some_and(|p| p.contains(':')) || u.contains('/') || p.is_some_and(|p| p.contains('/')) {
                    return Err(Error::config("the user name and password must be percent-encoded (@ : / are reserved)"));
                }
                Some(Credential {
                    username: Some(decode_part(u, "user name")?),
                    password: p.map(|p| decode_part(p, "password")).transpose()?,
                    source: o.default_database.clone().unwrap_or_else(|| "admin".into()),
                    mechanism: None,
                })
            }
        };

        // Collect the options: key (canonical, or unknown) → values in order.
        let mut warn = Warn(&mut o.warnings);
        let mut given: Vec<(&'static str, String)> = Vec::new();
        let mut offset = 0usize;
        for pair in query.unwrap_or("").split('&') {
            let at = offset;
            offset += pair.len() + 1;
            if pair.is_empty() {
                continue;
            }
            let Some((k, v)) = pair.split_once('=') else {
                return Err(Error::config(format!("the option at offset {at} in the query segment has no '='")));
            };
            let lower = k.to_ascii_lowercase();
            let name = match canonical_option(&lower) {
                Some(n) => n,
                None => match DEPRECATED_OPTIONS.iter().find(|(old, _)| old.eq_ignore_ascii_case(&lower)) {
                    Some((old, new)) => {
                        warn.0.push(format!("option '{old}' is deprecated; use '{new}'"));
                        // Applied only when its replacement is absent (checked below).
                        given.push((old, decode_part(v, "option value")?));
                        continue;
                    }
                    None => {
                        warn.0.push(format!("ignored an unknown option at offset {at} in the query segment"));
                        continue;
                    }
                },
            };
            let v = match decode_part(v, "option value") {
                Ok(v) => v,
                Err(_) if SENSITIVE_OPTIONS.contains(&name) => return Err(Error::config(format!("option '{name}' holds a malformed value"))),
                Err(e) => return Err(e),
            };
            given.push((name, v));
        }
        let present = |n: &str| given.iter().any(|(k, _)| *k == n);
        // Spec-mandated errors for conflicting TLS options (URI Options spec).
        for (a, b) in [
            ("tlsInsecure", "tlsAllowInvalidCertificates"),
            ("tlsInsecure", "tlsAllowInvalidHostnames"),
            ("tlsInsecure", "tlsDisableOCSPEndpointCheck"),
            ("tlsInsecure", "tlsDisableCertificateRevocationCheck"),
            ("tlsAllowInvalidCertificates", "tlsDisableOCSPEndpointCheck"),
            ("tlsAllowInvalidCertificates", "tlsDisableCertificateRevocationCheck"),
            ("tlsDisableOCSPEndpointCheck", "tlsDisableCertificateRevocationCheck"),
        ] {
            if present(a) && present(b) {
                return Err(Error::config(format!("options '{a}' and '{b}' cannot both be given")));
            }
        }
        {
            let tls_values: Vec<&str> = given.iter().filter(|(k, _)| *k == "tls" || *k == "ssl").map(|(_, v)| v.as_str()).collect();
            if tls_values.windows(2).any(|w| w[0] != w[1]) {
                return Err(Error::config("options 'tls' and 'ssl' must all have the same value"));
            }
        }
        // A repeated key that is not a list: warn; the last one wins.
        let mut seen: Vec<&str> = Vec::new();
        for (k, _) in &given {
            if *k != "readPreferenceTags" && seen.contains(k) {
                warn.0.push(format!("option '{k}' is repeated; the last value is used"));
            }
            seen.push(k);
        }
        let replaced: Vec<&str> = DEPRECATED_OPTIONS.iter().filter(|(_, new)| present(new)).map(|(old, _)| *old).collect();

        let mut auth_source = None;
        let mut mechanism = None;
        let mut tls_on: Option<bool> = if srv { Some(true) } else { None };
        let mut tls = TlsOptions::default();
        let mut tags: Vec<Vec<(String, String)>> = Vec::new();
        let mut read_mode = None;
        let mut max_staleness = None;
        let mut zlib_level = None;
        for (name, v) in &given {
            let name: &str = name;
            if replaced.contains(&name) {
                warn.0.push(format!("option '{name}' is ignored: its replacement is given"));
                continue;
            }
            if ClientOptions::UNSUPPORTED_OPTIONS.contains(&name) {
                warn.0.push(format!("ignored option '{name}': this driver does not implement it"));
                continue;
            }
            // Typed values; an empty one is ignored (Connection String spec, "Values").
            let int = |warn: &mut Warn<'_>, min: i64, max: i64, expected: &str| -> Option<i64> {
                if v.is_empty() {
                    warn.0.push(format!("ignored option '{name}': empty value"));
                    return None;
                }
                match v.parse::<i64>() {
                    Ok(n) if (min..=max).contains(&n) => Some(n),
                    Ok(n) => {
                        warn.out_of_range(name, n, expected);
                        None
                    }
                    Err(_) => {
                        warn.invalid(name, expected);
                        None
                    }
                }
            };
            let boolean = |warn: &mut Warn<'_>| -> Option<bool> {
                match v.as_str() {
                    "true" => Some(true),
                    "false" => Some(false),
                    "" => {
                        warn.0.push(format!("ignored option '{name}': empty value"));
                        None
                    }
                    _ => {
                        warn.invalid(name, "true or false");
                        None
                    }
                }
            };
            let millis = |warn: &mut Warn<'_>| int(warn, 0, i32::MAX as i64, "a non-negative number of milliseconds").map(|n| Duration::from_millis(n as u64));
            match name {
                "authSource" => {
                    if v.is_empty() {
                        return Err(Error::config("option 'authSource' is empty"));
                    }
                    auth_source = Some(v.clone())
                }
                "authMechanism" => {
                    let upper = v.to_ascii_uppercase();
                    mechanism = Some(match upper.as_str() {
                        "SCRAM-SHA-1" => AuthMechanism::ScramSha1,
                        "SCRAM-SHA-256" => AuthMechanism::ScramSha256,
                        "MONGODB-X509" => AuthMechanism::X509,
                        "PLAIN" => AuthMechanism::Plain,
                        m => {
                            return Err(match ClientOptions::UNSUPPORTED_MECHANISMS.iter().find(|x| **x == m) {
                                Some(known) => Error::config(format!("authMechanism {known} is not supported by this driver (SCRAM-SHA-256, SCRAM-SHA-1, MONGODB-X509, PLAIN are)")),
                                None => Error::config("option 'authMechanism' names an unknown mechanism"),
                            })
                        }
                    })
                }
                "tls" | "ssl" => {
                    if let Some(on) = boolean(&mut warn) {
                        tls_on = Some(on);
                    }
                }
                "tlsCAFile" => tls.ca_file = Some(v.into()),
                "tlsCertificateKeyFile" => tls.cert_key_file = Some(v.into()),
                "tlsInsecure" => {
                    if let Some(b) = boolean(&mut warn) {
                        tls.allow_invalid_certificates = b;
                        tls.allow_invalid_hostnames = b;
                    }
                }
                "tlsAllowInvalidCertificates" => {
                    if let Some(b) = boolean(&mut warn) {
                        tls.allow_invalid_certificates = b;
                    }
                }
                "tlsAllowInvalidHostnames" => {
                    if let Some(b) = boolean(&mut warn) {
                        tls.allow_invalid_hostnames = b;
                    }
                }
                "replicaSet" => o.replica_set = Some(v.clone()),
                "directConnection" => {
                    if let Some(b) = boolean(&mut warn) {
                        o.direct_connection = Some(b);
                    }
                }
                "loadBalanced" => {
                    if let Some(b) = boolean(&mut warn) {
                        o.load_balanced = b;
                    }
                }
                "appname" => {
                    if v.len() > 128 {
                        warn.invalid(name, "at most 128 bytes");
                    } else {
                        o.app_name = Some(v.clone());
                    }
                }
                "readPreference" => {
                    read_mode = Some(match v.as_str() {
                        "primary" => ReadMode::Primary,
                        "primaryPreferred" => ReadMode::PrimaryPreferred,
                        "secondary" => ReadMode::Secondary,
                        "secondaryPreferred" => ReadMode::SecondaryPreferred,
                        "nearest" => ReadMode::Nearest,
                        _ => return Err(Error::config("option 'readPreference' names an unknown mode")),
                    })
                }
                "readPreferenceTags" => {
                    let set: Option<Vec<(String, String)>> =
                        v.split(',').filter(|t| !t.is_empty()).map(|t| t.split_once(':').map(|(a, b)| (a.to_string(), b.to_string()))).collect();
                    match set {
                        Some(s) => tags.push(s),
                        None => warn.invalid(name, "comma-separated key:value pairs"),
                    }
                }
                "maxStalenessSeconds" => {
                    if let Some(s) = int(&mut warn, -1, i32::MAX as i64, "-1 or a number of seconds") {
                        max_staleness = Some(s);
                    }
                }
                "readConcernLevel" => o.read_concern = Some(ReadConcern(v.clone())),
                "w" => {
                    if v.is_empty() {
                        warn.0.push("ignored option 'w': empty value".into());
                    } else {
                        o.write_concern.w = Some(match v.parse::<i64>() {
                            Ok(n) if (0..=i32::MAX as i64).contains(&n) => Bson::Int32(n as i32),
                            Ok(n) => {
                                warn.out_of_range(name, n, "a non-negative number or a tag set name");
                                continue;
                            }
                            Err(_) => Bson::String(v.clone()),
                        })
                    }
                }
                "journal" => {
                    if let Some(b) = boolean(&mut warn) {
                        o.write_concern.journal = Some(b);
                    }
                }
                "wTimeoutMS" | "wtimeout" => {
                    if let Some(n) = int(&mut warn, 0, i64::MAX, "a non-negative number of milliseconds") {
                        o.write_concern.timeout = Some(Duration::from_millis(n as u64));
                    }
                }
                "retryWrites" => {
                    if let Some(b) = boolean(&mut warn) {
                        o.retry_writes = b;
                    }
                }
                "retryReads" => {
                    if let Some(b) = boolean(&mut warn) {
                        o.retry_reads = b;
                    }
                }
                "compressors" => {
                    o.compressors.clear();
                    for c in v.split(',') {
                        match c {
                            "snappy" => o.compressors.push(Compressor::Snappy),
                            "zlib" => o.compressors.push(Compressor::Zlib(-1)),
                            // zstd and unknown ones are skipped, as the compression spec asks.
                            _ => warn.0.push("ignored a compressor this driver does not implement".into()),
                        }
                    }
                }
                "zlibCompressionLevel" => zlib_level = int(&mut warn, -1, 9, "-1 to 9"),
                "connectTimeoutMS" => {
                    if let Some(d) = millis(&mut warn) {
                        o.connect_timeout = d;
                    }
                }
                "socketTimeoutMS" => {
                    if let Some(d) = millis(&mut warn) {
                        o.socket_timeout = (!d.is_zero()).then_some(d);
                    }
                }
                "serverSelectionTimeoutMS" => {
                    if let Some(n) = int(&mut warn, 1, i32::MAX as i64, "a positive number of milliseconds") {
                        o.server_selection_timeout = Duration::from_millis(n as u64);
                    }
                }
                "localThresholdMS" => {
                    if let Some(d) = millis(&mut warn) {
                        o.local_threshold = d;
                    }
                }
                "heartbeatFrequencyMS" => {
                    if let Some(n) = int(&mut warn, 500, i32::MAX as i64, "at least 500 milliseconds") {
                        o.heartbeat_frequency = Duration::from_millis(n as u64);
                    }
                }
                "maxPoolSize" => {
                    if let Some(n) = int(&mut warn, 0, i32::MAX as i64, "a non-negative number (0: no limit)") {
                        o.max_pool_size = n as usize;
                    }
                }
                "srvMaxHosts" | "srvServiceName" if !srv => {
                    return Err(Error::config(format!("option '{name}' applies only to mongodb+srv://")));
                }
                "srvMaxHosts" => {
                    if let Some(n) = int(&mut warn, 0, i32::MAX as i64, "a non-negative number (0: no limit)") {
                        o.srv_max_hosts = n as usize;
                    }
                }
                "srvServiceName" => {
                    let ok = !v.is_empty() && v.len() <= 15 && v.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-');
                    if ok {
                        o.srv_service_name = v.clone();
                    } else {
                        warn.invalid(name, "an RFC 6335 service name");
                    }
                }
                _ => warn.0.push(format!("ignored option '{name}': this driver does not implement it")),
            }
        }
        if let Some(l) = zlib_level {
            o.zlib_level = l as i32;
        }
        for c in &mut o.compressors {
            if let Compressor::Zlib(l) = c {
                *l = o.zlib_level;
            }
        }
        if let Some(m) = read_mode {
            o.read_preference.mode = m;
        }
        o.read_preference.tag_sets = tags;
        // -1 (or absent) is no limit; the max-staleness spec makes 1..89 an error.
        o.read_preference.max_staleness = match max_staleness {
            Some(s) if (0..90).contains(&s) => return Err(Error::config(format!("option 'maxStalenessSeconds'={s} is below the minimum of 90"))),
            Some(s) if s >= 90 => Some(Duration::from_secs(s as u64)),
            _ => None,
        };
        if o.read_preference.mode == ReadMode::Primary && (!o.read_preference.tag_sets.is_empty() || o.read_preference.max_staleness.is_some()) {
            return Err(Error::config("readPreferenceTags / maxStalenessSeconds do not apply to readPreference=primary"));
        }
        // TLS on when asked for, or — unless tls/ssl says otherwise — when any
        // other tls* option is given (the URI Options spec leaves this to drivers).
        let implied = given.iter().any(|(k, _)| k.starts_with("tls") && *k != "tls");
        let wants_tls = tls_on.unwrap_or(implied);
        o.tls = wants_tls.then_some(tls);
        // Credentials and their source.
        if let Some(m) = mechanism {
            match m {
                AuthMechanism::X509 => {
                    if cred.as_ref().is_some_and(|c| c.password.is_some()) {
                        return Err(Error::config("MONGODB-X509 takes no password"));
                    }
                    if auth_source.as_deref().is_some_and(|s| s != "$external") {
                        return Err(Error::config("MONGODB-X509 users live in $external"));
                    }
                    let c = cred.get_or_insert(Credential { username: None, password: None, source: "$external".into(), mechanism: None });
                    c.source = "$external".into();
                }
                AuthMechanism::Plain => {
                    let c = cred.as_mut().ok_or_else(|| Error::config("PLAIN needs a user name"))?;
                    // SASL PLAIN separates its fields with NUL (RFC 4616).
                    if c.username.as_deref().is_some_and(|u| u.contains('\0')) || c.password.as_deref().is_some_and(|p| p.contains('\0')) {
                        return Err(Error::config("PLAIN user names and passwords cannot hold a NUL character"));
                    }
                    if c.source == "admin" && auth_source.is_none() {
                        c.source = "$external".into();
                    }
                }
                _ => {
                    if cred.is_none() {
                        return Err(Error::config(format!("{} needs a user name", m.name())));
                    }
                }
            }
            if let Some(c) = &mut cred {
                c.mechanism = Some(m);
            }
        }
        if let (Some(c), Some(src)) = (&mut cred, auth_source) {
            c.source = src;
        }
        if let Some(c) = &cred {
            if c.mechanism != Some(AuthMechanism::X509) && c.username.as_deref().is_none_or(str::is_empty) {
                return Err(Error::config("an empty user name"));
            }
        }
        o.credential = cred;
        if o.direct_connection == Some(true) {
            if o.srv.is_some() {
                return Err(Error::config("directConnection=true is not allowed with mongodb+srv://"));
            }
            if o.hosts.len() > 1 {
                return Err(Error::config("directConnection=true takes exactly one host"));
            }
        }
        if o.load_balanced && (o.hosts.len() > 1 || o.replica_set.is_some() || o.direct_connection == Some(true)) {
            return Err(Error::config("loadBalanced=true takes one host, no replicaSet, no directConnection"));
        }
        if o.srv_max_hosts > 0 && (o.replica_set.is_some() || o.load_balanced) {
            return Err(Error::config("srvMaxHosts cannot be combined with replicaSet or loadBalanced=true"));
        }
        Ok(o)
    }


    /// For `mongodb+srv://`: replace the seed list with the SRV records
    /// targets and merge the TXT record's options (the client does this when
    /// it connects; exposed for inspection and tests).
    pub async fn resolve_srv(&mut self) -> Result<()> {
        if let Some(name) = self.srv.clone() {
            crate::dns::apply_srv(self, &name).await?;
        }
        Ok(())
    }

    /// The default database, or `None`.
    pub fn database(&self) -> Option<&str> {
        self.default_database.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_strings() {
        let o = ClientOptions::parse(
            "mongodb://app:p%40ss@db1.example:27018,db2.example,[::1]:27019/shop?replicaSet=rs0&readPreference=secondaryPreferred\
             &readPreferenceTags=dc:ny,rack:1&readPreferenceTags=&maxStalenessSeconds=120&w=majority&journal=true&wTimeoutMS=500\
             &retryWrites=false&compressors=zstd,snappy,zlib&zlibCompressionLevel=6&appName=pos&connectTimeoutMS=2000&maxPoolSize=4",
        )
        .unwrap();
        assert_eq!(
            o.hosts,
            [Address::Tcp("db1.example".into(), 27018), Address::Tcp("db2.example".into(), 27017), Address::Tcp("::1".into(), 27019)]
        );
        let c = o.credential.clone().unwrap();
        assert_eq!((c.username.as_deref(), c.password.as_deref(), c.source.as_str()), (Some("app"), Some("p@ss"), "shop"));
        assert_eq!(o.replica_set.as_deref(), Some("rs0"));
        assert_eq!(o.read_preference.mode, ReadMode::SecondaryPreferred);
        assert_eq!(o.read_preference.tag_sets, vec![vec![("dc".into(), "ny".into()), ("rack".into(), "1".into())], vec![]]);
        assert_eq!(o.read_preference.max_staleness, Some(Duration::from_secs(120)));
        assert_eq!(o.write_concern.to_document(), Document::new().with("w", "majority").with("j", true).with("wtimeout", 500i64));
        assert!(!o.retry_writes);
        assert_eq!(o.compressors, [Compressor::Snappy, Compressor::Zlib(6)]);
        assert_eq!(o.max_pool_size, 4);
        assert_eq!(o.connect_timeout, Duration::from_secs(2));

        let o = ClientOptions::parse("mongodb+srv://u:p@cluster0.example.net/?authSource=admin&srvMaxHosts=2").unwrap();
        assert_eq!(o.srv.as_deref(), Some("cluster0.example.net"));
        assert!(o.tls.is_some(), "SRV implies TLS");
        assert_eq!(o.srv_max_hosts, 2);

        let o = ClientOptions::parse("mongodb://localhost/?authMechanism=MONGODB-X509&tls=true&tlsCertificateKeyFile=/c.pem&tlsCAFile=/ca.pem").unwrap();
        let c = o.credential.unwrap();
        assert_eq!((c.username, c.source.as_str(), c.mechanism), (None, "$external", Some(AuthMechanism::X509)));
        assert_eq!(o.tls.unwrap().ca_file, Some("/ca.pem".into()));

        let o = ClientOptions::parse("mongodb://%2Ftmp%2Fmongodb-27017.sock/db").unwrap();
        assert_eq!(o.hosts, [Address::Unix("/tmp/mongodb-27017.sock".into())]);
        assert!(o.credential.is_none());
        assert!(o.tls.is_none());

        let o = ClientOptions::parse("mongodb://h/?TLS=true&tlsInsecure=true&directConnection=true").unwrap();
        let t = o.tls.unwrap();
        assert!(t.allow_invalid_certificates && t.allow_invalid_hostnames);
        assert_eq!(o.direct_connection, Some(true));
    }

    #[test]
    fn bad_connection_strings_say_why() {
        for bad in [
            "http://h",
            "mongodb://",
            "mongodb://h:0",
            "mongodb://h:port",
            "mongodb://u:p@w@h",
            "mongodb://h/?authMechanism=GSSAPI",
            "mongodb://u:p@h/?authMechanism=MONGODB-X509",
            "mongodb://h/?readPreferenceTags=dc:ny",
            "mongodb://h/?readPreference=secondary&maxStalenessSeconds=10",
            "mongodb://a,b/?directConnection=true",
            "mongodb+srv://a.example:27017",
            "mongodb+srv://a.example,b.example",
            "mongodb://h/?tls=true&ssl=false",
            "mongodb://h/bad$db",
        ] {
            assert!(ClientOptions::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn unknown_or_invalid_options_warn_and_are_ignored() {
        for (uri, why) in [
            ("mongodb://h/?nope=1", "unknown option"),
            ("mongodb://h/?w=-1", "negative w"),
            ("mongodb://h/?heartbeatFrequencyMS=100", "below 500"),
            ("mongodb://h/?journal=", "empty value"),
            ("mongodb://h/?replicaSet=a&replicaSet=a", "repeated"),
            ("mongodb://h/?minPoolSize=1", "not implemented"),
        ] {
            let o = ClientOptions::parse(uri).unwrap_or_else(|e| panic!("{why}: {e}"));
            assert!(!o.warnings.is_empty(), "{why}: no warning");
        }
        let o = ClientOptions::parse("mongodb://h/?heartbeatFrequencyMS=100").unwrap();
        assert_eq!(o.heartbeat_frequency, ClientOptions::default().heartbeat_frequency, "the invalid value is ignored");
        // Messages never repeat URI text.
        let o = ClientOptions::parse("mongodb://h/?s3cretKey=s3cretValue").unwrap();
        assert!(o.warnings.iter().all(|w| !w.contains("s3cret")), "{:?}", o.warnings);
        let e = ClientOptions::parse("mongodb://us3r:p4ss@h:99999").unwrap_err().to_string();
        assert!(!e.contains("us3r") && !e.contains("p4ss") && !e.contains("99999"), "{e}");
    }
}
