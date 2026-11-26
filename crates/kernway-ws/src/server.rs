//! WebSocket **server** (RFC 6455) on the Kernway runtime.
//!
//! A route answers the upgrade request with [`upgrade`]; the server sends the
//! `101` and hands the socket to the `on_open` task on the same shard:
//!
//! ```ignore
//! router.get("/ws", |req, _ctx| {
//!     kernway_ws::upgrade(&req, WsConfig::default(), |mut ws| async move {
//!         let tx = ws.sender();
//!         while let Some(msg) = ws.recv().await {
//!             if let Message::Text(t) = msg {
//!                 let _ = tx.send_text(&t); // echo
//!             }
//!         }
//!     })
//! })
//! ```
//!
//! # Performance model
//! Built for one core holding tens of thousands of mostly-idle connections that
//! occasionally burst (a chess move fanned out to a table's spectators):
//!
//! - **No timer on the hot path.** Reading a message costs no timer insert or
//!   syscall for time: a per-shard heartbeat (`heartbeat` module) owns *one*
//!   timer, sends pings, and closes idle connections; connections only stamp a
//!   coarse clock.
//! - **Cursor buffer.** Frames are parsed and unmasked *in place* in the read
//!   buffer, consumed by advancing a cursor (no `drain` memmove per frame); the
//!   buffer starts small (idle connections cost ~1 KiB), grows to the size a
//!   frame header announces in one step, and shrinks back after a big message.
//! - **Word-wide unmasking** ([`frame::unmask`]): 8 bytes per XOR.
//! - **Vectored, coalesced writes.** A writer task per connection takes the whole
//!   outbox (double-buffered, no allocation per batch) and writes it with one
//!   `writev` — no copy into a joined buffer.
//! - **Encode once, fan out free.** [`PreparedMessage`] holds an encoded frame in
//!   an `Rc<[u8]>`; sending it to 10 000 spectators queues 10 000 pointers.
//! - **Bounded outbox = backpressure.** A client that cannot keep up gets
//!   [`SendError::Full`]; the caller drops the update (spectator) or closes the
//!   client (player, `1008`). Memory never grows without bound.
//!
//! Everything is `!Send` by design: a connection lives on one core.

use std::cell::RefCell;
use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use kernway_core::error::StatusCode;
use kernway_core::request::Request;
use kernway_core::response::{Response, Upgraded, UpgradedRead, UpgradedWrite};

use crate::frame::{self, close_code, opcode};
use crate::handshake::{self, HandshakeError};
use crate::heartbeat::{self, Beat};

/// Server-side limits and timers.
#[derive(Debug, Clone)]
pub struct WsConfig {
    /// Largest assembled message (bytes); bigger → close `1009`.
    pub max_message: usize,
    /// Largest single frame (bytes); bigger → close `1009`.
    pub max_frame: usize,
    /// Most data frames waiting in a connection's outbox before `send_*` says `Full`.
    pub send_queue: usize,
    /// Send a ping this often (`None` = never). Keeps NATs/proxies from dropping idle sockets.
    pub ping_interval: Option<Duration>,
    /// Close a client that sends nothing (not even a pong) for this long; also
    /// the grace period for the client to answer our close frame.
    pub idle_timeout: Duration,
    /// Subprotocols the server speaks, in preference order (`Sec-WebSocket-Protocol`).
    pub protocols: Vec<String>,
    /// Initial read-buffer size per connection (it grows on demand).
    pub read_buffer: usize,
}

impl Default for WsConfig {
    fn default() -> Self {
        Self {
            max_message: 1 << 20,
            max_frame: 1 << 20,
            send_queue: 256,
            ping_interval: Some(Duration::from_secs(25)),
            idle_timeout: Duration::from_secs(60),
            protocols: Vec::new(),
            read_buffer: 1024,
        }
    }
}

/// A data message from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// UTF-8 text (validated).
    Text(String),
    /// Binary data.
    Binary(Vec<u8>),
}

/// Why a send did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    /// The outbox is at [`WsConfig::send_queue`]: the client is not keeping up.
    Full,
    /// The connection is closing or closed.
    Closed,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SendError::Full => "websocket send queue full",
            SendError::Closed => "websocket closed",
        })
    }
}

impl std::error::Error for SendError {}

/// Answer a WebSocket upgrade request.
///
/// A valid handshake gets `101 Switching Protocols` and, once it is on the wire,
/// `on_open` runs with the connection (on this shard) until it returns — the
/// connection is then closed normally. An invalid one gets `400`, or `426` with
/// `Sec-WebSocket-Version: 13` for a version mismatch. Authentication, origin
/// checks and rate limits belong in the route/middleware before this call.
pub fn upgrade<F, Fut>(req: &Request, config: WsConfig, on_open: F) -> Response
where
    F: FnOnce(WebSocket) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let accept = match handshake::validate(&req.method, |name| req.header(name)) {
        Ok(accept) => accept,
        Err(HandshakeError::Version) => {
            let mut r = Response::new(StatusCode(426)).content_type("text/plain; charset=utf-8");
            r.headers.insert("sec-websocket-version", "13");
            return r.body(HandshakeError::Version.to_string());
        }
        Err(e) => {
            return Response::new(StatusCode::BAD_REQUEST)
                .content_type("text/plain; charset=utf-8")
                .body(e.to_string());
        }
    };
    let protocol = pick_protocol(req.header("sec-websocket-protocol"), &config.protocols);
    let mut response = Response::new(StatusCode::SWITCHING_PROTOCOLS);
    response.headers.insert("upgrade", "websocket");
    response.headers.insert("sec-websocket-accept", &accept);
    if let Some(p) = &protocol {
        response.headers.insert("sec-websocket-protocol", p);
    }
    response.upgrade(move |up| Box::pin(run(up, config, protocol, on_open)))
}

/// The first protocol we speak that the client offered.
fn pick_protocol(offered: Option<&str>, ours: &[String]) -> Option<String> {
    let offered: Vec<&str> = offered?.split(',').map(str::trim).collect();
    ours.iter().find(|p| offered.contains(&p.as_str())).cloned()
}

/// Drive one upgraded connection: start the writer, join the shard heartbeat,
/// run the app, then close.
async fn run<F, Fut>(up: Upgraded, config: WsConfig, protocol: Option<String>, on_open: F)
where
    F: FnOnce(WebSocket) -> Fut,
    Fut: Future<Output = ()>,
{
    let Upgraded {
        reader,
        writer,
        buffered,
        remote_addr,
    } = up;
    let now = heartbeat::coarse_now();
    let conn = Rc::new(Conn(RefCell::new(Outbox::new(&config, now))));
    rt_core::spawn(writer_task(writer, Rc::clone(&conn)));
    let finest = config
        .ping_interval
        .map_or(config.idle_timeout, |p| p.min(config.idle_timeout));
    let beat: Rc<dyn Beat> = Rc::clone(&conn) as Rc<dyn Beat>;
    heartbeat::register(Rc::downgrade(&beat), finest);
    drop(beat);

    let sender = WsSender {
        conn: Rc::clone(&conn),
    };
    let mut data = buffered;
    let initial = config.read_buffer.max(64);
    let end = data.len();
    if data.len() < initial {
        data.resize(initial, 0);
    }
    let ws = WebSocket {
        reader: WsReader {
            io: reader,
            data,
            start: 0,
            end,
            initial,
            conn: Rc::clone(&conn),
            max_frame: config.max_frame,
            max_message: config.max_message,
            partial: None,
            done: false,
        },
        sender: sender.clone(),
        protocol,
        remote_addr,
    };
    on_open(ws).await;
    sender.close(close_code::NORMAL, "");
}

// ------------------------------------------------------------------ connection state

/// Per-connection state shared by the reader, every [`WsSender`] clone, the
/// writer task and the shard heartbeat (all on one core, hence `RefCell`).
struct Conn(RefCell<Outbox>);

struct Outbox {
    /// Encoded frames waiting for the writer. A `Vec`, swapped wholesale with the
    /// writer's spare one (double-buffering): no allocation per batch.
    frames: Vec<Chunk>,
    /// Data frames queued (control frames do not count against the cap).
    data: usize,
    cap: usize,
    /// A close frame is queued or sent: no more data.
    closing: bool,
    /// When `closing` started (the peer gets `idle` to answer).
    closing_since: Option<Instant>,
    /// The socket failed, or the connection was torn down.
    dead: bool,
    /// Last time the reader received anything (coarse clock).
    last_seen: Instant,
    idle: Duration,
    ping_every: Option<Duration>,
    next_ping: Option<Instant>,
    writer: Option<Waker>,
    /// The reader parks here while waiting for bytes, so a teardown can wake it.
    reader: Option<Waker>,
}

enum Chunk {
    Data(Vec<u8>),
    Shared(Rc<[u8]>),
    Control(Vec<u8>),
}

impl Chunk {
    fn bytes(&self) -> &[u8] {
        match self {
            Chunk::Data(b) | Chunk::Control(b) => b,
            Chunk::Shared(b) => b,
        }
    }
}

impl Outbox {
    fn new(config: &WsConfig, now: Instant) -> Self {
        Self {
            frames: Vec::new(),
            data: 0,
            cap: config.send_queue.max(1),
            closing: false,
            closing_since: None,
            dead: false,
            last_seen: now,
            idle: config.idle_timeout,
            ping_every: config.ping_interval,
            next_ping: config.ping_interval.map(|p| now + p),
            writer: None,
            reader: None,
        }
    }

    fn wake_writer(&mut self) {
        if let Some(w) = self.writer.take() {
            w.wake();
        }
    }

    fn push_data(&mut self, chunk: Chunk) -> Result<(), SendError> {
        if self.closing || self.dead {
            return Err(SendError::Closed);
        }
        if self.data >= self.cap {
            return Err(SendError::Full);
        }
        self.data += 1;
        self.frames.push(chunk);
        self.wake_writer();
        Ok(())
    }

    fn push_control(&mut self, op: u8, payload: &[u8]) {
        if self.closing || self.dead {
            return;
        }
        let mut f = Vec::with_capacity(payload.len() + 2);
        frame::encode_server_frame(&mut f, op, payload);
        self.frames.push(Chunk::Control(f));
        self.wake_writer();
    }

    /// Queue a close frame (once) — nothing is sent after it.
    fn push_close(&mut self, code: u16, reason: &str) {
        if self.closing || self.dead {
            return;
        }
        // 1005 ("no status") is never sent: answer it with an empty close.
        let payload = if code == 1005 {
            Vec::new()
        } else {
            frame::close_payload(code, reason)
        };
        let mut f = Vec::with_capacity(payload.len() + 2);
        frame::encode_server_frame(&mut f, opcode::CLOSE, &payload);
        self.frames.push(Chunk::Control(f));
        self.closing = true;
        self.closing_since = Some(heartbeat::coarse_now());
        self.wake_writer();
    }

    /// Tear the connection down now: drop queued frames, stop the writer, wake
    /// the reader so `recv` returns `None`.
    fn kill(&mut self) {
        self.dead = true;
        self.closing = true;
        self.frames.clear();
        self.data = 0;
        self.wake_writer();
        if let Some(r) = self.reader.take() {
            r.wake();
        }
    }
}

impl Beat for Conn {
    fn beat(&self, now: Instant) -> bool {
        let mut o = self.0.borrow_mut();
        if o.dead {
            return false;
        }
        if let Some(since) = o.closing_since {
            // We sent a close; the peer has `idle` to answer before we drop it.
            if now.duration_since(since) >= o.idle {
                o.kill();
                return false;
            }
            return true;
        }
        if now.duration_since(o.last_seen) >= o.idle {
            o.push_close(close_code::GOING_AWAY, "idle timeout");
            return true; // stays registered for the close grace period
        }
        if let (Some(every), Some(due)) = (o.ping_every, o.next_ping) {
            if now >= due {
                o.push_control(opcode::PING, b"");
                o.next_ping = Some(now + every);
            }
        }
        true
    }
}

/// Write queued frames: take the whole outbox (swap with a spare `Vec`) and send
/// it with one vectored write. Ends after the close frame is out, or on failure.
async fn writer_task(mut io: Box<dyn UpgradedWrite>, conn: Rc<Conn>) {
    let mut batch: Vec<Chunk> = Vec::new();
    loop {
        let more = poll_fn(|cx| {
            let mut o = conn.0.borrow_mut();
            if o.dead {
                return Poll::Ready(false);
            }
            if o.frames.is_empty() {
                if o.closing {
                    return Poll::Ready(false); // the close frame has been written
                }
                o.writer = Some(cx.waker().clone());
                return Poll::Pending;
            }
            std::mem::swap(&mut o.frames, &mut batch);
            let data = batch
                .iter()
                .filter(|c| !matches!(c, Chunk::Control(_)))
                .count();
            o.data -= data;
            Poll::Ready(true)
        })
        .await;
        if !more {
            break;
        }
        let ok = {
            let slices: Vec<&[u8]> = batch.iter().map(Chunk::bytes).collect();
            io.write_all_vectored(&slices).await.is_ok()
        };
        batch.clear(); // keeps its capacity: it becomes the next spare
        if !ok {
            conn.0.borrow_mut().kill();
            break;
        }
    }
    io.shutdown();
}

// ------------------------------------------------------------------ public handles

/// A frame encoded once, sent to many connections without re-encoding or
/// copying the payload per connection.
#[derive(Clone)]
pub struct PreparedMessage(Rc<[u8]>);

impl PreparedMessage {
    /// A text message.
    #[must_use]
    pub fn text(text: &str) -> Self {
        Self::encode(opcode::TEXT, text.as_bytes())
    }

    /// A binary message.
    #[must_use]
    pub fn binary(data: &[u8]) -> Self {
        Self::encode(opcode::BINARY, data)
    }

    fn encode(op: u8, payload: &[u8]) -> Self {
        let mut f = Vec::with_capacity(payload.len() + 10);
        frame::encode_server_frame(&mut f, op, payload);
        Self(f.into())
    }

    /// Encoded size on the wire.
    #[must_use]
    pub fn wire_len(&self) -> usize {
        self.0.len()
    }
}

/// Sends to one connection. Cheap to clone; stays on the connection's shard.
#[derive(Clone)]
pub struct WsSender {
    conn: Rc<Conn>,
}

impl WsSender {
    /// Queue a text message.
    pub fn send_text(&self, text: &str) -> Result<(), SendError> {
        self.send_data(opcode::TEXT, text.as_bytes())
    }

    /// Queue a binary message.
    pub fn send_binary(&self, data: &[u8]) -> Result<(), SendError> {
        self.send_data(opcode::BINARY, data)
    }

    /// Queue a pre-encoded message (shared bytes — for broadcasts).
    pub fn send_prepared(&self, msg: &PreparedMessage) -> Result<(), SendError> {
        self.conn
            .0
            .borrow_mut()
            .push_data(Chunk::Shared(Rc::clone(&msg.0)))
    }

    fn send_data(&self, op: u8, payload: &[u8]) -> Result<(), SendError> {
        let mut o = self.conn.0.borrow_mut();
        // Check before encoding: a full or closed outbox costs no allocation.
        if o.closing || o.dead {
            return Err(SendError::Closed);
        }
        if o.data >= o.cap {
            return Err(SendError::Full);
        }
        let mut f = Vec::with_capacity(payload.len() + 10);
        frame::encode_server_frame(&mut f, op, payload);
        o.push_data(Chunk::Data(f))
    }

    /// Start the closing handshake with `code` (see [`close_code`]). Idempotent.
    pub fn close(&self, code: u16, reason: &str) {
        self.conn.0.borrow_mut().push_close(code, reason);
    }

    /// Closing or closed — further sends fail with [`SendError::Closed`].
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.conn.0.borrow().closing
    }

    /// Data messages waiting to be written (how far behind this client is).
    #[must_use]
    pub fn queued(&self) -> usize {
        self.conn.0.borrow().data
    }

    /// Whether two senders reach the same connection.
    #[must_use]
    pub fn same_connection(&self, other: &WsSender) -> bool {
        Rc::ptr_eq(&self.conn, &other.conn)
    }
}

/// Receives client messages. Driven by one task.
pub struct WsReader {
    io: Box<dyn UpgradedRead>,
    /// Read buffer; live bytes are `data[start..end]`.
    data: Vec<u8>,
    start: usize,
    end: usize,
    initial: usize,
    conn: Rc<Conn>,
    max_frame: usize,
    max_message: usize,
    /// A fragmented message being assembled: (opcode, bytes so far).
    partial: Option<(u8, Vec<u8>)>,
    done: bool,
}

enum Step {
    Message(Message),
    Continue,
    End,
}

impl WsReader {
    /// The next text/binary message, or `None` once the connection is closed
    /// (by the client, by a protocol error, an idle timeout, or the socket).
    /// Pings are answered and pongs absorbed here.
    pub async fn recv(&mut self) -> Option<Message> {
        while !self.done {
            let live = &self.data[self.start..self.end];
            match frame::parse_client_header(live, self.max_frame) {
                Err(e) => self.fail(e.code, e.reason),
                Ok(Some(h)) if live.len() >= h.frame_len() => {
                    let (s, e) = (self.start + h.payload_start, self.start + h.frame_len());
                    frame::unmask(&mut self.data[s..e], h.mask);
                    let step = self.on_frame(h.fin, h.opcode, s, e);
                    self.consume(h.frame_len());
                    match step {
                        Step::Message(m) => return Some(m),
                        Step::Continue => {}
                        Step::End => self.done = true,
                    }
                }
                // Header known: make room for the whole frame in one step.
                Ok(Some(h)) => self.fill(h.frame_len()).await,
                Ok(None) => self.fill(14).await, // the largest client header
            }
        }
        None
    }

    /// Advance the cursor; an emptied buffer resets (and shrinks if a big
    /// message grew it) without moving any bytes.
    fn consume(&mut self, n: usize) {
        self.start += n;
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
            if self.data.len() > self.initial.max(64 * 1024) {
                self.data.truncate(self.initial);
                self.data.shrink_to_fit();
            }
        }
    }

    /// Read until at least `need` live bytes are buffered (or the connection ends).
    async fn fill(&mut self, need: usize) {
        if self.data.len() - self.start < need {
            // Compact once, then grow to fit the whole frame.
            self.data.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
            if self.data.len() < need {
                self.data.resize(need.next_power_of_two(), 0);
            }
        }
        let conn = &self.conn;
        let mut read = self.io.read(&mut self.data[self.end..]);
        let got = poll_fn(|cx| {
            {
                let mut o = conn.0.borrow_mut();
                if o.dead {
                    return Poll::Ready(None);
                }
                if !o.reader.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
                    o.reader = Some(cx.waker().clone());
                }
            }
            Pin::new(&mut read).poll(cx).map(Some)
        })
        .await;
        drop(read);
        match got {
            Some(Ok(n)) if n > 0 => {
                self.end += n;
                self.conn.0.borrow_mut().last_seen = heartbeat::coarse_now();
            }
            // EOF / error without a close frame (1006), or torn down by the heartbeat.
            _ => {
                self.conn.0.borrow_mut().kill();
                self.done = true;
            }
        }
    }

    fn fail(&mut self, code: u16, reason: &str) {
        self.conn.0.borrow_mut().push_close(code, reason);
        self.done = true;
    }

    /// Handle one unmasked frame whose payload is `data[s..e]`.
    fn on_frame(&mut self, fin: bool, op: u8, s: usize, e: usize) -> Step {
        match op {
            opcode::PING => {
                let payload = &self.data[s..e];
                self.conn.0.borrow_mut().push_control(opcode::PONG, payload);
                Step::Continue
            }
            opcode::PONG => Step::Continue,
            opcode::CLOSE => {
                let mut o = self.conn.0.borrow_mut();
                match frame::parse_close(&self.data[s..e]) {
                    // Echo the peer's code, as the closing handshake asks (§5.5.1).
                    Ok(code) => o.push_close(code, ""),
                    Err(err) => o.push_close(err.code, err.reason),
                }
                Step::End
            }
            opcode::TEXT | opcode::BINARY => {
                if self.partial.is_some() {
                    self.fail(close_code::PROTOCOL, "expected continuation");
                    return Step::End;
                }
                if e - s > self.max_message {
                    self.fail(close_code::TOO_BIG, "message too big");
                    return Step::End;
                }
                if fin {
                    let payload = self.data[s..e].to_vec(); // the one copy
                    self.finish(op, payload)
                } else {
                    self.partial = Some((op, self.data[s..e].to_vec()));
                    Step::Continue
                }
            }
            _ => {
                // continuation
                let Some((first_op, mut acc)) = self.partial.take() else {
                    self.fail(close_code::PROTOCOL, "unexpected continuation");
                    return Step::End;
                };
                if acc.len() + (e - s) > self.max_message {
                    self.fail(close_code::TOO_BIG, "message too big");
                    return Step::End;
                }
                acc.extend_from_slice(&self.data[s..e]);
                if fin {
                    self.finish(first_op, acc)
                } else {
                    self.partial = Some((first_op, acc));
                    Step::Continue
                }
            }
        }
    }

    fn finish(&mut self, op: u8, payload: Vec<u8>) -> Step {
        if op == opcode::TEXT {
            match String::from_utf8(payload) {
                Ok(t) => Step::Message(Message::Text(t)),
                Err(_) => {
                    self.fail(close_code::INVALID_DATA, "invalid utf-8");
                    Step::End
                }
            }
        } else {
            Step::Message(Message::Binary(payload))
        }
    }
}

/// An open WebSocket connection, handed to the `on_open` task of [`upgrade`].
pub struct WebSocket {
    reader: WsReader,
    sender: WsSender,
    protocol: Option<String>,
    remote_addr: Option<std::net::SocketAddr>,
}

impl WebSocket {
    /// The next message from the client — see [`WsReader::recv`].
    pub async fn recv(&mut self) -> Option<Message> {
        self.reader.recv().await
    }

    /// A sender for this connection (clone it into rooms / broadcast lists).
    #[must_use]
    pub fn sender(&self) -> WsSender {
        self.sender.clone()
    }

    /// Separate the reading half from the sending half.
    #[must_use]
    pub fn split(self) -> (WsReader, WsSender) {
        (self.reader, self.sender)
    }

    /// The negotiated subprotocol, if any.
    #[must_use]
    pub fn protocol(&self) -> Option<&str> {
        self.protocol.as_deref()
    }

    /// The client's address, when known.
    #[must_use]
    pub fn remote_addr(&self) -> Option<std::net::SocketAddr> {
        self.remote_addr
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_first_supported_protocol() {
        let ours = vec!["xq.v2".to_string(), "xq.v1".to_string()];
        assert_eq!(
            pick_protocol(Some("xq.v1, xq.v2"), &ours).as_deref(),
            Some("xq.v2")
        );
        assert_eq!(pick_protocol(Some("chat"), &ours), None);
        assert_eq!(pick_protocol(None, &ours), None);
    }

    #[test]
    fn outbox_caps_data_but_not_control_frames() {
        let config = WsConfig {
            send_queue: 2,
            ..WsConfig::default()
        };
        let mut o = Outbox::new(&config, Instant::now());
        assert!(o.push_data(Chunk::Data(vec![1])).is_ok());
        assert!(o.push_data(Chunk::Data(vec![2])).is_ok());
        assert_eq!(o.push_data(Chunk::Data(vec![3])), Err(SendError::Full));
        o.push_control(opcode::PONG, b"");
        assert_eq!(o.frames.len(), 3);
        o.push_close(1000, "bye");
        assert_eq!(o.push_data(Chunk::Data(vec![4])), Err(SendError::Closed));
        o.push_close(1000, "again"); // idempotent
        assert_eq!(o.frames.len(), 4);
    }

    #[test]
    fn heartbeat_pings_then_closes_idle_then_drops_after_grace() {
        let config = WsConfig {
            ping_interval: Some(Duration::from_secs(10)),
            idle_timeout: Duration::from_secs(30),
            ..WsConfig::default()
        };
        let t0 = Instant::now();
        let conn = Conn(RefCell::new(Outbox::new(&config, t0)));
        assert!(conn.beat(t0 + Duration::from_secs(5)));
        assert!(conn.0.borrow().frames.is_empty());
        assert!(conn.beat(t0 + Duration::from_secs(10))); // ping due
        assert_eq!(conn.0.borrow().frames.len(), 1);
        assert!(conn.beat(t0 + Duration::from_secs(31))); // idle → close queued
        assert!(conn.0.borrow().closing);
        // Grace for the peer's close reply, measured from when we closed.
        let since = conn.0.borrow().closing_since.unwrap();
        assert!(!conn.beat(since + Duration::from_secs(31)));
        assert!(conn.0.borrow().dead);
    }
}
