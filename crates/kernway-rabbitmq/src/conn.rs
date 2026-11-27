//! An AMQP 0-9-1 connection and its channels.
//!
//! One actor task owns the socket: it writes what channels queue, reads
//! frames, assembles content (method + header + body frames) and routes
//! everything to its channel — replies to synchronous methods, deliveries,
//! publisher confirms, returns — and keeps heartbeats in both directions.
//! A channel the server closes (a failed declare, a bad ack…) fails with
//! the server's reply code; the connection and the other channels go on.

use crate::codec::{self, Field, Properties, Table, R, W};
use crate::options::{Mechanism, Options};
use kernway_messaging::MsgError;
use kernway_wire::BufStream;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::rc::{Rc, Weak};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

/// A decoded method frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    /// Class id.
    pub class: u16,
    /// Method id.
    pub id: u16,
    /// The arguments, encoded.
    pub args: Vec<u8>,
}

/// A message with its method (`Basic.Deliver`, `Basic.Return`, `Basic.Get-Ok`).
#[derive(Debug, Clone, PartialEq)]
pub struct Content {
    /// The method that carried it.
    pub method: Method,
    /// Its properties.
    pub props: Properties,
    /// Its body.
    pub body: Vec<u8>,
}

/// A delivered message, decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivered {
    /// The consumer it went to.
    pub consumer_tag: String,
    /// The channel's tag for acknowledging it.
    pub delivery_tag: u64,
    /// Delivered before.
    pub redelivered: bool,
    /// The exchange it was published to.
    pub exchange: String,
    /// Its routing key.
    pub routing_key: String,
    /// Properties.
    pub props: Properties,
    /// Body.
    pub body: Vec<u8>,
}

/// A message the broker returned (mandatory and unroutable).
#[derive(Debug, Clone, PartialEq)]
pub struct Returned {
    /// Reply code (312 NO_ROUTE).
    pub code: u16,
    /// Reply text.
    pub text: String,
    /// Exchange.
    pub exchange: String,
    /// Routing key.
    pub routing_key: String,
    /// Properties.
    pub props: Properties,
    /// Body.
    pub body: Vec<u8>,
}

fn close_error(what: &str, args: &[u8]) -> MsgError {
    let mut r = R::new(args);
    let code = r.u16().unwrap_or(0);
    let text = r.short().unwrap_or_default();
    let class = r.u16().unwrap_or(0);
    let method = r.u16().unwrap_or(0);
    let m = format!("{what} closed by the broker: {code} {text} (method {class}.{method})");
    match code {
        // ACCESS_REFUSED; NOT_ALLOWED (no access to the virtual host).
        403 | 530 => MsgError::Auth(m),
        320 | 501..=506 | 540 | 541 => MsgError::Connection(m),
        _ => MsgError::Broker(m),
    }
}

#[derive(Default)]
struct Chan {
    replies: VecDeque<Method>,
    deliveries: VecDeque<Content>,
    returns: VecDeque<Content>,
    /// Publisher confirms: sequence numbers awaiting ack / nack, and outcomes.
    confirm_mode: bool,
    next_seq: u64,
    unconfirmed: BTreeSet<u64>,
    outcomes: HashMap<u64, bool>,
    /// Content being assembled: the method, its properties, size, body so far.
    partial: Option<(Method, Option<Properties>, u64, Vec<u8>)>,
    closed: Option<MsgError>,
    /// Consumers the broker cancelled (queue deleted …).
    cancelled: Vec<String>,
    wakers: Vec<Waker>,
}

impl Chan {
    fn wake(&mut self) {
        for w in self.wakers.drain(..) {
            w.wake();
        }
    }
}

struct State {
    out: Vec<u8>,
    /// Bytes queued and bytes written to the socket, in total.
    queued: u64,
    written: u64,
    flush_waiters: Vec<Waker>,
    chans: HashMap<u16, Chan>,
    next_channel: u16,
    closed: Option<MsgError>,
    writer: Option<Waker>,
    close_ok: bool,
    blocked: Option<String>,
    frame_max: u32,
    channel_max: u16,
    heartbeat: Duration,
    server_properties: Table,
}

struct Inner {
    opts: Options,
    st: RefCell<State>,
}

/// An AMQP connection. Cheap to clone; `!Send`.
#[derive(Clone)]
pub struct Connection {
    inner: Rc<Inner>,
}

fn net(e: impl std::fmt::Display) -> MsgError {
    MsgError::Connection(e.to_string())
}

/// Read one frame: `(type, channel, payload)`.
async fn read_frame(io: &mut BufStream, frame_max: u32) -> Result<(u8, u16, Vec<u8>), MsgError> {
    io.fill(7).await.map_err(net)?;
    let h = io.peek(7);
    if &h[..4] == b"AMQP" {
        // The broker answered our header with its own: a protocol mismatch.
        io.fill(8).await.map_err(net)?;
        let v = io.peek(8).to_vec();
        return Err(MsgError::Protocol(format!("the broker speaks AMQP {}.{}.{}, not 0-9-1", v[5], v[6], v[7])));
    }
    let kind = h[0];
    let channel = u16::from_be_bytes([h[1], h[2]]);
    let size = u32::from_be_bytes([h[3], h[4], h[5], h[6]]);
    if size > frame_max.max(4096) {
        return Err(MsgError::Protocol(format!("AMQP frame of {size} bytes exceeds frame_max {frame_max}")));
    }
    let total = 7 + size as usize + 1;
    io.fill(total).await.map_err(net)?;
    let f = io.take(total);
    if f[total - 1] != codec::FRAME_END {
        return Err(MsgError::Protocol("AMQP frame without its end octet".into()));
    }
    Ok((kind, channel, f[7..total - 1].to_vec()))
}

fn parse_method(p: &[u8]) -> Result<Method, MsgError> {
    if p.len() < 4 {
        return Err(MsgError::Protocol("AMQP method frame too short".into()));
    }
    Ok(Method { class: u16::from_be_bytes([p[0], p[1]]), id: u16::from_be_bytes([p[2], p[3]]), args: p[4..].to_vec() })
}

async fn send(io: &mut BufStream, kind: u8, channel: u16, payload: &[u8]) -> Result<(), MsgError> {
    let w = io.write_buf();
    codec::frame(kind, channel, payload, w);
    io.flush().await.map_err(net)
}

/// Await a method on channel 0 during the handshake (skipping heartbeats).
async fn expect(io: &mut BufStream, class: u16, id: u16, what: &str) -> Result<Method, MsgError> {
    loop {
        let (kind, _, p) = read_frame(io, 131_072).await?;
        if kind == codec::FRAME_HEARTBEAT {
            continue;
        }
        if kind != codec::FRAME_METHOD {
            return Err(MsgError::Protocol(format!("expected {what}, got a frame of type {kind}")));
        }
        let m = parse_method(&p)?;
        if (m.class, m.id) == (10, 50) {
            return Err(close_error("the connection", &m.args));
        }
        if (m.class, m.id) != (class, id) {
            return Err(MsgError::Protocol(format!("expected {what}, got method {}.{}", m.class, m.id)));
        }
        return Ok(m);
    }
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

async fn handshake(opts: &Options, host: &str, port: u16) -> Result<(BufStream, State), MsgError> {
    let tcp = kernway_wire::offload::connect_tcp(host, port).await.map_err(|e| MsgError::Connection(format!("connect {host}:{port}: {e}")))?;
    let mut io = BufStream::new(tcp);
    if let Some(cfg) = tls_config(opts).await? {
        let name = opts.tls.as_ref().and_then(|t| t.server_name.clone()).unwrap_or_else(|| host.to_string());
        io.upgrade_tls(cfg, &name).await.map_err(|e| MsgError::Connection(format!("TLS with {host}: {e}")))?;
    }
    io.write_buf().extend_from_slice(codec::PROTOCOL_HEADER);
    io.flush().await.map_err(net)?;
    let start = expect(&mut io, 10, 10, "Connection.Start").await?;
    let mut r = R::new(&start.args);
    let (major, minor) = (r.u8()?, r.u8()?);
    if (major, minor) != (0, 9) {
        return Err(MsgError::Protocol(format!("the broker speaks AMQP {major}-{minor}, not 0-9-1")));
    }
    let server_properties = r.table()?;
    let mechanisms = String::from_utf8_lossy(&r.long()?).into_owned();
    let (mech, response) = match opts.mechanism {
        Mechanism::Plain => ("PLAIN", [&b"\0"[..], opts.user.as_bytes(), b"\0", opts.password.as_bytes()].concat()),
        Mechanism::AmqPlain => {
            let mut w = W::default();
            w.table(&vec![("LOGIN".into(), Field::str(&opts.user)), ("PASSWORD".into(), Field::str(&opts.password))])?;
            ("AMQPLAIN", w.b[4..].to_vec())
        }
        Mechanism::External => ("EXTERNAL", Vec::new()),
    };
    if !mechanisms.split_whitespace().any(|m| m == mech) {
        return Err(MsgError::Auth(format!("the broker does not offer {mech} (it offers: {mechanisms})")));
    }
    let caps: Table = ["publisher_confirms", "consumer_cancel_notify", "exchange_exchange_bindings", "basic.nack", "connection.blocked", "authentication_failure_close", "per_consumer_qos"]
        .iter()
        .map(|c| (c.to_string(), Field::Bool(true)))
        .collect();
    let mut client: Table = vec![
        ("product".into(), Field::str("kernway-rabbitmq")),
        ("version".into(), Field::str(env!("CARGO_PKG_VERSION"))),
        ("platform".into(), Field::str("Rust / Kernway")),
        ("capabilities".into(), Field::Table(caps)),
    ];
    if let Some(n) = &opts.name {
        client.push(("connection_name".into(), Field::str(n)));
    }
    let mut w = W::default();
    w.table(&client)?.short(mech)?.long(&response).short("en_US")?;
    send(&mut io, codec::FRAME_METHOD, 0, &codec::method(10, 11, &w.b)).await?;
    let tune = match expect(&mut io, 10, 30, "Connection.Tune").await {
        Ok(t) => t,
        // Without authentication_failure_close the broker just hangs up.
        Err(MsgError::Connection(m)) if m.contains("closed") || m.contains("EOF") || m.contains("reset") => {
            return Err(MsgError::Auth(format!("the broker closed the connection during login ({m})")));
        }
        Err(e) => return Err(e),
    };
    let mut r = R::new(&tune.args);
    let (srv_ch, srv_frame, srv_hb) = (r.u16()?, r.u32()?, r.u16()?);
    let pick = |ours: u64, theirs: u64| if ours == 0 { theirs } else if theirs == 0 { ours } else { ours.min(theirs) };
    let channel_max = pick(opts.channel_max.into(), srv_ch.into()) as u16;
    let frame_max = pick(opts.frame_max.into(), srv_frame.into()) as u32;
    let heartbeat = if opts.heartbeat.is_zero() { 0 } else { pick(opts.heartbeat.as_secs().max(1), srv_hb.into()) as u16 };
    let mut w = W::default();
    w.u16(channel_max).u32(frame_max).u16(heartbeat);
    send(&mut io, codec::FRAME_METHOD, 0, &codec::method(10, 31, &w.b)).await?;
    let mut w = W::default();
    w.short(&opts.vhost)?.short("")?.u8(0);
    send(&mut io, codec::FRAME_METHOD, 0, &codec::method(10, 40, &w.b)).await?;
    expect(&mut io, 10, 41, "Connection.Open-Ok").await.map_err(|e| match e {
        // Refused tuning is a hang-up without Connection.Close.
        MsgError::Connection(m) => MsgError::Connection(format!(
            "the broker closed the connection after tuning (channel_max {channel_max}, frame_max {frame_max}, heartbeat {heartbeat}s; RabbitMQ 4 needs frame_max >= 8192): {m}"
        )),
        e => e,
    })?;
    Ok((
        io,
        State {
            out: Vec::new(),
            queued: 0,
            written: 0,
            flush_waiters: Vec::new(),
            chans: HashMap::new(),
            next_channel: 1,
            closed: None,
            writer: None,
            close_ok: false,
            blocked: None,
            frame_max,
            channel_max: if channel_max == 0 { u16::MAX } else { channel_max },
            heartbeat: Duration::from_secs(heartbeat.into()),
            server_properties,
        },
    ))
}

impl Connection {
    /// Connect (servers in turn) and open the virtual host.
    pub async fn connect(opts: Options) -> Result<Self, MsgError> {
        let mut errors = Vec::new();
        for (host, port) in opts.servers.clone() {
            match rt_core::timeout(opts.connect_timeout, handshake(&opts, &host, port)).await {
                Ok(Ok((io, st))) => {
                    let inner = Rc::new(Inner { opts, st: RefCell::new(st) });
                    rt_core::spawn(actor(Rc::downgrade(&inner), io));
                    return Ok(Connection { inner });
                }
                Ok(Err(e @ (MsgError::Auth(_) | MsgError::Broker(_)))) => return Err(e),
                Ok(Err(e)) => errors.push(format!("{host}:{port}: {e}")),
                Err(_) => errors.push(format!("{host}:{port}: timed out")),
            }
        }
        Err(MsgError::Connection(format!("no AMQP broker accepted: {}", errors.join("; "))))
    }

    /// Connect with a URI.
    pub async fn connect_uri(uri: &str) -> Result<Self, MsgError> {
        Self::connect(Options::parse(uri)?).await
    }

    /// The options.
    pub fn options(&self) -> &Options {
        &self.inner.opts
    }

    /// The broker's properties (product, version, capabilities…).
    pub fn server_properties(&self) -> Table {
        self.inner.st.borrow().server_properties.clone()
    }

    /// Negotiated `(channel_max, frame_max, heartbeat)`.
    pub fn tuning(&self) -> (u16, u32, Duration) {
        let st = self.inner.st.borrow();
        (st.channel_max, st.frame_max, st.heartbeat)
    }

    /// Whether the connection is open.
    pub fn is_open(&self) -> bool {
        self.inner.st.borrow().closed.is_none()
    }

    /// Why the broker blocks publishers (memory / disk alarm), if it does.
    pub fn blocked(&self) -> Option<String> {
        self.inner.st.borrow().blocked.clone()
    }

    fn queue(&self, bytes: &[u8]) -> Result<(), MsgError> {
        let mut st = self.inner.st.borrow_mut();
        if let Some(e) = &st.closed {
            return Err(e.clone());
        }
        st.out.extend_from_slice(bytes);
        st.queued += bytes.len() as u64;
        if let Some(w) = st.writer.take() {
            w.wake();
        }
        Ok(())
    }

    /// Wait until everything queued so far is written to the socket (not a
    /// broker acknowledgement — AMQP has none for acks — but no longer
    /// only in this process).
    pub async fn flush(&self) -> Result<(), MsgError> {
        let target = self.inner.st.borrow().queued;
        let inner = self.inner.clone();
        let wait = std::future::poll_fn(move |cx| {
            let mut st = inner.st.borrow_mut();
            if st.written >= target {
                return Poll::Ready(Ok(()));
            }
            if let Some(e) = &st.closed {
                return Poll::Ready(Err(e.clone()));
            }
            st.flush_waiters.push(cx.waker().clone());
            Poll::Pending
        });
        rt_core::timeout(self.inner.opts.request_timeout, wait).await.unwrap_or(Err(MsgError::Connection("write did not complete within the request timeout".into())))
    }

    /// Open a channel.
    pub async fn channel(&self) -> Result<Channel, MsgError> {
        let id = {
            let mut st = self.inner.st.borrow_mut();
            if let Some(e) = &st.closed {
                return Err(e.clone());
            }
            let max = st.channel_max;
            let mut id = st.next_channel;
            let mut tries = 0u32;
            while st.chans.contains_key(&id) {
                id = if id >= max { 1 } else { id + 1 };
                tries += 1;
                if tries > u32::from(max) {
                    return Err(MsgError::Broker(format!("all {max} channels are open")));
                }
            }
            st.next_channel = if id >= max { 1 } else { id + 1 };
            st.chans.insert(id, Chan::default());
            id
        };
        let ch = Channel { conn: self.clone(), id };
        let mut w = W::default();
        w.short("")?;
        ch.rpc(20, 10, &w.b, (20, 11)).await?;
        Ok(ch)
    }

    /// Close cleanly (Connection.Close, awaiting Close-Ok).
    pub async fn close(&self) -> Result<(), MsgError> {
        let mut w = W::default();
        w.u16(200).short("bye")?.u16(0).u16(0);
        let mut out = Vec::new();
        codec::frame(codec::FRAME_METHOD, 0, &codec::method(10, 50, &w.b), &mut out);
        self.queue(&out)?;
        let inner = self.inner.clone();
        let wait = std::future::poll_fn(move |cx| {
            let mut st = inner.st.borrow_mut();
            if st.close_ok || st.closed.is_some() {
                return Poll::Ready(());
            }
            st.writer = st.writer.take().or_else(|| Some(cx.waker().clone()));
            // Re-polled by the actor's wake-ups of every channel too.
            for c in st.chans.values_mut() {
                c.wakers.push(cx.waker().clone());
            }
            Poll::Pending
        });
        let _ = rt_core::timeout(self.inner.opts.request_timeout, wait).await;
        let mut st = self.inner.st.borrow_mut();
        st.closed.get_or_insert(MsgError::Connection("connection closed".into()));
        if let Some(w) = st.writer.take() {
            w.wake();
        }
        Ok(())
    }
}

/// A channel. Dropping it closes it (best effort).
pub struct Channel {
    conn: Connection,
    id: u16,
}

impl Channel {
    /// The channel number.
    pub fn id(&self) -> u16 {
        self.id
    }

    /// Whether it is still open.
    pub fn is_open(&self) -> bool {
        let st = self.conn.inner.st.borrow();
        st.closed.is_none() && st.chans.get(&self.id).is_some_and(|c| c.closed.is_none())
    }

    fn method(&self, class: u16, id: u16, args: &[u8]) -> Result<(), MsgError> {
        self.check()?;
        let mut out = Vec::new();
        codec::frame(codec::FRAME_METHOD, self.id, &codec::method(class, id, args), &mut out);
        self.conn.queue(&out)
    }

    fn check(&self) -> Result<(), MsgError> {
        let st = self.conn.inner.st.borrow();
        if let Some(e) = &st.closed {
            return Err(e.clone());
        }
        match st.chans.get(&self.id) {
            Some(c) => c.closed.clone().map_or(Ok(()), Err),
            None => Err(MsgError::Broker("channel closed".into())),
        }
    }

    /// Wait on this channel's state until `f` yields.
    async fn wait<T>(&self, timeout: Option<Duration>, mut f: impl FnMut(&mut Chan) -> Option<T>) -> Result<T, MsgError> {
        let inner = self.conn.inner.clone();
        let id = self.id;
        let fut = std::future::poll_fn(move |cx| {
            let mut st = inner.st.borrow_mut();
            let conn_err = st.closed.clone();
            let Some(c) = st.chans.get_mut(&id) else {
                return Poll::Ready(Err(MsgError::Broker("channel closed".into())));
            };
            if let Some(v) = f(c) {
                return Poll::Ready(Ok(v));
            }
            if let Some(e) = c.closed.clone().or(conn_err) {
                return Poll::Ready(Err(e));
            }
            c.wakers.push(cx.waker().clone());
            Poll::Pending
        });
        match timeout {
            None => fut.await,
            Some(t) => rt_core::timeout(t, fut).await.unwrap_or(Err(MsgError::Timeout)),
        }
    }

    /// A synchronous method: send, await the reply `expect`.
    pub async fn rpc(&self, class: u16, id: u16, args: &[u8], expect: (u16, u16)) -> Result<Method, MsgError> {
        self.method(class, id, args)?;
        let t = self.conn.inner.opts.request_timeout;
        let m = self.wait(Some(t), |c| c.replies.pop_front()).await.map_err(|e| match e {
            MsgError::Timeout => MsgError::Connection(format!("no reply to method {class}.{id} within {t:?}")),
            e => e,
        })?;
        if (m.class, m.id) != expect {
            return Err(MsgError::Protocol(format!("method {class}.{id} answered with {}.{}", m.class, m.id)));
        }
        Ok(m)
    }

    /// Declare an exchange.
    pub async fn exchange_declare(&self, name: &str, kind: &str, durable: bool, auto_delete: bool, args: &Table) -> Result<(), MsgError> {
        let mut w = W::default();
        w.u16(0).short(name)?.short(kind)?.u8((u8::from(durable) << 1) | (u8::from(auto_delete) << 2)).table(args)?;
        self.rpc(40, 10, &w.b, (40, 11)).await.map(|_| ())
    }

    /// Check an exchange exists (passive declare; the channel closes with 404 if not).
    pub async fn exchange_exists(&self, name: &str) -> Result<(), MsgError> {
        let mut w = W::default();
        w.u16(0).short(name)?.short("")?.u8(1).table(&Vec::new())?;
        self.rpc(40, 10, &w.b, (40, 11)).await.map(|_| ())
    }

    /// Delete an exchange.
    pub async fn exchange_delete(&self, name: &str, if_unused: bool) -> Result<(), MsgError> {
        let mut w = W::default();
        w.u16(0).short(name)?.u8(u8::from(if_unused));
        self.rpc(40, 20, &w.b, (40, 21)).await.map(|_| ())
    }

    /// Declare a queue: `(name, messages, consumers)` (an empty name asks the broker for one).
    pub async fn queue_declare(&self, name: &str, durable: bool, exclusive: bool, auto_delete: bool, args: &Table) -> Result<(String, u32, u32), MsgError> {
        let mut w = W::default();
        w.u16(0).short(name)?.u8((u8::from(durable) << 1) | (u8::from(exclusive) << 2) | (u8::from(auto_delete) << 3)).table(args)?;
        let m = self.rpc(50, 10, &w.b, (50, 11)).await?;
        let mut r = R::new(&m.args);
        Ok((r.short()?, r.u32()?, r.u32()?))
    }

    /// Passive declare: `(messages, consumers)` of an existing queue.
    pub async fn queue_inspect(&self, name: &str) -> Result<(u32, u32), MsgError> {
        let mut w = W::default();
        w.u16(0).short(name)?.u8(1).table(&Vec::new())?;
        let m = self.rpc(50, 10, &w.b, (50, 11)).await?;
        let mut r = R::new(&m.args);
        r.short()?;
        Ok((r.u32()?, r.u32()?))
    }

    /// Bind a queue to an exchange.
    pub async fn queue_bind(&self, queue: &str, exchange: &str, routing_key: &str, args: &Table) -> Result<(), MsgError> {
        let mut w = W::default();
        w.u16(0).short(queue)?.short(exchange)?.short(routing_key)?.u8(0).table(args)?;
        self.rpc(50, 20, &w.b, (50, 21)).await.map(|_| ())
    }

    /// Unbind.
    pub async fn queue_unbind(&self, queue: &str, exchange: &str, routing_key: &str, args: &Table) -> Result<(), MsgError> {
        let mut w = W::default();
        w.u16(0).short(queue)?.short(exchange)?.short(routing_key)?.table(args)?;
        self.rpc(50, 50, &w.b, (50, 51)).await.map(|_| ())
    }

    /// Remove every ready message; returns how many.
    pub async fn queue_purge(&self, queue: &str) -> Result<u32, MsgError> {
        let mut w = W::default();
        w.u16(0).short(queue)?.u8(0);
        let m = self.rpc(50, 30, &w.b, (50, 31)).await?;
        R::new(&m.args).u32()
    }

    /// Delete a queue; returns how many messages it held.
    pub async fn queue_delete(&self, queue: &str, if_unused: bool, if_empty: bool) -> Result<u32, MsgError> {
        let mut w = W::default();
        w.u16(0).short(queue)?.u8(u8::from(if_unused) | (u8::from(if_empty) << 1));
        let m = self.rpc(50, 40, &w.b, (50, 41)).await?;
        R::new(&m.args).u32()
    }

    /// Prefetch: at most `count` unacknowledged deliveries to this channel's consumers.
    pub async fn qos(&self, count: u16) -> Result<(), MsgError> {
        let mut w = W::default();
        w.u32(0).u16(count).u8(0);
        self.rpc(60, 10, &w.b, (60, 11)).await.map(|_| ())
    }

    /// Publisher confirms on this channel.
    pub async fn confirm_select(&self) -> Result<(), MsgError> {
        self.rpc(85, 10, &[0], (85, 11)).await?;
        let mut st = self.conn.inner.st.borrow_mut();
        if let Some(c) = st.chans.get_mut(&self.id) {
            c.confirm_mode = true;
            c.next_seq = 1;
        }
        Ok(())
    }

    /// Publish; returns the confirm sequence number (0 without confirms).
    pub fn publish(&self, exchange: &str, routing_key: &str, mandatory: bool, props: &Properties, body: &[u8]) -> Result<u64, MsgError> {
        self.check()?;
        let mut w = W::default();
        w.u16(0).short(exchange)?.short(routing_key)?.u8(u8::from(mandatory));
        let frame_max = self.conn.inner.st.borrow().frame_max;
        let mut out = Vec::with_capacity(body.len() + 256);
        codec::content_frames(self.id, &codec::method(60, 40, &w.b), props, body, frame_max, &mut out)?;
        let seq = {
            let mut st = self.conn.inner.st.borrow_mut();
            let c = st.chans.get_mut(&self.id).ok_or_else(|| MsgError::Broker("channel closed".into()))?;
            if c.confirm_mode {
                let s = c.next_seq;
                c.next_seq += 1;
                c.unconfirmed.insert(s);
                s
            } else {
                0
            }
        };
        self.conn.queue(&out)?;
        Ok(seq)
    }

    /// Await the broker's confirm of `seq`: `true` acked, `false` nacked.
    pub async fn wait_confirm(&self, seq: u64) -> Result<bool, MsgError> {
        let t = self.conn.inner.opts.request_timeout;
        self.wait(Some(t), |c| c.outcomes.remove(&seq)).await.map_err(|e| match e {
            MsgError::Timeout => MsgError::Connection(format!("no publisher confirm within {t:?}")),
            e => e,
        })
    }

    /// A returned (unroutable) message, if one came back.
    pub fn take_return(&self) -> Option<Returned> {
        let mut st = self.conn.inner.st.borrow_mut();
        let c = st.chans.get_mut(&self.id)?.returns.pop_front()?;
        let mut r = R::new(&c.method.args);
        Some(Returned {
            code: r.u16().unwrap_or(0),
            text: r.short().unwrap_or_default(),
            exchange: r.short().unwrap_or_default(),
            routing_key: r.short().unwrap_or_default(),
            props: c.props,
            body: c.body,
        })
    }

    /// Start a consumer; returns its tag.
    pub async fn consume(&self, queue: &str, tag: &str, no_ack: bool, exclusive: bool, args: &Table) -> Result<String, MsgError> {
        let mut w = W::default();
        w.u16(0).short(queue)?.short(tag)?.u8((u8::from(no_ack) << 1) | (u8::from(exclusive) << 2)).table(args)?;
        let m = self.rpc(60, 20, &w.b, (60, 21)).await?;
        R::new(&m.args).short()
    }

    /// Stop a consumer.
    pub async fn cancel(&self, tag: &str) -> Result<(), MsgError> {
        let mut w = W::default();
        w.short(tag)?.u8(0);
        self.rpc(60, 30, &w.b, (60, 31)).await.map(|_| ())
    }

    /// Consumers the broker cancelled (e.g. their queue was deleted), taking them.
    pub fn take_cancelled(&self) -> Vec<String> {
        let mut st = self.conn.inner.st.borrow_mut();
        st.chans.get_mut(&self.id).map(|c| std::mem::take(&mut c.cancelled)).unwrap_or_default()
    }

    /// The next delivery (`None` after `timeout`).
    pub async fn next_delivery(&self, timeout: Option<Duration>) -> Result<Option<Delivered>, MsgError> {
        // A closed channel's buffered deliveries cannot be acknowledged any
        // more — the broker gives them out again — so none is handed out.
        let r = self.wait(timeout, |c| if c.closed.is_some() { None } else { c.deliveries.pop_front() }).await;
        let c = match r {
            Ok(c) => c,
            Err(MsgError::Timeout) => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut r = R::new(&c.method.args);
        Ok(Some(Delivered {
            consumer_tag: r.short()?,
            delivery_tag: r.u64()?,
            redelivered: r.u8()? & 1 != 0,
            exchange: r.short()?,
            routing_key: r.short()?,
            props: c.props,
            body: c.body,
        }))
    }

    /// Fetch one message (`Basic.Get`): `None` when the queue is empty.
    pub async fn get(&self, queue: &str, no_ack: bool) -> Result<Option<Delivered>, MsgError> {
        let mut w = W::default();
        w.u16(0).short(queue)?.u8(u8::from(no_ack));
        self.method(60, 70, &w.b)?;
        let t = self.conn.inner.opts.request_timeout;
        // Get-Ok carries content (it arrives on the delivery queue); Get-Empty is a reply.
        let got = self
            .wait(Some(t), |c| {
                if let Some(i) = c.deliveries.iter().position(|d| (d.method.class, d.method.id) == (60, 71)) {
                    return c.deliveries.remove(i).map(Ok);
                }
                c.replies.pop_front().map(Err)
            })
            .await?;
        match got {
            Ok(c) => {
                let mut r = R::new(&c.method.args);
                let delivery_tag = r.u64()?;
                let redelivered = r.u8()? & 1 != 0;
                Ok(Some(Delivered {
                    consumer_tag: String::new(),
                    delivery_tag,
                    redelivered,
                    exchange: r.short()?,
                    routing_key: r.short()?,
                    props: c.props,
                    body: c.body,
                }))
            }
            Err(m) if (m.class, m.id) == (60, 72) => Ok(None),
            Err(m) => Err(MsgError::Protocol(format!("Basic.Get answered with {}.{}", m.class, m.id))),
        }
    }

    /// Acknowledge (`multiple`: every tag up to this one).
    pub fn ack(&self, tag: u64, multiple: bool) -> Result<(), MsgError> {
        let mut w = W::default();
        w.u64(tag).u8(u8::from(multiple));
        self.method(60, 80, &w.b)
    }

    /// Negative acknowledgement: requeue or reject (dead-letter / drop).
    pub fn nack(&self, tag: u64, multiple: bool, requeue: bool) -> Result<(), MsgError> {
        let mut w = W::default();
        w.u64(tag).u8(u8::from(multiple) | (u8::from(requeue) << 1));
        self.method(60, 120, &w.b)
    }

    /// Wait until what this channel queued (acks …) is written to the socket.
    pub async fn flush(&self) -> Result<(), MsgError> {
        self.conn.flush().await
    }

    /// Close the channel (Channel.Close, awaiting Close-Ok). Unacknowledged
    /// deliveries go back to their queues.
    pub async fn close(self) -> Result<(), MsgError> {
        if self.check().is_ok() {
            let mut w = W::default();
            w.u16(200).short("bye")?.u16(0).u16(0);
            let _ = self.rpc(20, 40, &w.b, (20, 41)).await;
        }
        Ok(())
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        let open = {
            let st = self.conn.inner.st.borrow();
            st.closed.is_none() && st.chans.get(&self.id).is_some_and(|c| c.closed.is_none())
        };
        if open {
            let mut w = W::default();
            if w.u16(200).short("bye").is_ok() {
                w.u16(0).u16(0);
                let mut out = Vec::new();
                codec::frame(codec::FRAME_METHOD, self.id, &codec::method(20, 40, &w.b), &mut out);
                let _ = self.conn.queue(&out);
            }
            // Its Close-Ok frees the number (the actor removes the channel).
            if let Some(c) = self.conn.inner.st.borrow_mut().chans.get_mut(&self.id) {
                c.closed = Some(MsgError::Broker("channel closed".into()));
            }
        } else {
            self.conn.inner.st.borrow_mut().chans.remove(&self.id);
        }
    }
}

fn fail_all(st: &mut State, e: MsgError) {
    st.closed.get_or_insert(e.clone());
    for w in st.flush_waiters.drain(..) {
        w.wake();
    }
    for c in st.chans.values_mut() {
        c.closed.get_or_insert(e.clone());
        c.wake();
    }
}

/// Route one frame. Returns frames to send back at once (Close-Ok…).
fn route(st: &mut State, kind: u8, channel: u16, payload: Vec<u8>) -> Result<Vec<u8>, MsgError> {
    let mut reply = Vec::new();
    match kind {
        codec::FRAME_HEARTBEAT => {}
        codec::FRAME_METHOD => {
            let m = parse_method(&payload)?;
            if channel == 0 {
                match (m.class, m.id) {
                    (10, 50) => {
                        codec::frame(codec::FRAME_METHOD, 0, &codec::method(10, 51, &[]), &mut reply);
                        let e = close_error("the connection", &m.args);
                        fail_all(st, e);
                    }
                    (10, 51) => {
                        st.close_ok = true;
                        fail_all(st, MsgError::Connection("connection closed".into()));
                    }
                    (10, 60) => st.blocked = Some(R::new(&m.args).short().unwrap_or_default()),
                    (10, 61) => st.blocked = None,
                    _ => {}
                }
                return Ok(reply);
            }
            let Some(c) = st.chans.get_mut(&channel) else { return Ok(reply) };
            match (m.class, m.id) {
                // Content follows.
                (60, 60) | (60, 50) | (60, 71) => c.partial = Some((m, None, 0, Vec::new())),
                (60, 80) | (60, 120) => {
                    let mut r = R::new(&m.args);
                    let tag = r.u64()?;
                    let multiple = r.u8()? & 1 != 0;
                    let ok = m.id == 80;
                    let done: Vec<u64> = if multiple { c.unconfirmed.range(..=tag).copied().collect() } else { vec![tag] };
                    for s in done {
                        c.unconfirmed.remove(&s);
                        c.outcomes.insert(s, ok);
                    }
                    c.wake();
                }
                (60, 30) => {
                    // The broker cancelled a consumer.
                    c.cancelled.push(R::new(&m.args).short().unwrap_or_default());
                    c.wake();
                }
                (20, 20) => {
                    let active = m.args.first().copied().unwrap_or(1) & 1;
                    codec::frame(codec::FRAME_METHOD, channel, &codec::method(20, 21, &[active]), &mut reply);
                }
                (20, 40) => {
                    codec::frame(codec::FRAME_METHOD, channel, &codec::method(20, 41, &[]), &mut reply);
                    c.closed = Some(close_error("the channel", &m.args));
                    c.wake();
                }
                (20, 41) => {
                    // Our close went through: the number is free again.
                    c.replies.push_back(m);
                    c.closed.get_or_insert(MsgError::Broker("channel closed".into()));
                    c.wake();
                    if c.wakers.is_empty() {
                        st.chans.remove(&channel);
                    }
                }
                _ => {
                    c.replies.push_back(m);
                    c.wake();
                }
            }
        }
        codec::FRAME_HEADER => {
            let Some(c) = st.chans.get_mut(&channel) else { return Ok(reply) };
            let Some(p) = c.partial.as_mut() else { return Err(MsgError::Protocol("content header without a method".into())) };
            let mut r = R::new(&payload);
            r.u16()?; // class
            r.u16()?; // weight
            p.2 = r.u64()?;
            p.1 = Some(Properties::decode(&mut r)?);
            if p.2 == 0 {
                let (m, props, _, body) = c.partial.take().unwrap_or_else(|| unreachable!());
                deliver(c, m, props.unwrap_or_default(), body);
            }
        }
        codec::FRAME_BODY => {
            let Some(c) = st.chans.get_mut(&channel) else { return Ok(reply) };
            let Some(p) = c.partial.as_mut() else { return Err(MsgError::Protocol("content body without a header".into())) };
            p.3.extend_from_slice(&payload);
            if p.3.len() as u64 >= p.2 {
                let (m, props, _, body) = c.partial.take().unwrap_or_else(|| unreachable!());
                deliver(c, m, props.unwrap_or_default(), body);
            }
        }
        k => return Err(MsgError::Protocol(format!("unknown AMQP frame type {k}"))),
    }
    Ok(reply)
}

fn deliver(c: &mut Chan, method: Method, props: Properties, body: Vec<u8>) {
    let content = Content { method, props, body };
    if (content.method.class, content.method.id) == (60, 50) {
        c.returns.push_back(content);
    } else {
        c.deliveries.push_back(content);
    }
    c.wake();
}

enum Event {
    Frame(u8, u16, Vec<u8>),
    Write,
    Tick,
    Stop,
}

async fn actor(weak: Weak<Inner>, mut io: BufStream) {
    let mut last_sent = Instant::now();
    let mut last_recv = Instant::now();
    let result: Result<(), MsgError> = async {
        loop {
            let ev = {
                let Some(inner) = weak.upgrade() else { return Ok(()) };
                let frame_max = inner.st.borrow().frame_max;
                let mut read = std::pin::pin!(read_frame(&mut io, frame_max));
                let mut tick = std::pin::pin!(rt_core::sleep(Duration::from_millis(250)));
                std::future::poll_fn(|cx| {
                    {
                        let mut st = inner.st.borrow_mut();
                        if !st.out.is_empty() {
                            return Poll::Ready(Ok(Event::Write));
                        }
                        if st.close_ok {
                            return Poll::Ready(Ok(Event::Stop));
                        }
                        st.writer = Some(cx.waker().clone());
                    }
                    if let Poll::Ready(r) = read.as_mut().poll(cx) {
                        return Poll::Ready(r.map(|(k, c, p)| Event::Frame(k, c, p)));
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
                Event::Stop => return Ok(()),
                Event::Write => {
                    let out = std::mem::take(&mut inner.st.borrow_mut().out);
                    io.write_buf().extend_from_slice(&out);
                    io.flush().await.map_err(net)?;
                    io.write_buf();
                    last_sent = Instant::now();
                    {
                        let mut st = inner.st.borrow_mut();
                        st.written += out.len() as u64;
                        for w in st.flush_waiters.drain(..) {
                            w.wake();
                        }
                    }
                    if inner.st.borrow().closed.is_some() && !inner.st.borrow().close_ok {
                        // A close we queued after a failure: nothing more to do.
                        let st = inner.st.borrow();
                        if st.chans.is_empty() {
                            return Ok(());
                        }
                    }
                }
                Event::Tick => {
                    let hb = inner.st.borrow().heartbeat;
                    if !hb.is_zero() {
                        if last_recv.elapsed() > hb * 2 {
                            return Err(MsgError::Connection(format!("no frame from the broker for {:?}: heartbeat missed", hb * 2)));
                        }
                        if last_sent.elapsed() >= hb / 2 {
                            let w = io.write_buf();
                            codec::frame(codec::FRAME_HEARTBEAT, 0, &[], w);
                            io.flush().await.map_err(net)?;
                            io.write_buf();
                            last_sent = Instant::now();
                        }
                    }
                }
                Event::Frame(kind, channel, payload) => {
                    last_recv = Instant::now();
                    let reply = route(&mut inner.st.borrow_mut(), kind, channel, payload)?;
                    if !reply.is_empty() {
                        io.write_buf().extend_from_slice(&reply);
                        io.flush().await.map_err(net)?;
                        io.write_buf();
                        last_sent = Instant::now();
                    }
                    if inner.st.borrow().closed.is_some() {
                        return Ok(());
                    }
                }
            }
        }
    }
    .await;
    if let Some(inner) = weak.upgrade() {
        let mut st = inner.st.borrow_mut();
        let e = result.err().unwrap_or_else(|| MsgError::Connection("connection closed".into()));
        fail_all(&mut st, e);
    }
}
