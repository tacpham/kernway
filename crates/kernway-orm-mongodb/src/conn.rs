//! One connection to one server: socket (TCP / TLS / Unix), the `hello`
//! handshake (client metadata, compression, speculative authentication),
//! authentication, and commands over `OP_MSG`.

use crate::bson::{Binary, Bson, Document};
use crate::error::{check_reply, Error, Result};
use crate::options::{Address, AuthMechanism, ClientOptions, Compressor, Credential};
use crate::wire::{self, Sequence};
use kernway_tls::rustls::ClientConfig;
use kernway_wire::scram::{self, Hash, Scram};
use kernway_wire::BufStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What a server said about itself in `hello`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Hello {
    /// The raw reply.
    pub raw: Document,
    /// Round trip of the `hello`.
    pub rtt: Duration,
}

impl Hello {
    /// `isWritablePrimary` / legacy `ismaster`.
    pub fn is_writable_primary(&self) -> bool {
        self.raw.get_bool("isWritablePrimary").or(self.raw.get_bool("ismaster")).unwrap_or(false)
    }
    /// Highest wire version.
    pub fn max_wire_version(&self) -> i64 {
        self.raw.get_i64("maxWireVersion").unwrap_or(0)
    }
    /// `maxBsonObjectSize` (16 MiB).
    pub fn max_bson_size(&self) -> usize {
        self.raw.get_i64("maxBsonObjectSize").unwrap_or(16 * 1024 * 1024) as usize
    }
    /// `maxMessageSizeBytes` (48 MB).
    pub fn max_message_size(&self) -> usize {
        self.raw.get_i64("maxMessageSizeBytes").unwrap_or(48_000_000) as usize
    }
    /// `maxWriteBatchSize` (100 000).
    pub fn max_write_batch(&self) -> usize {
        self.raw.get_i64("maxWriteBatchSize").unwrap_or(100_000) as usize
    }
    /// The replica-set name.
    pub fn set_name(&self) -> Option<&str> {
        self.raw.get_str("setName")
    }
}

/// Settings every connection to the deployment shares.
#[derive(Clone)]
pub(crate) struct ConnectSpec {
    pub tls: Option<Arc<ClientConfig>>,
    pub credential: Option<Credential>,
    pub compressors: Vec<Compressor>,
    pub app_name: Option<String>,
    pub connect_timeout: Duration,
    pub socket_timeout: Option<Duration>,
    pub load_balanced: bool,
}

impl ConnectSpec {
    pub(crate) async fn from_options(o: &ClientOptions) -> Result<Self> {
        let tls = match &o.tls {
            None => None,
            Some(t) => {
                let read = |p: &std::path::Path, what: &'static str| {
                    let p = p.to_path_buf();
                    async move { kernway_wire::offload::read_file(&p).await.map_err(|e| Error::config(format!("read {what} {}: {e}", p.display()))) }
                };
                let ca = match &t.ca_file {
                    Some(p) => Some(read(p, "tlsCAFile").await?),
                    None => None,
                };
                let pem = match &t.cert_key_file {
                    Some(p) => Some(read(p, "tlsCertificateKeyFile").await?),
                    None => None,
                };
                let check = if t.allow_invalid_certificates {
                    kernway_tls::CertCheck::None
                } else if t.allow_invalid_hostnames {
                    kernway_tls::CertCheck::CaOnly
                } else {
                    kernway_tls::CertCheck::Full
                };
                // One PEM file holds the certificate chain and the key.
                let id = pem.as_ref().map(|p| kernway_tls::ClientIdentity { cert_pem: p, key_pem: p });
                Some(kernway_tls::client_config_with_identity(ca.as_deref(), check, id).map_err(|e| Error::config(format!("TLS configuration: {e}")))?)
            }
        };
        Ok(ConnectSpec {
            tls,
            credential: o.credential.clone(),
            compressors: o.compressors.clone(),
            app_name: o.app_name.clone(),
            connect_timeout: o.connect_timeout,
            socket_timeout: o.socket_timeout,
            load_balanced: o.load_balanced,
        })
    }
}

/// An open connection.
pub struct Connection {
    io: BufStream,
    address: Address,
    next_request: i32,
    compressor: Option<Compressor>,
    hello: Hello,
    socket_timeout: Option<Duration>,
    broken: bool,
    /// The pool generation it was opened in (stale after a pool clear).
    pub(crate) generation: u64,
}

fn next_request_id() -> i32 {
    use std::sync::atomic::{AtomicI32, Ordering};
    static ID: AtomicI32 = AtomicI32::new(1);
    ID.fetch_add(1, Ordering::Relaxed) & i32::MAX
}

/// The `client` metadata document of the handshake.
fn client_metadata(app: Option<&str>) -> Document {
    let mut d = Document::new()
        .with("driver", Document::new().with("name", "kernway").with("version", env!("CARGO_PKG_VERSION")))
        .with("os", Document::new().with("type", std::env::consts::OS).with("architecture", std::env::consts::ARCH));
    if let Some(a) = app {
        d.insert("application", Document::new().with("name", a));
    }
    d
}

impl Connection {
    /// Open a connection to `address`: socket, TLS, `hello`, and — with
    /// `authenticate` — the credentials (monitoring connections skip it).
    pub(crate) async fn open(address: &Address, spec: &ConnectSpec, authenticate: bool) -> Result<Self> {
        let open = async {
            let io = match address {
                Address::Tcp(h, p) => {
                    let tcp = kernway_wire::offload::connect_tcp(h, *p).await.map_err(|e| Error::network(format!("connect {address}: {e}")))?;
                    let mut io = BufStream::new(tcp);
                    if let Some(cfg) = &spec.tls {
                        io.upgrade_tls(cfg.clone(), h).await.map_err(|e| Error::network(format!("TLS handshake with {address}: {e}")))?;
                    }
                    io
                }
                Address::Unix(path) => open_unix(path).await?,
            };
            let mut c = Connection {
                io,
                address: address.clone(),
                next_request: 0,
                compressor: None,
                hello: Hello::default(),
                socket_timeout: spec.socket_timeout,
                broken: false,
                generation: 0,
            };
            c.handshake(spec, authenticate).await?;
            Ok(c)
        };
        if spec.connect_timeout.is_zero() {
            // connectTimeoutMS=0: no timeout.
            return open.await;
        }
        match rt_core::timeout(spec.connect_timeout, open).await {
            Ok(r) => r,
            Err(_) => Err(Error::network_timeout(format!("connect {address}: timed out after {:?}", spec.connect_timeout))),
        }
    }

    async fn handshake(&mut self, spec: &ConnectSpec, authenticate: bool) -> Result<()> {
        let mut hello = Document::new().with("hello", 1).with("helloOk", true).with("client", client_metadata(spec.app_name.as_deref()));
        if !spec.compressors.is_empty() {
            hello.insert("compression", Bson::Array(spec.compressors.iter().map(|c| Bson::from(c.name())).collect()));
        }
        if spec.load_balanced {
            hello.insert("loadBalanced", true);
        }
        let cred = spec.credential.as_ref().filter(|_| authenticate);
        let mut speculative: Option<(AuthMechanism, Option<Scram>)> = None;
        if let Some(c) = cred {
            if c.mechanism.is_none() {
                if let Some(u) = &c.username {
                    hello.insert("saslSupportedMechs", format!("{}.{u}", c.source));
                }
            }
            // Speculative authentication: the first step rides on hello.
            let mech = c.mechanism.unwrap_or(AuthMechanism::ScramSha256);
            match mech {
                AuthMechanism::ScramSha1 | AuthMechanism::ScramSha256 => {
                    let s = new_scram(c, mech)?;
                    let mut start = sasl_start(mech, &s);
                    start.insert("db", c.source.clone());
                    hello.insert("speculativeAuthenticate", start);
                    speculative = Some((mech, Some(s)));
                }
                AuthMechanism::X509 => {
                    hello.insert("speculativeAuthenticate", x509_command(c).with("db", "$external"));
                    speculative = Some((mech, None));
                }
                AuthMechanism::Plain => {}
            }
        }
        let started = Instant::now();
        let reply = self.command("admin", hello).await?;
        self.hello = Hello { rtt: started.elapsed(), raw: reply };
        if spec.load_balanced && !self.hello.raw.contains_key("serviceId") {
            return Err(Error::config("loadBalanced=true, but the server is not behind a load balancer (no serviceId)"));
        }
        // The first compressor the server agreed to.
        if let Some(agreed) = self.hello.raw.get_array("compression") {
            let names: Vec<&str> = agreed.iter().filter_map(Bson::as_str).collect();
            self.compressor = spec.compressors.iter().copied().find(|c| names.contains(&c.name()));
        }
        if let Some(c) = cred {
            self.authenticate(c, speculative).await?;
        }
        Ok(())
    }

    async fn authenticate(&mut self, c: &Credential, speculative: Option<(AuthMechanism, Option<Scram>)>) -> Result<()> {
        let spec_reply = self.hello.raw.get_document("speculativeAuthenticate").cloned();
        let mech = match c.mechanism {
            Some(m) => m,
            None => {
                let mechs: Vec<&str> = self.hello.raw.get_array("saslSupportedMechs").map(|a| a.iter().filter_map(Bson::as_str).collect()).unwrap_or_default();
                if mechs.is_empty() || mechs.contains(&"SCRAM-SHA-256") {
                    AuthMechanism::ScramSha256
                } else {
                    AuthMechanism::ScramSha1
                }
            }
        };
        let fail = |e: Error| Error::auth(format!("authentication of {:?} on {} failed: {}", c.username.as_deref().unwrap_or(""), c.source, e.message));
        match mech {
            AuthMechanism::X509 => {
                if spec_reply.is_some() && matches!(speculative, Some((AuthMechanism::X509, _))) {
                    return Ok(());
                }
                self.command("$external", x509_command(c)).await.map_err(fail)?;
                Ok(())
            }
            AuthMechanism::Plain => {
                let user = c.username.clone().unwrap_or_default();
                let payload = format!("\0{user}\0{}", c.password.clone().unwrap_or_default());
                let start = Document::new()
                    .with("saslStart", 1)
                    .with("mechanism", "PLAIN")
                    .with("payload", Binary { subtype: 0, bytes: payload.into_bytes() })
                    .with("autoAuthorize", 1);
                self.command(&c.source, start).await.map_err(fail)?;
                Ok(())
            }
            AuthMechanism::ScramSha1 | AuthMechanism::ScramSha256 => {
                let (mut s, first_reply) = match (speculative, spec_reply) {
                    (Some((m, Some(s))), Some(r)) if m == mech => (s, r),
                    _ => {
                        let s = new_scram(c, mech)?;
                        let r = self.command(&c.source, sasl_start(mech, &s)).await.map_err(fail)?;
                        (s, r)
                    }
                };
                let conv = first_reply.get("conversationId").cloned().unwrap_or(Bson::Int32(1));
                let server_first = payload_text(&first_reply)?;
                let fin = s.client_final(&server_first).map_err(|m| fail(Error::auth(m)))?;
                let cont = Document::new()
                    .with("saslContinue", 1)
                    .with("conversationId", conv.clone())
                    .with("payload", Binary { subtype: 0, bytes: fin.into_bytes() });
                let r = self.command(&c.source, cont).await.map_err(fail)?;
                s.verify_server(&payload_text(&r)?).map_err(|m| fail(Error::auth(m)))?;
                if !r.get_bool("done").unwrap_or(false) {
                    let cont = Document::new()
                        .with("saslContinue", 1)
                        .with("conversationId", conv)
                        .with("payload", Binary { subtype: 0, bytes: Vec::new() });
                    let r = self.command(&c.source, cont).await.map_err(fail)?;
                    if !r.get_bool("done").unwrap_or(false) {
                        return Err(Error::auth("SCRAM conversation did not finish"));
                    }
                }
                Ok(())
            }
        }
    }

    #[doc(hidden)]
    pub async fn connect_for_test(o: &ClientOptions) -> Result<Self> {
        let spec = ConnectSpec::from_options(o).await?;
        Self::open(&o.hosts[0], &spec, true).await
    }

    /// The server's handshake reply.
    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    /// The address.
    pub fn address(&self) -> &Address {
        &self.address
    }

    /// The negotiated compressor.
    pub fn compressor(&self) -> Option<Compressor> {
        self.compressor
    }

    /// Whether the connection is unusable (I/O error, timeout mid-reply).
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// Whether TLS is on.
    pub fn is_tls(&self) -> bool {
        self.io.is_tls()
    }

    /// Run `command` on database `db` (adds `$db`). A reply with `ok: 0` or write errors is an [`Error`].
    pub async fn command(&mut self, db: &str, command: Document) -> Result<Document> {
        self.command_seq(db, command, &[]).await
    }

    /// [`command`](Self::command) with document sequences.
    pub(crate) async fn command_seq(&mut self, db: &str, command: Document, seqs: &[Sequence<'_>]) -> Result<Document> {
        let reply = self.command_raw(db, command, seqs, 0).await?;
        check_reply(reply)
    }

    /// Send a command and return the reply document unchecked.
    pub(crate) async fn command_raw(&mut self, db: &str, mut command: Document, seqs: &[Sequence<'_>], flags: u32) -> Result<Document> {
        if self.broken {
            return Err(Error::network(format!("connection to {} is broken", self.address)));
        }
        command.insert("$db", db);
        let id = next_request_id();
        self.next_request = id;
        let compressor = self.compressor;
        wire::encode_msg(self.io.write_buf(), id, flags, &command, seqs, compressor)?;
        let max = self.hello.max_message_size().max(48_000_000);
        if self.io.write_buf_append().len() > max {
            return Err(Error::new(crate::error::ErrorKind::Command { code: 10334, code_name: "BSONObjectTooLarge".into() }, "message larger than maxMessageSizeBytes"));
        }
        let timeout = self.socket_timeout;
        let r = match timeout {
            Some(t) => match rt_core::timeout(t, self.round_trip(id)).await {
                Ok(r) => r,
                Err(_) => {
                    self.broken = true;
                    Err(Error::network_timeout(format!("{}: no reply within socketTimeoutMS ({t:?})", self.address)))
                }
            },
            None => self.round_trip(id).await,
        };
        if r.as_ref().is_err_and(Error::is_network) {
            self.broken = true;
        }
        r
    }

    async fn round_trip(&mut self, id: i32) -> Result<Document> {
        self.io.flush().await.map_err(|e| Error::network(format!("send to {}: {e}", self.address)))?;
        self.io.write_buf();
        let reply = self.read_reply().await?;
        if reply.response_to != id {
            self.broken = true;
            return Err(Error::protocol(format!("reply to request {} while waiting for {id}", reply.response_to)));
        }
        Ok(reply.document)
    }

    async fn read_reply(&mut self) -> Result<wire::Reply> {
        let net = |e: std::io::Error, a: &Address| Error::network(format!("read from {a}: {e}"));
        self.io.fill(4).await.map_err(|e| net(e, &self.address))?;
        let h = self.io.peek(4);
        let len = i32::from_le_bytes([h[0], h[1], h[2], h[3]]);
        let max = self.hello.max_message_size().max(48_000_000);
        if len < 16 || len as usize > max {
            self.broken = true;
            return Err(Error::protocol(format!("reply length {len}")));
        }
        self.io.fill(len as usize).await.map_err(|e| net(e, &self.address))?;
        let msg = self.io.take(len as usize);
        let r = wire::decode_reply(msg, max);
        self.io.release_drained();
        let r = r?;
        if r.flags & wire::MORE_TO_COME != 0 {
            // Only sent when the client allows exhaust replies, which it never does.
            self.broken = true;
            return Err(Error::protocol("unexpected moreToCome reply"));
        }
        Ok(r)
    }
}

fn new_scram(c: &Credential, mech: AuthMechanism) -> Result<Scram> {
    let user = c.username.clone().ok_or_else(|| Error::auth("SCRAM needs a user name"))?;
    let password = c.password.clone().ok_or_else(|| Error::auth("SCRAM needs a password"))?;
    let (hash, pw) = match mech {
        // SCRAM-SHA-1 hashes the password as MONGODB-CR did: hex(md5(user:mongo:password)).
        AuthMechanism::ScramSha1 => {
            use md5::Digest;
            let d = md5::Md5::digest(format!("{user}:mongo:{password}").as_bytes());
            (Hash::Sha1, d.iter().map(|b| format!("{b:02x}")).collect::<String>().into_bytes())
        }
        // SCRAM-SHA-256 SASLprep's the password (the server did when it stored it).
        _ => {
            let prepped = kernway_wire::saslprep::saslprep(&password).map_err(|e| Error::auth(format!("the password: {e}")))?;
            (Hash::Sha256, prepped.into_owned().into_bytes())
        }
    };
    let nonce = scram::new_nonce().map_err(|e| Error::auth(e.to_string()))?;
    Ok(Scram::new(hash, &user, &pw, nonce, scram::Binding::None))
}

fn sasl_start(mech: AuthMechanism, s: &Scram) -> Document {
    let name = if mech == AuthMechanism::ScramSha1 { "SCRAM-SHA-1" } else { "SCRAM-SHA-256" };
    Document::new()
        .with("saslStart", 1)
        .with("mechanism", name)
        .with("payload", Binary { subtype: 0, bytes: s.client_first().into_bytes() })
        .with("autoAuthorize", 1)
        .with("options", Document::new().with("skipEmptyExchange", true))
}

fn x509_command(c: &Credential) -> Document {
    let mut d = Document::new().with("authenticate", 1).with("mechanism", "MONGODB-X509");
    if let Some(u) = &c.username {
        d.insert("user", u.clone());
    }
    d
}

fn payload_text(reply: &Document) -> Result<String> {
    match reply.get("payload") {
        Some(Bson::Binary(b)) => Ok(String::from_utf8_lossy(&b.bytes).into_owned()),
        Some(Bson::String(s)) => Ok(s.clone()),
        _ => Err(Error::auth("SASL reply without a payload")),
    }
}

#[cfg(unix)]
async fn open_unix(path: &std::path::Path) -> Result<BufStream> {
    rt_net::AsyncUnixStream::connect(path)
        .await
        .map(BufStream::new_unix)
        .map_err(|e| Error::network(format!("connect {}: {e}", path.display())))
}

#[cfg(not(unix))]
async fn open_unix(path: &std::path::Path) -> Result<BufStream> {
    Err(Error::config(format!("connect {}: Unix-domain sockets are not supported on this platform", path.display())))
}
