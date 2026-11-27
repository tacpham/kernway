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
    /// Parse `host[:port]`, `[v6]:port`, or a `.sock` path.
    pub fn parse(s: &str) -> Result<Self> {
        let bad = || Error::config(format!("bad host {s:?}"));
        if s.ends_with(".sock") && (s.starts_with('/') || s.contains('%')) {
            return Ok(Address::Unix(PathBuf::from(decode(s)?)));
        }
        if let Some(rest) = s.strip_prefix('[') {
            let (h, after) = rest.split_once(']').ok_or_else(bad)?;
            let port = match after.strip_prefix(':') {
                Some(p) => p.parse().map_err(|_| bad())?,
                None if after.is_empty() => 27017,
                None => return Err(bad()),
            };
            return Ok(Address::Tcp(h.to_ascii_lowercase(), port));
        }
        match s.rsplit_once(':') {
            Some((h, p)) => {
                let port: u16 = p.parse().map_err(|_| bad())?;
                if h.is_empty() || port == 0 {
                    return Err(bad());
                }
                Ok(Address::Tcp(h.to_ascii_lowercase(), port))
            }
            None if !s.is_empty() => Ok(Address::Tcp(s.to_ascii_lowercase(), 27017)),
            None => Err(bad()),
        }
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

/// Credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
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
        }
    }
}

fn decode(s: &str) -> Result<String> {
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
                _ => return Err(Error::config(format!("bad %-escape in {s:?}"))),
            }
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| Error::config(format!("{s:?} is not UTF-8 once decoded")))
}

fn ms(k: &str, v: &str) -> Result<Duration> {
    v.parse::<u64>().map(Duration::from_millis).map_err(|_| Error::config(format!("{k} must be milliseconds, not {v:?}")))
}

fn boolean(k: &str, v: &str) -> Result<bool> {
    match v {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(Error::config(format!("{k} must be true or false, not {v:?}"))),
    }
}

impl ClientOptions {
    /// Parse `mongodb://[user:pass@]host1[:port1][,host2…][/[db]][?options]`
    /// or `mongodb+srv://[user:pass@]host[/[db]][?options]`.
    ///
    /// Option names are case-insensitive. Supported: `authSource`,
    /// `authMechanism`, `tls` / `ssl`, `tlsCAFile`, `tlsCertificateKeyFile`,
    /// `tlsInsecure`, `tlsAllowInvalidCertificates`,
    /// `tlsAllowInvalidHostnames`, `replicaSet`, `directConnection`,
    /// `loadBalanced`, `appName`, `readPreference`, `readPreferenceTags`
    /// (repeatable), `maxStalenessSeconds`, `readConcernLevel`, `w`,
    /// `journal`, `wTimeoutMS`, `retryWrites`, `retryReads`, `compressors`,
    /// `zlibCompressionLevel`, `connectTimeoutMS`, `socketTimeoutMS`,
    /// `serverSelectionTimeoutMS`, `localThresholdMS`,
    /// `heartbeatFrequencyMS`, `maxPoolSize`, `srvMaxHosts`,
    /// `srvServiceName`. Unknown options are an error (a typo would
    /// otherwise silently change nothing).
    pub fn parse(uri: &str) -> Result<Self> {
        let (srv, rest) = if let Some(r) = uri.strip_prefix("mongodb+srv://") {
            (true, r)
        } else if let Some(r) = uri.strip_prefix("mongodb://") {
            (false, r)
        } else {
            return Err(Error::config("a connection string starts with mongodb:// or mongodb+srv://"));
        };
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (auth, rest) = match rest.rsplit_once('@') {
            Some((a, r)) => (Some(a), r),
            None => (None, rest),
        };
        // The path is after the first '/' that is not part of a socket path
        // (socket paths in host lists are percent-encoded).
        let (hostlist, db) = rest.split_once('/').unwrap_or((rest, ""));
        let mut o = ClientOptions::default();
        if hostlist.is_empty() {
            return Err(Error::config("no host"));
        }
        if srv {
            if hostlist.contains(',') || hostlist.contains(':') {
                return Err(Error::config("mongodb+srv:// takes one host name without a port"));
            }
            o.srv = Some(hostlist.to_ascii_lowercase());
            o.hosts.clear();
            o.tls = Some(TlsOptions::default());
        } else {
            o.hosts = hostlist.split(',').map(|h| Address::parse(&decode(h)?)).collect::<Result<_>>()?;
        }
        if !db.is_empty() {
            let db = decode(db)?;
            if db.contains(['/', '\\', ' ', '"', '$']) {
                return Err(Error::config(format!("bad database name {db:?}")));
            }
            o.default_database = Some(db);
        }
        let mut cred = auth
            .map(|a| -> Result<Credential> {
                let (u, p) = match a.split_once(':') {
                    Some((u, p)) => (u, Some(p)),
                    None => (a, None),
                };
                if u.contains(['@', '/', ':']) || p.is_some_and(|p| p.contains(['@', '/', ':'])) {
                    return Err(Error::config("user and password must be percent-encoded (@ / : are reserved)"));
                }
                Ok(Credential {
                    username: Some(decode(u)?),
                    password: p.map(decode).transpose()?,
                    source: o.default_database.clone().unwrap_or_else(|| "admin".into()),
                    mechanism: None,
                })
            })
            .transpose()?;
        let mut auth_source = None;
        let mut mechanism = None;
        let mut tls_on: Option<bool> = if srv { Some(true) } else { None };
        let mut tls = TlsOptions::default();
        let mut tags_given = false;
        let mut read_mode = None;
        let mut max_staleness = None;
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').ok_or_else(|| Error::config(format!("option {pair:?} has no value")))?;
            let v = decode(v)?;
            let key = k.to_ascii_lowercase();
            match key.as_str() {
                "authsource" => auth_source = Some(v),
                "authmechanism" => {
                    mechanism = Some(match v.as_str() {
                        "SCRAM-SHA-1" => AuthMechanism::ScramSha1,
                        "SCRAM-SHA-256" => AuthMechanism::ScramSha256,
                        "MONGODB-X509" => AuthMechanism::X509,
                        "PLAIN" => AuthMechanism::Plain,
                        "GSSAPI" | "MONGODB-AWS" | "MONGODB-OIDC" | "MONGODB-CR" => {
                            return Err(Error::config(format!("authMechanism {v} is not supported (SCRAM-SHA-256, SCRAM-SHA-1, MONGODB-X509, PLAIN are)")))
                        }
                        _ => return Err(Error::config(format!("unknown authMechanism {v:?}"))),
                    })
                }
                "authmechanismproperties" => return Err(Error::config("authMechanismProperties: no supported mechanism takes properties")),
                "tls" | "ssl" => {
                    let on = boolean(k, &v)?;
                    if tls_on.is_some_and(|t| t != on) && !srv {
                        return Err(Error::config("tls and ssl disagree"));
                    }
                    tls_on = Some(on);
                }
                "tlscafile" => tls.ca_file = Some(v.into()),
                "tlscertificatekeyfile" => tls.cert_key_file = Some(v.into()),
                "tlsinsecure" => {
                    let b = boolean(k, &v)?;
                    tls.allow_invalid_certificates = b;
                    tls.allow_invalid_hostnames = b;
                }
                "tlsallowinvalidcertificates" => tls.allow_invalid_certificates = boolean(k, &v)?,
                "tlsallowinvalidhostnames" => tls.allow_invalid_hostnames = boolean(k, &v)?,
                "tlscertificatekeyfilepassword" => return Err(Error::config("tlsCertificateKeyFilePassword: encrypted keys are not supported; decrypt the PEM")),
                "replicaset" => o.replica_set = Some(v),
                "directconnection" => o.direct_connection = Some(boolean(k, &v)?),
                "loadbalanced" => o.load_balanced = boolean(k, &v)?,
                "appname" => {
                    if v.len() > 128 {
                        return Err(Error::config("appName is longer than 128 bytes"));
                    }
                    o.app_name = Some(v)
                }
                "readpreference" => {
                    read_mode = Some(match v.as_str() {
                        "primary" => ReadMode::Primary,
                        "primaryPreferred" => ReadMode::PrimaryPreferred,
                        "secondary" => ReadMode::Secondary,
                        "secondaryPreferred" => ReadMode::SecondaryPreferred,
                        "nearest" => ReadMode::Nearest,
                        _ => return Err(Error::config(format!("unknown readPreference {v:?}"))),
                    })
                }
                "readpreferencetags" => {
                    tags_given = true;
                    let set: Result<Vec<(String, String)>> = v
                        .split(',')
                        .filter(|t| !t.is_empty())
                        .map(|t| t.split_once(':').map(|(a, b)| (a.to_string(), b.to_string())).ok_or_else(|| Error::config(format!("bad tag {t:?}"))))
                        .collect();
                    o.read_preference.tag_sets.push(set?);
                }
                "maxstalenessseconds" => {
                    let s: i64 = v.parse().map_err(|_| Error::config("maxStalenessSeconds must be a number"))?;
                    if s >= 0 {
                        if s < 90 {
                            return Err(Error::config("maxStalenessSeconds must be at least 90"));
                        }
                        max_staleness = Some(Duration::from_secs(s as u64));
                    }
                }
                "readconcernlevel" => o.read_concern = Some(ReadConcern(v)),
                "w" => {
                    o.write_concern.w = Some(match v.parse::<i32>() {
                        Ok(n) if n >= 0 => Bson::Int32(n),
                        Ok(_) => return Err(Error::config("w must not be negative")),
                        Err(_) => Bson::String(v),
                    })
                }
                "journal" => o.write_concern.journal = Some(boolean(k, &v)?),
                "wtimeoutms" => o.write_concern.timeout = Some(ms(k, &v)?),
                "retrywrites" => o.retry_writes = boolean(k, &v)?,
                "retryreads" => o.retry_reads = boolean(k, &v)?,
                "compressors" => {
                    for c in v.split(',') {
                        match c {
                            "snappy" => o.compressors.push(Compressor::Snappy),
                            "zlib" => o.compressors.push(Compressor::Zlib(-1)),
                            // Unsupported ones are skipped, as the spec asks.
                            _ => {}
                        }
                    }
                }
                "zlibcompressionlevel" => {
                    let l: i32 = v.parse().map_err(|_| Error::config("zlibCompressionLevel must be -1..=9"))?;
                    if !(-1..=9).contains(&l) {
                        return Err(Error::config("zlibCompressionLevel must be -1..=9"));
                    }
                    o.zlib_level = l;
                }
                "connecttimeoutms" => o.connect_timeout = ms(k, &v)?.max(Duration::from_millis(1)),
                "sockettimeoutms" => {
                    let d = ms(k, &v)?;
                    o.socket_timeout = (!d.is_zero()).then_some(d);
                }
                "serverselectiontimeoutms" => o.server_selection_timeout = ms(k, &v)?,
                "localthresholdms" => o.local_threshold = ms(k, &v)?,
                "heartbeatfrequencyms" => {
                    let d = ms(k, &v)?;
                    if d < Duration::from_millis(500) {
                        return Err(Error::config("heartbeatFrequencyMS must be at least 500"));
                    }
                    o.heartbeat_frequency = d;
                }
                "maxpoolsize" => o.max_pool_size = v.parse::<usize>().map_err(|_| Error::config("maxPoolSize must be a number"))?.max(1),
                "srvmaxhosts" => o.srv_max_hosts = v.parse().map_err(|_| Error::config("srvMaxHosts must be a number"))?,
                "srvservicename" => o.srv_service_name = v,
                _ => return Err(Error::config(format!("unknown option {k:?}"))),
            }
        }
        for c in &mut o.compressors {
            if let Compressor::Zlib(l) = c {
                *l = o.zlib_level;
            }
        }
        if let Some(m) = read_mode {
            o.read_preference.mode = m;
        }
        o.read_preference.max_staleness = max_staleness;
        if o.read_preference.mode == ReadMode::Primary && (tags_given || max_staleness.is_some()) {
            return Err(Error::config("readPreferenceTags / maxStalenessSeconds do not apply to readPreference=primary"));
        }
        // TLS on when asked for, or when any TLS file is given.
        let wants_tls = tls_on.unwrap_or(tls.ca_file.is_some() || tls.cert_key_file.is_some());
        o.tls = wants_tls.then_some(tls);
        if tls_on == Some(false) && o.srv.is_some() {
            o.tls = None;
        }
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
                    if c.source == "admin" && auth_source.is_none() {
                        c.source = "$external".into();
                    }
                }
                _ => {
                    if cred.is_none() {
                        return Err(Error::config(format!("{m:?} needs a user name")));
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
            "mongodb://h/?nope=1",
            "mongodb://h/?w=-1",
            "mongodb://h/?authMechanism=GSSAPI",
            "mongodb://u:p@h/?authMechanism=MONGODB-X509",
            "mongodb://h/?readPreferenceTags=dc:ny",
            "mongodb://h/?readPreference=secondary&maxStalenessSeconds=10",
            "mongodb://a,b/?directConnection=true",
            "mongodb+srv://a.example:27017",
            "mongodb+srv://a.example,b.example",
            "mongodb://h/?heartbeatFrequencyMS=100",
            "mongodb://h/?tls=true&ssl=false",
            "mongodb://h/bad$db",
        ] {
            assert!(ClientOptions::parse(bad).is_err(), "{bad}");
        }
    }
}
