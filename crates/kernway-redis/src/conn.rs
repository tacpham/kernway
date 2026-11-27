//! One Redis connection: a transport (TCP, TLS or a Unix socket) plus a read
//! buffer, speaking RESP2 or RESP3.
//!
//! [`command`](Connection::command) is the core — write the request, read one
//! reply. [`pipeline`](Connection::pipeline) writes many and reads their
//! replies in order (one round trip). The typed helpers are thin wrappers;
//! anything else goes through `command` directly. The connection is a
//! request/reply pipe, not a pool — sharing across cores is
//! [`Pool`](crate::Pool)'s job.
//!
//! A server error (`-ERR …`) is consumed whole: the connection stays usable.
//! An I/O error, a timeout or bytes that are not RESP leave it
//! [broken](Connection::is_broken) — the pool then drops it.

use std::collections::VecDeque;
use std::net::SocketAddr;

use rt_net::AsyncTcpStream;

use crate::error::RedisError;
use crate::options::{Endpoint, Options, Protocol};
use crate::resp::{self, Value};
use crate::stream::{self, StreamEntries, StreamEntry};

/// The byte stream a connection runs over.
enum Transport {
    Tcp(AsyncTcpStream),
    Tls(Box<kernway_tls::AsyncTlsStream>),
    #[cfg(unix)]
    Unix(rt_net::AsyncUnixStream),
}

impl Transport {
    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Transport::Tcp(s) => s.read(buf).await,
            Transport::Tls(s) => s.read(buf).await,
            #[cfg(unix)]
            Transport::Unix(s) => s.read(buf).await,
        }
    }

    async fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Transport::Tcp(s) => s.write_all(buf).await,
            Transport::Tls(s) => s.write_all(buf).await,
            #[cfg(unix)]
            Transport::Unix(s) => s.write_all(buf).await,
        }
    }
}

/// A pub/sub message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PubSubMessage {
    /// The pattern that matched (`PSUBSCRIBE`), `None` for a channel subscription.
    pub pattern: Option<String>,
    /// The channel it was published on.
    pub channel: String,
    /// The payload.
    pub payload: Vec<u8>,
}

/// A single, exclusively-owned connection to a Redis server.
pub struct Connection {
    io: Transport,
    /// Bytes read from the socket but not yet parsed into a reply.
    buf: Vec<u8>,
    /// How far into `buf` the parser has consumed.
    pos: usize,
    /// The protocol agreed at connect time.
    protocol: Protocol,
    /// Reply timeout.
    timeout: Option<std::time::Duration>,
    /// RESP3 pushes that arrived while waiting for a reply.
    pushes: VecDeque<Vec<Value>>,
    /// Set by an I/O error, a timeout or a protocol error: the stream position
    /// is unknown, the connection must not be reused.
    broken: bool,
    /// In subscribe mode (RESP2: only (un)subscribe / ping are allowed).
    subscribed: bool,
    /// Reached through Sentinel: a `READONLY` error means the node was
    /// demoted by a failover, so the connection is dropped to re-resolve.
    pub(crate) sentinel_managed: bool,
}

impl Connection {
    /// Open a plain RESP2 connection to `addr` (no authentication) — the
    /// original constructor; see [`open`](Self::open) for everything else.
    pub async fn connect(addr: SocketAddr) -> Result<Self, RedisError> {
        let stream = AsyncTcpStream::connect(addr).await?;
        stream.set_nodelay(true).ok();
        Ok(Self::over(Transport::Tcp(stream), Protocol::Resp2, None))
    }

    fn over(io: Transport, protocol: Protocol, timeout: Option<std::time::Duration>) -> Self {
        Self {
            io,
            buf: Vec::with_capacity(4096),
            pos: 0,
            protocol,
            timeout,
            pushes: VecDeque::new(),
            broken: false,
            subscribed: false,
            sentinel_managed: false,
        }
    }

    /// Open and set up a connection per `opts`: resolve the host (off the
    /// reactor), connect, TLS, then `HELLO 3` with credentials and client
    /// name (or `AUTH` / `CLIENT SETNAME` in RESP2, and RESP2 when the
    /// server has no `HELLO`), then `SELECT` the database.
    pub async fn open(opts: &Options) -> Result<Self, RedisError> {
        let what = opts.describe();
        // Boxed: the setup future (TLS, handshake, Sentinel) is large, and a
        // caller deep in an async stack should not carry it inline.
        match rt_core::timeout(opts.connect_timeout, Box::pin(Self::open_inner(opts))).await {
            Ok(r) => r,
            Err(_) => Err(RedisError::Timeout(format!("connecting to {what} took over {:?}", opts.connect_timeout))),
        }
    }

    async fn open_inner(opts: &Options) -> Result<Self, RedisError> {
        if let Endpoint::Sentinel(sen) = &opts.endpoint {
            return Box::pin(crate::sentinel::open(opts, sen)).await;
        }
        let io = match &opts.endpoint {
            Endpoint::Tcp { host, port } => {
                let addr = kernway_wire::offload::resolve(host, *port)
                    .await
                    .map_err(|e| RedisError::Config(format!("resolve {host}:{port}: {e}")))?;
                let tcp = AsyncTcpStream::connect(addr).await?;
                tcp.set_nodelay(true).ok();
                match &opts.tls {
                    None => Transport::Tcp(tcp),
                    Some(t) => {
                        let read = |p: &std::path::Path, what: &'static str| {
                            let p = p.to_path_buf();
                            async move {
                                kernway_wire::offload::read_file(&p)
                                    .await
                                    .map_err(|e| RedisError::Config(format!("read {what} {}: {e}", p.display())))
                            }
                        };
                        let ca = match &t.ca_file {
                            Some(p) => Some(read(p, "ssl_ca").await?),
                            None => None,
                        };
                        let identity = match (&t.cert_file, &t.key_file) {
                            (Some(c), Some(k)) => Some((read(c, "ssl_cert").await?, read(k, "ssl_key").await?)),
                            _ => None,
                        };
                        let config = kernway_tls::client_config_with_identity(
                            ca.as_deref(),
                            t.check,
                            identity.as_ref().map(|(c, k)| kernway_tls::ClientIdentity { cert_pem: c, key_pem: k }),
                        )
                        .map_err(|e| RedisError::Config(format!("TLS: {e}")))?;
                        let name = t.server_name.as_deref().unwrap_or(host.trim_start_matches('[').trim_end_matches(']'));
                        let tls = kernway_tls::AsyncTlsStream::connect(tcp, config, name)
                            .await
                            .map_err(|e| RedisError::Tls(format!("TLS handshake with {host}: {e}")))?;
                        Transport::Tls(Box::new(tls))
                    }
                }
            }
            #[cfg(unix)]
            Endpoint::Unix(path) => {
                if opts.tls.is_some() {
                    return Err(RedisError::Config("TLS over a Unix socket is not supported".into()));
                }
                Transport::Unix(rt_net::AsyncUnixStream::connect(path).await?)
            }
            #[cfg(not(unix))]
            Endpoint::Unix(_) => {
                return Err(RedisError::Config("Unix sockets are not available on this platform; use TCP".into()));
            }
            Endpoint::Sentinel(_) => unreachable!("handled above"),
        };
        let mut conn = Self::over(io, Protocol::Resp2, opts.command_timeout);
        conn.handshake(opts).await?;
        Ok(conn)
    }

    async fn handshake(&mut self, opts: &Options) -> Result<(), RedisError> {
        let mut need_auth = opts.password.is_some();
        let mut need_name = opts.client_name.is_some();
        if opts.protocol == Protocol::Resp3 {
            let mut args: Vec<&[u8]> = vec![b"HELLO", b"3"];
            if let Some(pw) = &opts.password {
                args.extend([&b"AUTH"[..], opts.username.as_deref().unwrap_or("default").as_bytes(), pw.as_bytes()]);
            }
            if let Some(n) = &opts.client_name {
                args.extend([&b"SETNAME"[..], n.as_bytes()]);
            }
            match self.command(&args).await {
                Ok(_) => {
                    self.protocol = Protocol::Resp3;
                    need_auth = false;
                    need_name = false;
                }
                // Redis < 6: no HELLO. Carry on in RESP2.
                Err(RedisError::Server(m)) if m.starts_with("ERR unknown command") || m.contains("unknown command 'HELLO'") => {}
                Err(e) => return Err(e),
            }
        }
        if need_auth {
            let pw = opts.password.as_deref().unwrap_or_default();
            self.auth(opts.username.as_deref(), pw).await?;
        }
        if need_name {
            let n = opts.client_name.as_deref().unwrap_or_default();
            self.command(&[b"CLIENT", b"SETNAME", n.as_bytes()]).await?;
        }
        if opts.database != 0 {
            let db = opts.database.to_string();
            self.command(&[b"SELECT", db.as_bytes()]).await?;
        }
        Ok(())
    }

    /// The protocol in use (RESP3 only if the server agreed to `HELLO 3`).
    pub fn protocol(&self) -> Protocol {
        self.protocol
    }

    /// Whether an I/O error, timeout or protocol error left the connection in
    /// an unknown state. A broken connection must be dropped.
    pub fn is_broken(&self) -> bool {
        self.broken
    }

    /// The reply timeout.
    pub fn timeout(&self) -> Option<std::time::Duration> {
        self.timeout
    }

    /// Change the reply timeout.
    pub fn set_timeout(&mut self, t: Option<std::time::Duration>) {
        self.timeout = t;
    }

    /// Whether the connection is in subscribe mode.
    pub fn is_subscribed(&self) -> bool {
        self.subscribed
    }

    /// Send one command (RESP array of bulk strings) and read one reply. A `-`
    /// error reply comes back as [`RedisError::Server`]; the connection stays
    /// usable.
    pub async fn command(&mut self, args: &[&[u8]]) -> Result<Value, RedisError> {
        let mut out = Vec::with_capacity(32);
        resp::encode(args, &mut out);
        self.send(&out).await?;
        let v = self.read_reply().await?;
        match v {
            Value::Error(m) => {
                self.note_error(&m);
                Err(RedisError::Server(m))
            }
            other => Ok(other),
        }
    }

    /// A master demoted by a failover answers writes with `READONLY`: when
    /// the connection came from Sentinel, drop it so the next one asks again.
    fn note_error(&mut self, message: &str) {
        if self.sentinel_managed && message.starts_with("READONLY") {
            self.broken = true;
        }
    }

    /// Send every command, then read every reply, in order — one round trip
    /// for the lot. Each reply is `Ok` or the server's error for that command;
    /// one command failing does not stop the others.
    pub async fn pipeline(&mut self, commands: &[Vec<Vec<u8>>]) -> Result<Vec<Result<Value, RedisError>>, RedisError> {
        let mut out = Vec::with_capacity(64 * commands.len());
        for c in commands {
            let args: Vec<&[u8]> = c.iter().map(Vec::as_slice).collect();
            resp::encode(&args, &mut out);
        }
        self.send(&out).await?;
        let mut replies = Vec::with_capacity(commands.len());
        for _ in commands {
            replies.push(match self.read_reply().await? {
                Value::Error(m) => Err(RedisError::Server(m)),
                v => Ok(v),
            });
        }
        Ok(replies)
    }

    pub(crate) async fn send(&mut self, bytes: &[u8]) -> Result<(), RedisError> {
        if self.broken {
            return Err(RedisError::Closed);
        }
        let r = match self.timeout {
            Some(t) => match rt_core::timeout(t, self.io.write_all(bytes)).await {
                Ok(r) => r.map_err(RedisError::from),
                Err(_) => Err(RedisError::Timeout(format!("write took over {t:?}"))),
            },
            None => self.io.write_all(bytes).await.map_err(RedisError::from),
        };
        if r.is_err() {
            self.broken = true;
        }
        r
    }

    /// Parse the next reply (a top-level error is returned as
    /// [`Value::Error`]). RESP3 pushes met on the way are queued, except in
    /// subscribe mode where they are the replies.
    pub(crate) async fn read_reply(&mut self) -> Result<Value, RedisError> {
        loop {
            let v = self.read_frame().await?;
            match v {
                Value::Push(items) if !self.subscribed => self.pushes.push_back(items),
                other => return Ok(other),
            }
        }
    }

    async fn read_frame(&mut self) -> Result<Value, RedisError> {
        if self.broken {
            return Err(RedisError::Closed);
        }
        let r = match self.timeout {
            Some(t) => match rt_core::timeout(t, self.read_frame_inner()).await {
                Ok(r) => r,
                Err(_) => Err(RedisError::Timeout(format!("no reply within {t:?}"))),
            },
            None => self.read_frame_inner().await,
        };
        if matches!(r, Err(RedisError::Io(_) | RedisError::Closed | RedisError::Protocol(_) | RedisError::Timeout(_))) {
            self.broken = true;
        }
        r
    }

    async fn read_frame_inner(&mut self) -> Result<Value, RedisError> {
        loop {
            if self.pos < self.buf.len() {
                if let Some((value, used)) = resp::parse(&self.buf[self.pos..])? {
                    self.pos += used;
                    if self.pos == self.buf.len() {
                        self.buf.clear();
                        self.pos = 0;
                    } else if self.pos > 64 * 1024 {
                        // Keep the unread tail, drop what was consumed.
                        self.buf.drain(..self.pos);
                        self.pos = 0;
                    }
                    return Ok(value);
                }
            }
            let mut chunk = [0u8; 16 * 1024];
            let n = self.io.read(&mut chunk).await?;
            if n == 0 {
                return Err(RedisError::Closed);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// The next RESP3 push (client-side caching invalidation, a pub/sub
    /// message in RESP3), waiting for one if none is queued.
    pub async fn next_push(&mut self) -> Result<Vec<Value>, RedisError> {
        if let Some(p) = self.pushes.pop_front() {
            return Ok(p);
        }
        match self.read_frame().await? {
            Value::Push(items) => Ok(items),
            other => Err(RedisError::unexpected("push", &other)),
        }
    }

    /// RESP3 pushes already received and queued.
    pub fn pending_pushes(&self) -> usize {
        self.pushes.len()
    }

    // --- typed helpers, for the commands the session store issues ----------

    /// `PING` — a liveness / handshake check.
    pub async fn ping(&mut self) -> Result<(), RedisError> {
        self.command(&[b"PING"]).await?;
        Ok(())
    }

    /// `AUTH [user] password` — authenticate the connection. `user` is `None` for
    /// a password-only server, `Some(_)` for an ACL user. Sent once per physical
    /// connection right after connect (the [`Pool`](crate::Pool) does this).
    pub async fn auth(&mut self, user: Option<&str>, password: &str) -> Result<(), RedisError> {
        match user {
            Some(user) => {
                self.command(&[b"AUTH", user.as_bytes(), password.as_bytes()])
                    .await?
            }
            None => self.command(&[b"AUTH", password.as_bytes()]).await?,
        };
        Ok(())
    }

    /// `GET key` → the value, or `None` if the key is absent.
    pub async fn get(&mut self, key: &str) -> Result<Option<Vec<u8>>, RedisError> {
        match self.command(&[b"GET", key.as_bytes()]).await? {
            Value::Bulk(bytes) => Ok(Some(bytes)),
            Value::Nil => Ok(None),
            other => Err(RedisError::unexpected("GET", &other)),
        }
    }

    /// `SET key value EX ttl` — store with an expiry (seconds).
    pub async fn set_ex(
        &mut self,
        key: &str,
        value: &[u8],
        ttl_secs: u64,
    ) -> Result<(), RedisError> {
        let ttl = ttl_secs.to_string();
        self.command(&[b"SET", key.as_bytes(), value, b"EX", ttl.as_bytes()])
            .await?;
        Ok(())
    }

    /// `DEL key…` → the number of keys removed.
    pub async fn del(&mut self, keys: &[&str]) -> Result<i64, RedisError> {
        let mut args: Vec<&[u8]> = Vec::with_capacity(keys.len() + 1);
        args.push(b"DEL");
        args.extend(keys.iter().map(|k| k.as_bytes()));
        Ok(self.command(&args).await?.as_int().unwrap_or(0))
    }

    /// `SADD key member` — add to a set (used to index a user's sessions).
    pub async fn sadd(&mut self, key: &str, member: &str) -> Result<(), RedisError> {
        self.command(&[b"SADD", key.as_bytes(), member.as_bytes()])
            .await?;
        Ok(())
    }

    /// `SREM key member` — remove from a set.
    pub async fn srem(&mut self, key: &str, member: &str) -> Result<(), RedisError> {
        self.command(&[b"SREM", key.as_bytes(), member.as_bytes()])
            .await?;
        Ok(())
    }

    /// `SMEMBERS key` → every member, as strings (non-UTF-8 members are skipped).
    pub async fn smembers(&mut self, key: &str) -> Result<Vec<String>, RedisError> {
        match self.command(&[b"SMEMBERS", key.as_bytes()]).await? {
            Value::Array(items) | Value::Set(items) => Ok(items
                .into_iter()
                .filter_map(|v| match v {
                    Value::Bulk(b) => String::from_utf8(b).ok(),
                    _ => None,
                })
                .collect()),
            Value::Nil => Ok(Vec::new()),
            other => Err(RedisError::unexpected("SMEMBERS", &other)),
        }
    }

    /// `EXPIRE key ttl` — (re)set a key's TTL in seconds.
    pub async fn expire(&mut self, key: &str, ttl_secs: u64) -> Result<(), RedisError> {
        let ttl = ttl_secs.to_string();
        self.command(&[b"EXPIRE", key.as_bytes(), ttl.as_bytes()])
            .await?;
        Ok(())
    }

    /// `SCARD key` → the number of members in a set (0 for a missing key).
    pub async fn scard(&mut self, key: &str) -> Result<i64, RedisError> {
        Ok(self
            .command(&[b"SCARD", key.as_bytes()])
            .await?
            .as_int()
            .unwrap_or(0))
    }

    // --- sorted sets (for presence: score = last-heartbeat timestamp) -------

    /// `ZADD key score member` — add or update a member's score.
    pub async fn zadd(&mut self, key: &str, score: i64, member: &str) -> Result<(), RedisError> {
        let score = score.to_string();
        self.command(&[b"ZADD", key.as_bytes(), score.as_bytes(), member.as_bytes()])
            .await?;
        Ok(())
    }

    /// `ZSCORE key member` → the member's score, or `None` if it is not present.
    pub async fn zscore(&mut self, key: &str, member: &str) -> Result<Option<i64>, RedisError> {
        match self
            .command(&[b"ZSCORE", key.as_bytes(), member.as_bytes()])
            .await?
        {
            Value::Bulk(bytes) => Ok(std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| s.parse().ok())),
            // RESP3 answers a double.
            Value::Double(d) if d.fract() == 0.0 && d.is_finite() => Ok(Some(d as i64)),
            Value::Nil => Ok(None),
            other => Err(RedisError::unexpected("ZSCORE", &other)),
        }
    }

    /// `ZRANGEBYSCORE key min max` → members whose score is in `[min, max]`, as
    /// strings. `min`/`max` are the raw Redis bounds (`"-inf"`, `"+inf"`, a number,
    /// or `"(1700"` for exclusive).
    pub async fn zrangebyscore(
        &mut self,
        key: &str,
        min: &str,
        max: &str,
    ) -> Result<Vec<String>, RedisError> {
        match self
            .command(&[
                b"ZRANGEBYSCORE",
                key.as_bytes(),
                min.as_bytes(),
                max.as_bytes(),
            ])
            .await?
        {
            Value::Array(items) => Ok(items
                .into_iter()
                .filter_map(|v| match v {
                    Value::Bulk(b) => String::from_utf8(b).ok(),
                    _ => None,
                })
                .collect()),
            Value::Nil => Ok(Vec::new()),
            other => Err(RedisError::unexpected("ZRANGEBYSCORE", &other)),
        }
    }

    /// `ZCOUNT key min max` → how many members score within `[min, max]`.
    pub async fn zcount(&mut self, key: &str, min: &str, max: &str) -> Result<i64, RedisError> {
        Ok(self
            .command(&[b"ZCOUNT", key.as_bytes(), min.as_bytes(), max.as_bytes()])
            .await?
            .as_int()
            .unwrap_or(0))
    }

    /// `ZREMRANGEBYSCORE key min max` → drop members scoring within `[min, max]`
    /// (prunes stale heartbeats). Returns how many were removed.
    pub async fn zremrangebyscore(
        &mut self,
        key: &str,
        min: &str,
        max: &str,
    ) -> Result<i64, RedisError> {
        Ok(self
            .command(&[
                b"ZREMRANGEBYSCORE",
                key.as_bytes(),
                min.as_bytes(),
                max.as_bytes(),
            ])
            .await?
            .as_int()
            .unwrap_or(0))
    }

    // --- pub/sub ------------------------------------------------------------
    //
    // `command`'s send-one/read-one shape fits every request/reply command, but a
    // subscriber is different: after `SUBSCRIBE` the server pushes `message` frames
    // whenever a publisher sends, with no request in between. So subscribing is split
    // into `subscribe` (enter the mode) + `next_message` (block for the next push).

    /// `PUBLISH channel payload` → the number of subscribers that received it.
    /// **Fire-and-forget**: with no subscriber connected at publish time the message is
    /// dropped — Redis pub/sub is not durable, so a subscriber that was offline must
    /// reconcile any missed work from a durable source on reconnect.
    pub async fn publish(&mut self, channel: &str, payload: &[u8]) -> Result<i64, RedisError> {
        Ok(self
            .command(&[b"PUBLISH", channel.as_bytes(), payload])
            .await?
            .as_int()
            .unwrap_or(0))
    }

    /// `SUBSCRIBE channel…` — put this connection into subscribe mode and drain the
    /// per-channel confirmation frames. After this the connection is **dedicated** to
    /// receiving messages via [`next_message`](Self::next_message) /
    /// [`next_pubsub`](Self::next_pubsub); in RESP2 only (un)subscribe and PING are
    /// valid on a subscribed connection, so it must not be returned to a
    /// [`Pool`](crate::Pool) — keep it for the lifetime of the listener.
    pub async fn subscribe(&mut self, channels: &[&str]) -> Result<(), RedisError> {
        self.sub_command(b"SUBSCRIBE", channels).await
    }

    /// `PSUBSCRIBE pattern…` — subscribe to every channel matching a glob
    /// pattern (`news.*`). Messages arrive with the pattern that matched.
    pub async fn psubscribe(&mut self, patterns: &[&str]) -> Result<(), RedisError> {
        self.sub_command(b"PSUBSCRIBE", patterns).await
    }

    /// `UNSUBSCRIBE channel…` (no channels: from all).
    pub async fn unsubscribe(&mut self, channels: &[&str]) -> Result<(), RedisError> {
        self.sub_command(b"UNSUBSCRIBE", channels).await
    }

    /// `PUNSUBSCRIBE pattern…` (no patterns: from all).
    pub async fn punsubscribe(&mut self, patterns: &[&str]) -> Result<(), RedisError> {
        self.sub_command(b"PUNSUBSCRIBE", patterns).await
    }

    /// Send a (un)subscribe command and wait for its confirmations: one per
    /// name, or one when unsubscribing from everything (Redis sends one per
    /// subscription then; the rest are skipped by `next_pubsub`). Messages
    /// that arrive meanwhile are kept.
    async fn sub_command(&mut self, cmd: &[u8], names: &[&str]) -> Result<(), RedisError> {
        let mut args: Vec<&[u8]> = Vec::with_capacity(names.len() + 1);
        args.push(cmd);
        args.extend(names.iter().map(|c| c.as_bytes()));
        let mut out = Vec::with_capacity(32);
        resp::encode(&args, &mut out);
        self.subscribed = true;
        self.send(&out).await?;
        let kind = String::from_utf8_lossy(cmd).to_ascii_lowercase();
        let mut confirmations = names.len().max(1);
        while confirmations > 0 {
            match self.read_frame().await? {
                Value::Error(m) => return Err(RedisError::Server(m)),
                Value::Array(items) | Value::Push(items) => {
                    let is_confirmation = matches!(items.first(), Some(Value::Bulk(k)) if k.eq_ignore_ascii_case(kind.as_bytes()));
                    if is_confirmation {
                        confirmations -= 1;
                        // Back to normal mode once nothing is subscribed.
                        if let Some(Value::Int(0)) = items.get(2) {
                            if kind.starts_with("unsub") || kind.starts_with("punsub") {
                                self.subscribed = false;
                                confirmations = 0;
                            }
                        }
                    } else {
                        self.pushes.push_back(items);
                    }
                }
                other => return Err(RedisError::unexpected("subscribe", &other)),
            }
        }
        Ok(())
    }

    /// The next pub/sub message — from a channel or a pattern subscription.
    /// It awaits the socket, so this call *is* the push wake-up. Other frames
    /// (subscribe confirmations, a `pong`) are skipped.
    pub async fn next_pubsub(&mut self) -> Result<PubSubMessage, RedisError> {
        loop {
            let items = match self.pushes.pop_front() {
                Some(items) => items,
                None => match self.read_frame().await? {
                    Value::Array(items) | Value::Push(items) => items,
                    Value::Error(m) => return Err(RedisError::Server(m)),
                    _ => continue,
                },
            };
            let text = |v: &Value| v.as_text();
            match items.as_slice() {
                [Value::Bulk(kind), channel, payload] if kind.as_slice() == b"message" => {
                    return Ok(PubSubMessage {
                        pattern: None,
                        channel: text(channel).unwrap_or_default(),
                        payload: payload.as_bulk().map(<[u8]>::to_vec).unwrap_or_default(),
                    });
                }
                [Value::Bulk(kind), pattern, channel, payload] if kind.as_slice() == b"pmessage" => {
                    return Ok(PubSubMessage {
                        pattern: text(pattern),
                        channel: text(channel).unwrap_or_default(),
                        payload: payload.as_bulk().map(<[u8]>::to_vec).unwrap_or_default(),
                    });
                }
                _ => continue,
            }
        }
    }

    /// Block until the next message, returning `(channel, payload)` (see
    /// [`next_pubsub`](Self::next_pubsub) for the matched pattern too).
    pub async fn next_message(&mut self) -> Result<(String, Vec<u8>), RedisError> {
        let m = self.next_pubsub().await?;
        Ok((m.channel, m.payload))
    }

    // --- streams (a durable queue with consumer groups) ----------------------
    //
    // Unlike pub/sub, a stream entry read through a consumer group stays in the
    // group's pending list until `XACK`ed, so a crashed consumer loses nothing: its
    // entries are recovered by another consumer with `xautoclaim`. Reply parsing
    // lives in [`crate::stream`].

    /// `XADD key [MAXLEN ~ n] id field value …` → the new entry's ID. `id` is
    /// usually `"*"` (server-assigned). `maxlen_approx` caps the stream at about
    /// `n` entries (`MAXLEN ~`, which trims whole macro-nodes — cheap).
    pub async fn xadd(
        &mut self,
        key: &str,
        id: &str,
        fields: &[(&str, &[u8])],
        maxlen_approx: Option<u64>,
    ) -> Result<String, RedisError> {
        let args = stream::xadd_args(key, id, fields, maxlen_approx);
        match self.command(&stream::as_slices(&args)).await? {
            Value::Bulk(b) => String::from_utf8(b)
                .map_err(|_| RedisError::Protocol("XADD: non-UTF-8 entry id".into())),
            other => Err(RedisError::unexpected("XADD", &other)),
        }
    }

    /// `XGROUP CREATE key group start_id [MKSTREAM]` → `true` if created, `false`
    /// if the group already existed (`BUSYGROUP`), so it is safe to call on every
    /// startup. `start_id` is `"$"` (only new entries) or `"0"` (whole history);
    /// `mkstream` creates an empty stream if the key does not exist.
    pub async fn xgroup_create(
        &mut self,
        key: &str,
        group: &str,
        start_id: &str,
        mkstream: bool,
    ) -> Result<bool, RedisError> {
        let mut args: Vec<&[u8]> = vec![
            b"XGROUP",
            b"CREATE",
            key.as_bytes(),
            group.as_bytes(),
            start_id.as_bytes(),
        ];
        if mkstream {
            args.push(b"MKSTREAM");
        }
        match self.command(&args).await {
            Ok(_) => Ok(true),
            Err(e) if stream::is_busygroup(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// `XREADGROUP GROUP group consumer [COUNT n] [BLOCK ms] STREAMS key… id…` →
    /// the entries delivered, per stream. Use id `">"` for never-delivered entries,
    /// or `"0"` to re-read this consumer's own pending ones. A `BLOCK` that times
    /// out returns an empty vec. Note `BLOCK` holds this connection for the wait.
    pub async fn xreadgroup(
        &mut self,
        group: &str,
        consumer: &str,
        streams: &[(&str, &str)],
        count: Option<usize>,
        block_ms: Option<u64>,
    ) -> Result<Vec<StreamEntries>, RedisError> {
        let args = stream::xreadgroup_args(group, consumer, streams, count, block_ms);
        let reply = self.command(&stream::as_slices(&args)).await?;
        stream::parse_read_reply(&reply)
    }

    /// `XACK key group id…` → how many entries were acknowledged (removed from the
    /// group's pending list).
    pub async fn xack(&mut self, key: &str, group: &str, ids: &[&str]) -> Result<i64, RedisError> {
        let mut args: Vec<&[u8]> = Vec::with_capacity(ids.len() + 3);
        args.extend([b"XACK".as_slice(), key.as_bytes(), group.as_bytes()]);
        args.extend(ids.iter().map(|id| id.as_bytes()));
        match self.command(&args).await? {
            Value::Int(n) => Ok(n),
            other => Err(RedisError::unexpected("XACK", &other)),
        }
    }

    /// `XLEN key` → the number of entries (0 for a missing key).
    pub async fn xlen(&mut self, key: &str) -> Result<i64, RedisError> {
        match self.command(&[b"XLEN", key.as_bytes()]).await? {
            Value::Int(n) => Ok(n),
            other => Err(RedisError::unexpected("XLEN", &other)),
        }
    }

    /// `XPENDING key group` (summary form) → the total number of entries delivered
    /// to the group but not yet acknowledged.
    pub async fn xpending_count(&mut self, key: &str, group: &str) -> Result<i64, RedisError> {
        let reply = self
            .command(&[b"XPENDING", key.as_bytes(), group.as_bytes()])
            .await?;
        stream::parse_pending_count(&reply)
    }

    /// `XAUTOCLAIM key group consumer min_idle_ms start COUNT count` → transfer
    /// entries pending for at least `min_idle_ms` (their consumer is presumed dead)
    /// to `consumer`, returning `(next start, claimed entries)`. Call again with the
    /// returned start until it is `"0-0"` to sweep the whole pending list.
    pub async fn xautoclaim(
        &mut self,
        key: &str,
        group: &str,
        consumer: &str,
        min_idle_ms: u64,
        start: &str,
        count: usize,
    ) -> Result<(String, Vec<StreamEntry>), RedisError> {
        let idle = min_idle_ms.to_string();
        let count = count.to_string();
        let reply = self
            .command(&[
                b"XAUTOCLAIM",
                key.as_bytes(),
                group.as_bytes(),
                consumer.as_bytes(),
                idle.as_bytes(),
                start.as_bytes(),
                b"COUNT",
                count.as_bytes(),
            ])
            .await?;
        stream::parse_autoclaim_reply(&reply)
    }

    /// `XTRIM key MAXLEN [~] n` → how many entries were evicted. `approx` (`~`)
    /// lets Redis trim only whole macro-nodes, which is much cheaper.
    pub async fn xtrim_maxlen(
        &mut self,
        key: &str,
        n: u64,
        approx: bool,
    ) -> Result<i64, RedisError> {
        let n = n.to_string();
        let mut args: Vec<&[u8]> = vec![b"XTRIM", key.as_bytes(), b"MAXLEN"];
        if approx {
            args.push(b"~");
        }
        args.push(n.as_bytes());
        match self.command(&args).await? {
            Value::Int(n) => Ok(n),
            other => Err(RedisError::unexpected("XTRIM", &other)),
        }
    }
}
