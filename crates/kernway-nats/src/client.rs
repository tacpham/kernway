//! The core NATS client: one connection, owned by an actor task on the
//! shard, multiplexing publishes, subscriptions (by `sid`) and requests.
//!
//! Protocol (text, `\r\n`-terminated): the server greets with `INFO {json}`;
//! the client answers `CONNECT {json}` (credentials, NKey signature of the
//! nonce) and `PING`; then `PUB` / `HPUB` (with a `NATS/1.0` header block),
//! `SUB` / `UNSUB`, and the server sends `MSG` / `HMSG`, `PING` (answered
//! with `PONG`), `+OK`, `-ERR`, and new `INFO`s (cluster URLs). When the
//! connection drops, the actor reconnects (servers in turn, the cluster's
//! advertised ones too), subscribes again and sends what was buffered.

use crate::options::{nkey::KeyPair, Auth, Options};
use kernway_messaging::MsgError;
use kernway_wire::BufStream;
use std::cell::RefCell;
use std::future::Future;
use std::collections::{HashMap, VecDeque};
use std::rc::{Rc, Weak};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

/// Outbound bytes kept while disconnected, at most.
const MAX_BUFFERED: usize = 8 << 20;

/// A received message.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Msg {
    /// Its subject.
    pub subject: String,
    /// Where to reply, if anywhere.
    pub reply: Option<String>,
    /// Headers (from `HMSG`), in order.
    pub headers: Vec<(String, String)>,
    /// A status line (`NATS/1.0 404 No Messages`): code and description.
    pub status: Option<(u16, String)>,
    /// The payload.
    pub payload: Vec<u8>,
}

impl Msg {
    /// The first header named `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct SubState {
    subject: String,
    queue: Option<String>,
    msgs: VecDeque<Msg>,
    waker: Option<Waker>,
}

#[derive(Default)]
struct State {
    out: Vec<u8>,
    subs: HashMap<u64, SubState>,
    next_sid: u64,
    /// Fatal: the client gave up (reconnect window passed, auth refused).
    closed: Option<MsgError>,
    connected: bool,
    /// Bumped on every (re)connect.
    epoch: u64,
    writer: Option<Waker>,
    /// PINGs asked for (by `flush()` and keep-alives), written by the actor
    /// in that order, and PONGs received: the n-th PONG answers the n-th
    /// PING written on this connection.
    pings_wanted: u64,
    pings_written: u64,
    pongs_seen: u64,
    pong_waiters: Vec<Waker>,
    info: serde_json::Value,
    /// Errors the server reported (`-ERR`), the last ones.
    errors: VecDeque<String>,
    max_payload: usize,
    /// The shared reply inbox (`_INBOX.<id>.*`): its sid and prefix.
    mux: Option<(u64, String)>,
    /// Requests awaiting their reply, by token.
    responses: HashMap<u64, (Option<Msg>, Option<Waker>)>,
    next_token: u64,
}

struct Inner {
    opts: Options,
    st: RefCell<State>,
}

/// A NATS client. Cheap to clone; `!Send` (one per shard). The connection
/// lives as long as a clone does.
#[derive(Clone)]
pub struct Client {
    inner: Rc<Inner>,
}

fn net(e: impl std::fmt::Display) -> MsgError {
    MsgError::Connection(e.to_string())
}

impl Client {
    /// Connect: returns once the first server accepted the handshake (a
    /// later disconnect is healed in the background).
    pub async fn connect(opts: Options) -> Result<Self, MsgError> {
        let inner = Rc::new(Inner { opts, st: RefCell::new(State { max_payload: 1 << 20, ..Default::default() }) });
        let conn = establish(&inner).await?;
        inner.st.borrow_mut().connected = true;
        rt_core::spawn(actor(Rc::downgrade(&inner), conn));
        Ok(Client { inner })
    }

    /// Connect with a URL.
    pub async fn connect_url(url: &str) -> Result<Self, MsgError> {
        Self::connect(Options::parse(url)?).await
    }

    /// The options.
    pub fn options(&self) -> &Options {
        &self.inner.opts
    }

    /// The server's latest `INFO`.
    pub fn server_info(&self) -> serde_json::Value {
        self.inner.st.borrow().info.clone()
    }

    /// Whether the connection is up now.
    pub fn is_connected(&self) -> bool {
        self.inner.st.borrow().connected
    }

    /// How many times the client (re)connected.
    pub fn connects(&self) -> u64 {
        self.inner.st.borrow().epoch
    }

    /// Errors the server sent (`-ERR …`), taking them.
    pub fn take_errors(&self) -> Vec<String> {
        self.inner.st.borrow_mut().errors.drain(..).collect()
    }

    fn check(&self) -> Result<(), MsgError> {
        match &self.inner.st.borrow().closed {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    fn send(&self, bytes: &[u8]) -> Result<(), MsgError> {
        self.check()?;
        let mut st = self.inner.st.borrow_mut();
        if !st.connected && st.out.len() + bytes.len() > MAX_BUFFERED {
            return Err(MsgError::Connection("disconnected, and the reconnect buffer is full".into()));
        }
        st.out.extend_from_slice(bytes);
        if let Some(w) = st.writer.take() {
            w.wake();
        }
        Ok(())
    }

    /// Publish (fire and forget at the core-NATS level; see [`flush`](Self::flush)).
    pub fn publish(&self, subject: &str, reply: Option<&str>, headers: &[(String, String)], payload: &[u8]) -> Result<(), MsgError> {
        check_subject(subject)?;
        let max = self.inner.st.borrow().max_payload;
        let hdr = encode_headers(headers);
        if hdr.len() + payload.len() > max {
            return Err(MsgError::Broker(format!("message of {} bytes exceeds the server's max_payload {max}", hdr.len() + payload.len())));
        }
        let mut line = Vec::with_capacity(64 + hdr.len() + payload.len());
        let r = reply.map(|r| format!(" {r}")).unwrap_or_default();
        if hdr.is_empty() {
            line.extend_from_slice(format!("PUB {subject}{r} {}\r\n", payload.len()).as_bytes());
        } else {
            line.extend_from_slice(format!("HPUB {subject}{r} {} {}\r\n", hdr.len(), hdr.len() + payload.len()).as_bytes());
            line.extend_from_slice(&hdr);
        }
        line.extend_from_slice(payload);
        line.extend_from_slice(b"\r\n");
        self.send(&line)
    }

    /// Round trip to the server (`PING` / `PONG`): everything sent before is processed.
    pub async fn flush(&self) -> Result<(), MsgError> {
        self.check()?;
        let target = {
            let mut st = self.inner.st.borrow_mut();
            st.pings_wanted += 1;
            if let Some(w) = st.writer.take() {
                w.wake();
            }
            st.pings_wanted
        };
        let wait = std::future::poll_fn(|cx| {
            let mut st = self.inner.st.borrow_mut();
            if let Some(e) = &st.closed {
                return Poll::Ready(Err(e.clone()));
            }
            if st.pongs_seen >= target {
                return Poll::Ready(Ok(()));
            }
            st.pong_waiters.push(cx.waker().clone());
            Poll::Pending
        });
        match rt_core::timeout(self.inner.opts.request_timeout, wait).await {
            Ok(r) => r,
            Err(_) => Err(MsgError::Connection("no PONG within the request timeout".into())),
        }
    }

    /// Subscribe to `subject` (wildcards allowed), optionally in a queue group.
    pub fn subscribe(&self, subject: &str, queue: Option<&str>) -> Result<Subscriber, MsgError> {
        let sid = {
            let mut st = self.inner.st.borrow_mut();
            st.next_sid += 1;
            let sid = st.next_sid;
            st.subs.insert(sid, SubState { subject: subject.into(), queue: queue.map(String::from), ..Default::default() });
            sid
        };
        let q = queue.map(|q| format!(" {q}")).unwrap_or_default();
        self.send(format!("SUB {subject}{q} {sid}\r\n").as_bytes())?;
        Ok(Subscriber { client: self.clone(), sid })
    }

    /// A unique inbox subject.
    pub fn new_inbox(&self) -> String {
        let mut b = [0u8; 12];
        let _ = kernway_tls::fill_random(&mut b);
        format!("_INBOX.{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>())
    }

    /// Request / reply: publish with a reply subject on the shared inbox,
    /// await the first answer (a `503` status — nobody listens — is an error).
    pub async fn request(&self, subject: &str, headers: &[(String, String)], payload: &[u8], timeout: Duration) -> Result<Msg, MsgError> {
        self.check()?;
        let (reply, token) = {
            let mux = self.inner.st.borrow().mux.clone();
            let prefix = match mux {
                Some((_, p)) => p,
                None => {
                    let prefix = self.new_inbox();
                    let sub = self.subscribe(&format!("{prefix}.*"), None)?;
                    let sid = sub.sid;
                    std::mem::forget(sub); // lives as long as the client
                    self.inner.st.borrow_mut().mux = Some((sid, prefix.clone()));
                    prefix
                }
            };
            let mut st = self.inner.st.borrow_mut();
            st.next_token += 1;
            let t = st.next_token;
            st.responses.insert(t, (None, None));
            (format!("{prefix}.{t}"), t)
        };
        struct Forget<'a>(&'a Inner, u64);
        impl Drop for Forget<'_> {
            fn drop(&mut self) {
                self.0.st.borrow_mut().responses.remove(&self.1);
            }
        }
        let _forget = Forget(&self.inner, token);
        self.publish(subject, Some(&reply), headers, payload)?;
        let wait = std::future::poll_fn(|cx| {
            let mut st = self.inner.st.borrow_mut();
            if let Some(e) = &st.closed {
                return Poll::Ready(Err(e.clone()));
            }
            match st.responses.get_mut(&token) {
                Some((Some(_), _)) => Poll::Ready(Ok(st.responses.get_mut(&token).and_then(|r| r.0.take()).unwrap_or_default())),
                Some((None, w)) => {
                    *w = Some(cx.waker().clone());
                    Poll::Pending
                }
                None => Poll::Ready(Err(MsgError::Protocol("request vanished".into()))),
            }
        });
        match rt_core::timeout(timeout, wait).await {
            Ok(Ok(m)) if m.status.as_ref().is_some_and(|(c, _)| *c == 503) => Err(MsgError::Broker(format!("no responders for {subject}"))),
            Ok(r) => r,
            Err(_) => Err(MsgError::Broker(format!("request to {subject} timed out after {timeout:?}"))),
        }
    }

    /// Close: stop the actor (pending output is sent first, best effort).
    pub async fn drain(&self) {
        let _ = self.flush().await;
        let mut st = self.inner.st.borrow_mut();
        st.closed = Some(MsgError::Connection("client closed".into()));
        if let Some(w) = st.writer.take() {
            w.wake();
        }
    }
}

/// A live subscription; dropping it unsubscribes.
pub struct Subscriber {
    client: Client,
    pub(crate) sid: u64,
}

impl Subscriber {
    /// The next message.
    pub async fn next(&mut self) -> Result<Msg, MsgError> {
        let inner = self.client.inner.clone();
        let sid = self.sid;
        std::future::poll_fn(|cx| {
            let mut st = inner.st.borrow_mut();
            if let Some(m) = st.subs.get_mut(&sid).and_then(|s| s.msgs.pop_front()) {
                return Poll::Ready(Ok(m));
            }
            if let Some(e) = &st.closed {
                return Poll::Ready(Err(e.clone()));
            }
            match st.subs.get_mut(&sid) {
                Some(s) => {
                    s.waker = Some(cx.waker().clone());
                    Poll::Pending
                }
                None => Poll::Ready(Err(MsgError::Connection("unsubscribed".into()))),
            }
        })
        .await
    }

    /// The next message, or [`MsgError::Timeout`].
    pub async fn next_timeout(&mut self, wait: Duration) -> Result<Msg, MsgError> {
        match rt_core::timeout(wait, self.next()).await {
            Ok(r) => r,
            Err(_) => Err(MsgError::Timeout),
        }
    }

    /// The subscription id.
    pub fn sid(&self) -> u64 {
        self.sid
    }
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        self.client.inner.st.borrow_mut().subs.remove(&self.sid);
        let _ = self.client.send(format!("UNSUB {}\r\n", self.sid).as_bytes());
    }
}

fn check_subject(s: &str) -> Result<(), MsgError> {
    if s.is_empty() || s.bytes().any(|b| b == b' ' || b == b'\t' || b == b'\r' || b == b'\n') || s.starts_with('.') || s.ends_with('.') || s.contains("..") {
        return Err(MsgError::Config(format!("invalid subject {s:?}")));
    }
    Ok(())
}

fn encode_headers(h: &[(String, String)]) -> Vec<u8> {
    if h.is_empty() {
        return Vec::new();
    }
    let mut out = b"NATS/1.0\r\n".to_vec();
    for (k, v) in h {
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(v.replace(['\r', '\n'], " ").as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// Parse a `NATS/1.0[ code[ description]]` header block.
type Headers = (Option<(u16, String)>, Vec<(String, String)>);

pub(crate) fn decode_headers(b: &[u8]) -> Headers {
    let text = String::from_utf8_lossy(b);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or("");
    let status = first.strip_prefix("NATS/1.0").map(str::trim).filter(|s| !s.is_empty()).and_then(|s| {
        let (code, desc) = s.split_once(' ').unwrap_or((s, ""));
        code.parse::<u16>().ok().map(|c| (c, desc.to_string()))
    });
    let headers = lines.filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))).collect();
    (status, headers)
}

async fn tls_config(opts: &Options) -> Result<Option<std::sync::Arc<kernway_tls::rustls::ClientConfig>>, MsgError> {
    let Some(t) = &opts.tls else { return Ok(None) };
    let read = |p: &std::path::Path| {
        let p = p.to_path_buf();
        async move { kernway_wire::offload::read_file(&p).await.map_err(|e| MsgError::Config(format!("read {}: {e}", p.display()))) }
    };
    let ca = match &t.ca_file {
        Some(p) => Some(read(p).await?),
        None => None,
    };
    let id = match (&t.cert_file, &t.key_file) {
        (Some(c), Some(k)) => Some((read(c).await?, read(k).await?)),
        _ => None,
    };
    let check = if t.insecure { kernway_tls::CertCheck::None } else { kernway_tls::CertCheck::Full };
    let ident = id.as_ref().map(|(c, k)| kernway_tls::ClientIdentity { cert_pem: c, key_pem: k });
    kernway_tls::client_config_with_identity(ca.as_deref(), check, ident).map(Some).map_err(|e| MsgError::Config(format!("TLS: {e}")))
}

/// Read one `\r\n` line (the `INFO` greeting, the handshake's answers).
async fn read_line(io: &mut BufStream) -> Result<String, MsgError> {
    let mut want = 1;
    loop {
        io.fill(want).await.map_err(net)?;
        let buf = io.peek(io.buffered());
        if let Some(i) = buf.windows(2).position(|w| w == b"\r\n") {
            let line = String::from_utf8_lossy(&buf[..i]).into_owned();
            io.take(i + 2);
            return Ok(line);
        }
        if io.buffered() > 64 * 1024 {
            return Err(MsgError::Protocol("line too long".into()));
        }
        want = io.buffered() + 1;
    }
}

/// Connect to one of the servers and complete the handshake.
async fn establish(inner: &Inner) -> Result<BufStream, MsgError> {
    let opts = &inner.opts;
    let tls = tls_config(opts).await?;
    let mut servers = opts.servers.clone();
    // The cluster's advertised members, after the configured ones.
    if let Some(urls) = inner.st.borrow().info.get("connect_urls").and_then(|u| u.as_array()) {
        for u in urls.iter().filter_map(|u| u.as_str()) {
            if let Some((h, p)) = u.rsplit_once(':').and_then(|(h, p)| p.parse().ok().map(|p| (h.trim_matches(['[', ']']).to_string(), p))) {
                if !servers.contains(&(h.clone(), p)) {
                    servers.push((h, p));
                }
            }
        }
    }
    let mut errors = Vec::new();
    for (host, port) in &servers {
        match rt_core::timeout(opts.connect_timeout, handshake(inner, host, *port, tls.clone())).await {
            Ok(Ok(io)) => return Ok(io),
            Ok(Err(e @ MsgError::Auth(_))) => return Err(e),
            Ok(Err(e)) => errors.push(format!("{host}:{port}: {e}")),
            Err(_) => errors.push(format!("{host}:{port}: timed out")),
        }
    }
    Err(MsgError::Connection(format!("no NATS server accepted: {}", errors.join("; "))))
}

async fn handshake(inner: &Inner, host: &str, port: u16, tls: Option<std::sync::Arc<kernway_tls::rustls::ClientConfig>>) -> Result<BufStream, MsgError> {
    let opts = &inner.opts;
    let tcp = kernway_wire::offload::connect_tcp(host, port).await.map_err(|e| MsgError::Connection(format!("connect {host}:{port}: {e}")))?;
    let mut io = BufStream::new(tcp);
    let first = opts.tls.as_ref().is_some_and(|t| t.handshake_first);
    if first {
        let cfg = tls.clone().ok_or_else(|| MsgError::Config("tls_first without TLS".into()))?;
        io.upgrade_tls(cfg, host).await.map_err(|e| MsgError::Connection(format!("TLS with {host}: {e}")))?;
    }
    let line = read_line(&mut io).await?;
    let info: serde_json::Value = line
        .strip_prefix("INFO ")
        .and_then(|j| serde_json::from_str(j).ok())
        .ok_or_else(|| MsgError::Protocol(format!("expected INFO, got {line:?}")))?;
    let tls_required = info.get("tls_required").and_then(|v| v.as_bool()).unwrap_or(false);
    if !first && (tls_required || opts.tls.is_some()) {
        let cfg = match tls {
            Some(c) => c,
            None => kernway_tls::client_config(None, kernway_tls::CertCheck::Full).map_err(|e| MsgError::Config(format!("TLS: {e}")))?,
        };
        io.upgrade_tls(cfg, host).await.map_err(|e| MsgError::Connection(format!("TLS with {host}: {e}")))?;
    }
    let mut connect = serde_json::json!({
        "verbose": false, "pedantic": false, "tls_required": io.is_tls(), "headers": true, "no_responders": true,
        "lang": "rust-kernway", "version": env!("CARGO_PKG_VERSION"), "protocol": 1, "echo": true,
    });
    if let Some(n) = &opts.name {
        connect["name"] = n.clone().into();
    }
    let nonce = info.get("nonce").and_then(|v| v.as_str()).unwrap_or("").to_string();
    match &opts.auth {
        Auth::None => {}
        Auth::UserPassword(u, p) => {
            connect["user"] = u.clone().into();
            connect["pass"] = p.clone().into();
        }
        Auth::Token(t) => connect["auth_token"] = t.clone().into(),
        Auth::NKey(seed) => {
            let kp = KeyPair::from_seed(seed)?;
            connect["nkey"] = kp.public_key().into();
            connect["sig"] = kp.sign(nonce.as_bytes()).into();
        }
        Auth::Jwt { jwt, seed } => {
            let kp = KeyPair::from_seed(seed)?;
            connect["jwt"] = jwt.clone().into();
            connect["sig"] = kp.sign(nonce.as_bytes()).into();
        }
    }
    let w = io.write_buf();
    w.extend_from_slice(format!("CONNECT {connect}\r\nPING\r\n").as_bytes());
    io.flush().await.map_err(net)?;
    io.write_buf();
    loop {
        let l = read_line(&mut io).await?;
        if l == "PONG" {
            break;
        }
        if let Some(e) = l.strip_prefix("-ERR ") {
            let e = e.trim_matches('\'').to_string();
            return Err(if e.to_ascii_lowercase().contains("authorization") || e.to_ascii_lowercase().contains("authentication") {
                MsgError::Auth(e)
            } else {
                MsgError::Broker(e)
            });
        }
        // +OK, INFO updates before the PONG: ignore.
    }
    let mut st = inner.st.borrow_mut();
    st.max_payload = info.get("max_payload").and_then(|v| v.as_u64()).unwrap_or(1 << 20) as usize;
    st.info = info;
    st.epoch += 1;
    Ok(io)
}

enum Event {
    Readable,
    Write,
    Tick,
    Closed,
}

fn wake_all(st: &mut State) {
    for s in st.subs.values_mut() {
        if let Some(w) = s.waker.take() {
            w.wake();
        }
    }
    for w in st.pong_waiters.drain(..) {
        w.wake();
    }
    for r in st.responses.values_mut() {
        if let Some(w) = r.1.take() {
            w.wake();
        }
    }
}

/// Send what callers queued plus the PINGs asked for.
async fn write_out(inner: &Inner, io: &mut BufStream) -> Result<(), MsgError> {
    let (out, pings) = {
        let mut st = inner.st.borrow_mut();
        let pings = st.pings_wanted - st.pings_written;
        st.pings_written = st.pings_wanted;
        (std::mem::take(&mut st.out), pings)
    };
    let w = io.write_buf();
    w.extend_from_slice(&out);
    for _ in 0..pings {
        w.extend_from_slice(b"PING\r\n");
    }
    let r = io.flush().await.map_err(net);
    io.write_buf();
    if r.is_err() {
        // Keep what may not have gone out: it is sent again after reconnecting
        // (the PINGs are re-sent by resetting the written count there).
        let mut st = inner.st.borrow_mut();
        let rest = std::mem::take(&mut st.out);
        st.out = [out, rest].concat();
    }
    r
}

/// The connection's owner: writes what callers queue, reads and routes
/// what the server sends, pings, and reconnects.
async fn actor(weak: Weak<Inner>, mut io: BufStream) {
    loop {
        let result: Result<(), MsgError> = async {
            let mut last_ping = Instant::now();
            let mut keepalive_at: Option<u64> = None;
            loop {
                let ev = {
                    let Some(inner) = weak.upgrade() else { return Ok(()) };
                    let need = io.buffered() + 1;
                    let mut fill = std::pin::pin!(io.fill(need));
                    let mut tick = std::pin::pin!(rt_core::sleep(Duration::from_millis(250)));
                    std::future::poll_fn(|cx| {
                        {
                            let mut st = inner.st.borrow_mut();
                            if st.closed.is_some() {
                                return Poll::Ready(Ok(Event::Closed));
                            }
                            if !st.out.is_empty() || st.pings_wanted > st.pings_written {
                                return Poll::Ready(Ok(Event::Write));
                            }
                            st.writer = Some(cx.waker().clone());
                        }
                        if let Poll::Ready(r) = fill.as_mut().poll(cx) {
                            return Poll::Ready(r.map(|_| Event::Readable).map_err(net));
                        }
                        if tick.as_mut().poll(cx).is_ready() {
                            return Poll::Ready(Ok(Event::Tick));
                        }
                        Poll::Pending
                    })
                    .await?
                };
                let Some(inner) = weak.upgrade() else { return Ok(()) };
                match ev {
                    Event::Closed => {
                        let _ = write_out(&inner, &mut io).await;
                        return Ok(());
                    }
                    Event::Write => write_out(&inner, &mut io).await?,
                    Event::Tick => {
                        if last_ping.elapsed() >= inner.opts.ping_interval {
                            let mut st = inner.st.borrow_mut();
                            // The previous keep-alive is still unanswered: stale.
                            if keepalive_at.is_some_and(|n| st.pongs_seen < n) {
                                return Err(MsgError::Connection("no PONG from the server: stale connection".into()));
                            }
                            st.pings_wanted += 1;
                            keepalive_at = Some(st.pings_wanted);
                            last_ping = Instant::now();
                        }
                    }
                    Event::Readable => {
                        while let Some(op) = parse_op(&mut io)? {
                            route(&inner, &mut io, op).await?;
                        }
                    }
                }
            }
        }
        .await;
        let Some(inner) = weak.upgrade() else { return };
        if inner.st.borrow().closed.is_some() {
            return;
        }
        let Err(e) = result else { return };
        inner.st.borrow_mut().connected = false;
        let started = Instant::now();
        let mut delay = Duration::from_millis(50);
        loop {
            if inner.opts.reconnect_for.is_some_and(|w| started.elapsed() > w) {
                let mut st = inner.st.borrow_mut();
                st.closed = Some(MsgError::Connection(format!("gave up reconnecting: {e}")));
                wake_all(&mut st);
                return;
            }
            rt_core::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(2));
            if inner.st.borrow().closed.is_some() {
                return;
            }
            match establish(&inner).await {
                Ok(new_io) => {
                    io = new_io;
                    let mut st = inner.st.borrow_mut();
                    // Subscribe again, then send what waited; unanswered PINGs go again.
                    let mut resub = Vec::new();
                    let mut sids: Vec<&u64> = st.subs.keys().collect();
                    sids.sort();
                    for sid in sids {
                        let s = &st.subs[sid];
                        let q = s.queue.as_ref().map(|q| format!(" {q}")).unwrap_or_default();
                        resub.extend_from_slice(format!("SUB {}{q} {sid}\r\n", s.subject).as_bytes());
                    }
                    let pending = std::mem::take(&mut st.out);
                    st.out = [resub, pending].concat();
                    st.pings_written = st.pongs_seen;
                    st.connected = true;
                    break;
                }
                Err(MsgError::Auth(m)) => {
                    let mut st = inner.st.borrow_mut();
                    st.closed = Some(MsgError::Auth(m));
                    wake_all(&mut st);
                    return;
                }
                Err(_) => continue,
            }
        }
    }
}

enum Op {
    Msg { subject: String, sid: u64, reply: Option<String>, hdr_len: usize, total: usize },
    Ping,
    Pong,
    Ok,
    Err(String),
    Info(String),
}

/// One complete operation from the buffer (`None`: more bytes needed).
fn parse_op(io: &mut BufStream) -> Result<Option<(Op, Vec<u8>)>, MsgError> {
    let buf = io.peek(io.buffered());
    let Some(eol) = buf.windows(2).position(|w| w == b"\r\n") else {
        if buf.len() > 64 * 1024 {
            return Err(MsgError::Protocol("control line too long".into()));
        }
        return Ok(None);
    };
    let line = String::from_utf8_lossy(&buf[..eol]).into_owned();
    let mut parts = line.split_whitespace();
    let verb = parts.next().unwrap_or("").to_ascii_uppercase();
    let op = match verb.as_str() {
        "MSG" | "HMSG" => {
            let a: Vec<&str> = parts.collect();
            let bad = || MsgError::Protocol(format!("bad {verb} line {line:?}"));
            let (subject, sid, reply, hdr_len, total) = match (verb.as_str(), a.len()) {
                ("MSG", 3) => (a[0], a[1], None, 0, a[2]),
                ("MSG", 4) => (a[0], a[1], Some(a[2]), 0, a[3]),
                ("HMSG", 4) => (a[0], a[1], None, a[2].parse().map_err(|_| bad())?, a[3]),
                ("HMSG", 5) => (a[0], a[1], Some(a[2]), a[3].parse().map_err(|_| bad())?, a[4]),
                _ => return Err(bad()),
            };
            let total: usize = total.parse().map_err(|_| bad())?;
            Op::Msg { subject: subject.into(), sid: sid.parse().map_err(|_| bad())?, reply: reply.map(String::from), hdr_len, total }
        }
        "PING" => Op::Ping,
        "PONG" => Op::Pong,
        "+OK" => Op::Ok,
        "-ERR" => Op::Err(line[4..].trim().trim_matches('\'').to_string()),
        "INFO" => Op::Info(line[4..].trim().to_string()),
        _ => return Err(MsgError::Protocol(format!("unknown operation {line:?}"))),
    };
    if let Op::Msg { total, .. } = &op {
        let need = eol + 2 + total + 2;
        if buf.len() < need {
            return Ok(None);
        }
        let body = buf[eol + 2..eol + 2 + total].to_vec();
        io.take(need);
        return Ok(Some((op, body)));
    }
    io.take(eol + 2);
    Ok(Some((op, Vec::new())))
}

async fn route(inner: &Inner, io: &mut BufStream, (op, body): (Op, Vec<u8>)) -> Result<(), MsgError> {
    match op {
        Op::Msg { subject, sid, reply, hdr_len, .. } => {
            let (status, headers) = if hdr_len > 0 { decode_headers(&body[..hdr_len.min(body.len())]) } else { (None, Vec::new()) };
            let payload = body[hdr_len.min(body.len())..].to_vec();
            let mut st = inner.st.borrow_mut();
            if st.mux.as_ref().is_some_and(|(m, _)| *m == sid) {
                let token = subject.rsplit('.').next().and_then(|t| t.parse::<u64>().ok());
                if let Some(r) = token.and_then(|t| st.responses.get_mut(&t)) {
                    if r.0.is_none() {
                        r.0 = Some(Msg { subject, reply, headers, status, payload });
                        if let Some(w) = r.1.take() {
                            w.wake();
                        }
                    }
                }
                return Ok(());
            }
            if let Some(s) = st.subs.get_mut(&sid) {
                s.msgs.push_back(Msg { subject, reply, headers, status, payload });
                if let Some(w) = s.waker.take() {
                    w.wake();
                }
            }
        }
        Op::Ping => {
            io.write_buf().extend_from_slice(b"PONG\r\n");
            io.flush().await.map_err(net)?;
            io.write_buf();
        }
        Op::Pong => {
            let mut st = inner.st.borrow_mut();
            st.pongs_seen += 1;
            for w in st.pong_waiters.drain(..) {
                w.wake();
            }
        }
        Op::Ok => {}
        Op::Err(e) => {
            let mut st = inner.st.borrow_mut();
            if st.errors.len() >= 32 {
                st.errors.pop_front();
            }
            st.errors.push_back(e.clone());
            if e.to_ascii_lowercase().contains("stale connection") {
                return Err(MsgError::Connection(e));
            }
        }
        Op::Info(j) => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&j) {
                let mut st = inner.st.borrow_mut();
                if let Some(obj) = v.as_object() {
                    for (k, val) in obj {
                        st.info[k] = val.clone();
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_round_trip() {
        let h = vec![("Kw-Key".to_string(), "k1".to_string()), ("X".to_string(), "a: b".to_string())];
        let enc = encode_headers(&h);
        assert!(enc.starts_with(b"NATS/1.0\r\n") && enc.ends_with(b"\r\n\r\n"));
        let (status, got) = decode_headers(&enc);
        assert_eq!(status, None);
        assert_eq!(got, h);
        let (status, _) = decode_headers(b"NATS/1.0 404 No Messages\r\n\r\n");
        assert_eq!(status, Some((404, "No Messages".into())));
        assert!(check_subject("a.b.c").is_ok());
        for bad in ["", "a b", ".a", "a.", "a..b"] {
            assert!(check_subject(bad).is_err(), "{bad:?}");
        }
    }
}
