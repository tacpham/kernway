//! `ws-load` — WebSocket load generator built on rt-core / rt-net.
//!
//! N client shards (threads), each one rt-core `Executor` driving thousands of
//! connections with a minimal masked-frame RFC 6455 client. Scenarios:
//!
//! ```text
//! ws-load idle   --conns N [--hold-secs 10]
//! ws-load echo   --conns N --rate R --duration S [--size B]
//! ws-load fanout --rooms K --members M --rate R --duration S [--size B]
//!
//! common: --connect 127.0.0.1:9001 (repeatable / comma list; connections are
//!         spread round-robin) --bind 127.0.0.2,.. (source IPv4s, round-robin;
//!         each has its own ephemeral port range) --threads 4 --concurrency 32 (in-flight
//!         handshakes per thread) --server-pid PID (else read from /stats)
//!         --stats host:port (repeatable; default: every --connect target)
//!         --drain-secs 2
//! ```
//!
//! Latency is measured from a timestamp the client writes into each message
//! (one process-wide monotonic clock, so a stamp written on one client thread
//! is read on another). Each message carries two stamps: when it was
//! *scheduled* (open-loop pacing; latency from it is corrected for
//! coordinated omission) and when it was actually *written*.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::future::poll_fn;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::rc::Rc;
use std::sync::mpsc;
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use rt_core::{mailbox, MailboxReceiver, MailboxSender};
use rt_net::stream::{TcpReadHalf, TcpWriteHalf};
use rt_net::AsyncTcpStream;
use ws_load::hist::Histogram;
use ws_load::sys;
use ws_load::wire::{self, op, Rng};

// ------------------------------------------------------------------ args

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Idle,
    Echo,
    Fanout,
}

#[derive(Clone, Debug)]
struct Args {
    kind: Kind,
    targets: Vec<SocketAddr>,
    /// Source IPv4 addresses to bind (round-robin); empty = let the kernel pick.
    binds: Vec<std::net::Ipv4Addr>,
    stats: Vec<SocketAddr>,
    conns: usize,
    rooms: usize,
    members: usize,
    rate: f64,
    duration: Duration,
    size: usize,
    hold: Duration,
    threads: usize,
    concurrency: usize,
    server_pid: Option<u32>,
    drain: Duration,
}

const USAGE: &str = "usage:
  ws-load idle   --conns N [--hold-secs 10]
  ws-load echo   --conns N --rate R(msg/s per conn) --duration S [--size 64]
  ws-load fanout --rooms K --members M --rate R(msg/s per sender) --duration S [--size 64]
common: --connect HOST:PORT[,..] (repeatable)  --bind SRC_IPV4[,..] (repeatable)
        --threads 4  --concurrency 32
        --server-pid PID  --stats HOST:PORT (repeatable)  --drain-secs 2";

fn die(msg: &str) -> ! {
    eprintln!("ws-load: {msg}\n{USAGE}");
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let kind = match it.next().as_deref() {
        Some("idle") => Kind::Idle,
        Some("echo") => Kind::Echo,
        Some("fanout") => Kind::Fanout,
        Some("-h" | "--help") => {
            println!("{USAGE}");
            std::process::exit(0);
        }
        _ => die("first argument must be a scenario: idle | echo | fanout"),
    };
    let mut a = Args {
        kind,
        targets: Vec::new(),
        binds: Vec::new(),
        stats: Vec::new(),
        conns: 1000,
        rooms: 10,
        members: 100,
        rate: 10.0,
        duration: Duration::from_secs(10),
        size: 64,
        hold: Duration::from_secs(10),
        threads: 4,
        concurrency: 32,
        server_pid: None,
        drain: Duration::from_secs(2),
    };
    let addrs = |v: String| -> Vec<SocketAddr> {
        v.split(',')
            .flat_map(|s| {
                s.trim()
                    .to_socket_addrs()
                    .unwrap_or_else(|e| die(&format!("bad address {s}: {e}")))
                    .take(1)
            })
            .collect()
    };
    while let Some(flag) = it.next() {
        let mut val = || {
            it.next()
                .unwrap_or_else(|| die(&format!("{flag} needs a value")))
        };
        fn num<T: std::str::FromStr>(flag: &str, v: String) -> T {
            v.parse()
                .unwrap_or_else(|_| die(&format!("bad value for {flag}: {v}")))
        }
        match flag.as_str() {
            "--connect" => a.targets.extend(addrs(val())),
            "--stats" => a.stats.extend(addrs(val())),
            "--bind" => {
                for ip in val().split(',') {
                    a.binds.push(num(&flag, ip.trim().to_string()));
                }
            }
            "--conns" => a.conns = num(&flag, val()),
            "--rooms" => a.rooms = num(&flag, val()),
            "--members" => a.members = num(&flag, val()),
            "--rate" => a.rate = num(&flag, val()),
            "--duration" => a.duration = Duration::from_secs_f64(num(&flag, val())),
            "--size" => a.size = num(&flag, val()),
            "--hold-secs" => a.hold = Duration::from_secs_f64(num(&flag, val())),
            "--threads" => a.threads = num::<usize>(&flag, val()).max(1),
            "--concurrency" => a.concurrency = num::<usize>(&flag, val()).max(1),
            "--server-pid" => a.server_pid = Some(num(&flag, val())),
            "--drain-secs" => a.drain = Duration::from_secs_f64(num(&flag, val())),
            other => die(&format!("unknown flag {other}")),
        }
    }
    if a.targets.is_empty() {
        a.targets.push("127.0.0.1:9001".parse().unwrap());
    }
    if a.stats.is_empty() {
        a.stats = a.targets.clone();
        a.stats.dedup();
    }
    if a.kind == Kind::Fanout {
        a.conns = a.rooms * a.members;
    }
    if a.kind != Kind::Idle && a.size < wire::STAMP_LEN {
        die(&format!("--size must be at least {}", wire::STAMP_LEN));
    }
    if a.kind != Kind::Idle && a.rate <= 0.0 {
        die("--rate must be positive");
    }
    a
}

// ------------------------------------------------------------------ messages

/// Main → client shard.
enum Cmd {
    /// Run the measured phase: `senders` (global connection ids owned by this
    /// shard) send from `t0_ns` until `end_ns`; report `drain` after `end_ns`.
    Start {
        t0_ns: u64,
        end_ns: u64,
        senders: Vec<usize>,
    },
    /// Close every connection and exit.
    Stop,
}

/// Client shard → main.
enum Event {
    Connected {
        ok: usize,
        failed: usize,
        errors: Vec<String>,
        connect: Histogram,
        /// `(global id, group key)`: for fanout, the server-side room group a
        /// member landed in (`pid/slot/room`).
        groups: Vec<(usize, String)>,
    },
    Report(Box<Report>),
}

#[derive(Default)]
struct Report {
    rtt: Histogram,
    sched: Histogram,
    received: u64,
    unstamped: u64,
    /// `(global id, messages sent)` per sending connection.
    sent: Vec<(usize, u64)>,
    disconnects: u64,
}

// ------------------------------------------------------------------ connection

/// One client connection: the reader task parses server frames; the writer
/// task flushes `out` whenever something (a pacer, a pong) appended to it.
struct Conn {
    out: RefCell<Vec<u8>>,
    writer: Cell<Option<Waker>>,
    dead: Cell<bool>,
}

impl Conn {
    fn push(&self, opcode: u8, payload: &[u8], mask: [u8; 4]) {
        wire::encode_client_frame(&mut self.out.borrow_mut(), opcode, payload, mask);
        if let Some(w) = self.writer.take() {
            w.wake();
        }
    }
}

/// Shard-wide state shared by every connection task on one client thread.
struct Shard {
    stopping: Cell<bool>,
    rtt: RefCell<Histogram>,
    sched: RefCell<Histogram>,
    received: Cell<u64>,
    unstamped: Cell<u64>,
    disconnects: Cell<u64>,
    rng: RefCell<Rng>,
}

impl Shard {
    fn mask(&self) -> [u8; 4] {
        (self.rng.borrow_mut().next_u64() as u32).to_ne_bytes()
    }

    fn on_data(&self, payload: &[u8]) {
        let now = wire::now_ns();
        match wire::read_stamp(payload) {
            Some((sched, sent)) => {
                self.received.set(self.received.get() + 1);
                self.rtt.borrow_mut().record(now.saturating_sub(sent));
                self.sched.borrow_mut().record(now.saturating_sub(sched));
            }
            None => self.unstamped.set(self.unstamped.get() + 1),
        }
    }
}

async fn writer_task(mut io: TcpWriteHalf, conn: Rc<Conn>, shard: Rc<Shard>) {
    let mut batch = Vec::new();
    loop {
        let more = poll_fn(|cx| {
            if conn.dead.get() {
                return Poll::Ready(false);
            }
            let mut out = conn.out.borrow_mut();
            if out.is_empty() {
                conn.writer.set(Some(cx.waker().clone()));
                return Poll::Pending;
            }
            std::mem::swap(&mut *out, &mut batch);
            Poll::Ready(true)
        })
        .await;
        if !more {
            break;
        }
        if io.write_all(&batch).await.is_err() {
            if !conn.dead.replace(true) && !shard.stopping.get() {
                shard.disconnects.set(shard.disconnects.get() + 1);
            }
            break;
        }
        batch.clear();
    }
}

async fn reader_task(mut io: TcpReadHalf, conn: Rc<Conn>, shard: Rc<Shard>, mut buf: Vec<u8>) {
    let (mut start, mut end) = (0usize, buf.len());
    if buf.len() < 2048 {
        buf.resize(2048, 0);
    }
    let mut closing = false;
    loop {
        // Handle every complete frame.
        loop {
            match wire::parse_server_frame(&buf[start..end]) {
                Ok(Some(f)) => {
                    let p = start + f.payload.start..start + f.payload.end;
                    match f.opcode {
                        op::PING => conn.push(op::PONG, &buf[p], shard.mask()),
                        op::CLOSE => {
                            if !closing {
                                closing = true;
                                conn.push(op::CLOSE, &buf[p], shard.mask());
                            }
                        }
                        op::TEXT | op::BINARY => shard.on_data(&buf[p]),
                        _ => {}
                    }
                    start += f.len;
                }
                Ok(None) => break,
                Err(_) => {
                    end = start; // protocol error: treat as a disconnect
                    break;
                }
            }
        }
        // Compact, grow if a frame header announced more than fits.
        if start == end {
            start = 0;
            end = 0;
        } else if start > 0 {
            buf.copy_within(start..end, 0);
            end -= start;
            start = 0;
        }
        if end == buf.len() {
            buf.resize(buf.len() * 2, 0);
        }
        match io.read(&mut buf[end..]).await {
            Ok(n) if n > 0 => end += n,
            _ => {
                if !conn.dead.replace(true) && !closing && !shard.stopping.get() {
                    shard.disconnects.set(shard.disconnects.get() + 1);
                }
                if let Some(w) = conn.writer.take() {
                    w.wake();
                }
                return;
            }
        }
    }
}

/// Send timed messages open-loop: message `k` is due at `first + k*interval`
/// whatever happened to message `k-1`.
async fn pacer_task(
    conn: Rc<Conn>,
    shard: Rc<Shard>,
    first_ns: u64,
    interval_ns: u64,
    end_ns: u64,
    size: usize,
    sent: Rc<Cell<u64>>,
) {
    let mut payload = vec![0x5au8; size];
    let mut due = first_ns;
    while due < end_ns {
        rt_core::time::sleep_until(wire::instant_at(due)).await;
        if conn.dead.get() || shard.stopping.get() {
            return;
        }
        wire::stamp(&mut payload, due, wire::now_ns());
        conn.push(op::BINARY, &payload, shard.mask());
        sent.set(sent.get() + 1);
        due += interval_ns;
    }
}

// ------------------------------------------------------------------ connect

struct Opened {
    reader: TcpReadHalf,
    writer: TcpWriteHalf,
    leftover: Vec<u8>,
    group: Option<String>,
}

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// TCP connect + HTTP upgrade (+ the room's hello for fanout).
async fn open(
    addr: SocketAddr,
    src: Option<std::net::Ipv4Addr>,
    path: &str,
    want_hello: bool,
) -> Result<Opened, String> {
    let mut s = match src {
        None => AsyncTcpStream::connect(addr)
            .await
            .map_err(|e| format!("connect: {e}"))?,
        Some(ip) => {
            // Bind-before-connect is not in rt-net's API: start the connect
            // by hand and poll for completion (adds <= one timer tick to the
            // measured connect latency on this path).
            let std = sys::start_connect_from(ip, addr).map_err(|e| format!("connect: {e}"))?;
            loop {
                if let Some(e) = std.take_error().map_err(|e| format!("connect: {e}"))? {
                    return Err(format!("connect: {e}"));
                }
                match std.peer_addr() {
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::NotConnected => {
                        rt_core::sleep(Duration::from_micros(200)).await;
                    }
                    Err(e) => return Err(format!("connect: {e}")),
                }
            }
            AsyncTcpStream::from_std(std).map_err(|e| format!("register: {e}"))?
        }
    };
    let _ = s.set_nodelay(true);
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {KEY}\r\n\r\n"
    );
    s.write_all(req.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p + 4;
        }
        let n = s.read(&mut chunk).await.map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Err("closed during handshake".into());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    if !buf.starts_with(b"HTTP/1.1 101") {
        let line = String::from_utf8_lossy(&buf[..head_end]);
        return Err(format!("handshake: {}", line.lines().next().unwrap_or("")));
    }
    buf.drain(..head_end);
    let mut group = None;
    if want_hello {
        loop {
            if let Ok(Some(f)) = wire::parse_server_frame(&buf) {
                if f.opcode == op::TEXT {
                    let text = String::from_utf8_lossy(&buf[f.payload.clone()]).into_owned();
                    let field = |k: &str| {
                        text.split(' ')
                            .find_map(|kv| kv.strip_prefix(k))
                            .unwrap_or("?")
                            .to_string()
                    };
                    group = Some(format!(
                        "{}/{}/{}",
                        field("pid="),
                        field("slot="),
                        field("room=")
                    ));
                    buf.drain(..f.len);
                    break;
                }
                buf.drain(..f.len); // a ping before the hello: ignore
                continue;
            }
            let n = s.read(&mut chunk).await.map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                return Err("closed before hello".into());
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    }
    let (reader, writer) = s.into_split();
    Ok(Opened {
        reader,
        writer,
        leftover: buf,
        group,
    })
}

// ------------------------------------------------------------------ client shard

/// A count-down latch for tasks on one executor.
#[derive(Default)]
struct Latch {
    left: Cell<usize>,
    waker: Cell<Option<Waker>>,
}

impl Latch {
    fn done(&self) {
        self.left.set(self.left.get() - 1);
        if self.left.get() == 0 {
            if let Some(w) = self.waker.take() {
                w.wake();
            }
        }
    }
    async fn wait(&self) {
        poll_fn(|cx| {
            if self.left.get() == 0 {
                Poll::Ready(())
            } else {
                self.waker.set(Some(cx.waker().clone()));
                Poll::Pending
            }
        })
        .await;
    }
}

struct Plan {
    /// Global connection ids this shard opens.
    ids: Vec<usize>,
    args: Args,
}

/// Connections are spread round-robin over `--connect` targets — except in
/// fanout, where a whole room goes to one target so it can share a shard.
fn target_for(args: &Args, g: usize) -> SocketAddr {
    let n = args.targets.len();
    match args.kind {
        Kind::Fanout => args.targets[(g / args.members) % n],
        _ => args.targets[g % n],
    }
}

fn path_for(args: &Args, g: usize) -> String {
    match args.kind {
        Kind::Idle | Kind::Echo => "/echo".into(),
        Kind::Fanout => format!("/room/r{}", g / args.members),
    }
}

async fn shard_main(
    index: usize,
    plan: Plan,
    mut cmds: MailboxReceiver<Cmd>,
    events: mpsc::Sender<Event>,
) {
    let args = Rc::new(plan.args);
    let shard = Rc::new(Shard {
        stopping: Cell::new(false),
        rtt: RefCell::new(Histogram::new()),
        sched: RefCell::new(Histogram::new()),
        received: Cell::new(0),
        unstamped: Cell::new(0),
        disconnects: Cell::new(0),
        rng: RefCell::new(Rng::new(
            wire::now_ns() ^ ((index as u64 + 1) * 0x9E37_79B9_7F4A_7C15),
        )),
    });

    // --- connect phase: `concurrency` workers pull ids from a shared cursor.
    let ids = Rc::new(plan.ids);
    let next = Rc::new(Cell::new(0usize));
    let conns: Rc<RefCell<HashMap<usize, Rc<Conn>>>> = Rc::new(RefCell::new(HashMap::new()));
    let connect = Rc::new(RefCell::new(Histogram::new()));
    let errors: Rc<RefCell<HashMap<String, usize>>> = Rc::new(RefCell::new(HashMap::new()));
    let groups: Rc<RefCell<Vec<(usize, String)>>> = Rc::new(RefCell::new(Vec::new()));
    let workers = args.concurrency.min(ids.len()).max(1);
    let latch = Rc::new(Latch::default());
    latch.left.set(workers);
    for _ in 0..workers {
        let (ids, next, conns, connect, errors, groups, latch, args, shard) = (
            Rc::clone(&ids),
            Rc::clone(&next),
            Rc::clone(&conns),
            Rc::clone(&connect),
            Rc::clone(&errors),
            Rc::clone(&groups),
            Rc::clone(&latch),
            Rc::clone(&args),
            Rc::clone(&shard),
        );
        rt_core::spawn(async move {
            loop {
                let i = next.get();
                if i >= ids.len() {
                    break;
                }
                next.set(i + 1);
                let g = ids[i];
                let target = target_for(&args, g);
                let t = wire::now_ns();
                let res = rt_core::timeout(
                    Duration::from_secs(15),
                    open(
                        target,
                        (!args.binds.is_empty()).then(|| args.binds[g % args.binds.len()]),
                        &path_for(&args, g),
                        args.kind == Kind::Fanout,
                    ),
                )
                .await
                .unwrap_or_else(|_| Err("timeout".into()));
                match res {
                    Ok(o) => {
                        connect.borrow_mut().record(wire::now_ns() - t);
                        if let Some(key) = o.group {
                            groups.borrow_mut().push((g, key));
                        }
                        let conn = Rc::new(Conn {
                            out: RefCell::new(Vec::new()),
                            writer: Cell::new(None),
                            dead: Cell::new(false),
                        });
                        rt_core::spawn(writer_task(o.writer, Rc::clone(&conn), Rc::clone(&shard)));
                        rt_core::spawn(reader_task(
                            o.reader,
                            Rc::clone(&conn),
                            Rc::clone(&shard),
                            o.leftover,
                        ));
                        conns.borrow_mut().insert(g, conn);
                    }
                    Err(e) => *errors.borrow_mut().entry(e).or_default() += 1,
                }
            }
            latch.done();
        });
    }
    latch.wait().await;
    let failed: usize = errors.borrow().values().sum();
    let _ = events.send(Event::Connected {
        ok: conns.borrow().len(),
        failed,
        errors: errors
            .borrow()
            .iter()
            .map(|(e, n)| format!("{n} x {e}"))
            .collect(),
        connect: connect.borrow().clone(),
        groups: std::mem::take(&mut *groups.borrow_mut()),
    });

    // --- measured phase(s), then stop.
    while let Some(cmd) = cmds.recv().await {
        match cmd {
            Cmd::Start {
                t0_ns,
                end_ns,
                senders,
            } => {
                // Fresh counters for this phase.
                shard.rtt.replace(Histogram::new());
                shard.sched.replace(Histogram::new());
                shard.received.set(0);
                shard.unstamped.set(0);
                let interval = (1e9 / args.rate) as u64;
                let mut sent = Vec::new();
                for g in senders {
                    let Some(conn) = conns.borrow().get(&g).cloned() else {
                        continue;
                    };
                    let counter = Rc::new(Cell::new(0u64));
                    // Random phase so connections do not send in lockstep.
                    let phase = (shard.rng.borrow_mut().next_f64() * interval as f64) as u64;
                    rt_core::spawn(pacer_task(
                        conn,
                        Rc::clone(&shard),
                        t0_ns + phase,
                        interval,
                        end_ns,
                        args.size,
                        Rc::clone(&counter),
                    ));
                    sent.push((g, counter));
                }
                rt_core::time::sleep_until(wire::instant_at(end_ns) + args.drain).await;
                let report = Report {
                    rtt: shard.rtt.borrow().clone(),
                    sched: shard.sched.borrow().clone(),
                    received: shard.received.get(),
                    unstamped: shard.unstamped.get(),
                    sent: sent.iter().map(|(g, c)| (*g, c.get())).collect(),
                    disconnects: shard.disconnects.get(),
                };
                let _ = events.send(Event::Report(Box::new(report)));
            }
            Cmd::Stop => break,
        }
    }
    shard.stopping.set(true);
    for conn in conns.borrow().values() {
        conn.push(op::CLOSE, &1000u16.to_be_bytes(), shard.mask());
    }
    rt_core::sleep(Duration::from_millis(300)).await;
}

// ------------------------------------------------------------------ server stats

#[derive(Default, Clone, Copy, Debug)]
struct ServerStats {
    conns_open: u64,
    msgs_in: u64,
    msgs_out: u64,
    drops: u64,
    send_closed: u64,
}

fn fetch_stats(addr: SocketAddr) -> Result<(u32, ServerStats, serde_json::Value), String> {
    let mut s =
        TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    write!(
        s,
        "GET /stats HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    let _ = s.read_to_end(&mut raw);
    let at = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("no http head")?;
    let v: serde_json::Value =
        serde_json::from_slice(&raw[at + 4..]).map_err(|e| format!("bad /stats json: {e}"))?;
    let n = |k: &str| v[k].as_u64().unwrap_or(0);
    let st = ServerStats {
        conns_open: n("conns_open"),
        msgs_in: n("msgs_in"),
        msgs_out: n("msgs_out"),
        drops: n("drops"),
        send_closed: n("send_closed"),
    };
    Ok((n("pid") as u32, st, v))
}

/// Sum `/stats` over every distinct server process.
fn all_stats(addrs: &[SocketAddr]) -> (Vec<u32>, ServerStats, Vec<serde_json::Value>) {
    let mut pids = Vec::new();
    let mut total = ServerStats::default();
    let mut raw = Vec::new();
    for &a in addrs {
        match fetch_stats(a) {
            Ok((pid, s, v)) if !pids.contains(&pid) => {
                pids.push(pid);
                total.conns_open += s.conns_open;
                total.msgs_in += s.msgs_in;
                total.msgs_out += s.msgs_out;
                total.drops += s.drops;
                total.send_closed += s.send_closed;
                raw.push(v);
            }
            Ok(_) => {}
            Err(e) => eprintln!("[ws-load] /stats from {a} failed: {e}"),
        }
    }
    (pids, total, raw)
}

fn total_rss(pids: &[u32]) -> Option<u64> {
    pids.iter().map(|&p| sys::rss_kib(p)).sum()
}

fn total_cpu(pids: &[u32]) -> Option<f64> {
    pids.iter().map(|&p| sys::cpu_secs(p)).sum()
}

// ------------------------------------------------------------------ main

fn ms(ns: u64) -> String {
    format!("{:.3}", ns as f64 / 1e6)
}

fn print_latency(label: &str, h: &Histogram) {
    println!(
        "  {label:<26} p50 {} | p90 {} | p99 {} | p99.9 {} | max {} ms  (n={}, mean {} ms)",
        ms(h.quantile(0.50)),
        ms(h.quantile(0.90)),
        ms(h.quantile(0.99)),
        ms(h.quantile(0.999)),
        ms(h.max()),
        h.count(),
        ms(h.mean() as u64),
    );
}

fn main() {
    let fd_soft = sys::report_fd_limit("ws-load");
    let args = parse_args();
    wire::now_ns(); // fix the epoch before any thread stamps
    if (args.conns as u64) + 64 > fd_soft {
        eprintln!(
            "[ws-load] warning: {} connections need more fds than the limit {fd_soft}",
            args.conns
        );
    }
    let mut per_target: HashMap<SocketAddr, usize> = HashMap::new();
    for g in 0..args.conns {
        *per_target.entry(target_for(&args, g)).or_default() += 1;
    }
    let per_target = per_target.values().copied().max().unwrap_or(0);
    let sources = args.binds.len().max(1);
    if cfg!(target_os = "macos") && args.conns / sources > 16_000 {
        eprintln!(
            "[ws-load] warning: macOS takes ephemeral ports from ONE range per source IP \
             (default 49152-65535 = 16384) whatever the destination port; use --bind with \
             lo0 aliases or lower net.inet.ip.portrange.first"
        );
    } else if per_target / sources > 28_000 {
        eprintln!(
            "[ws-load] warning: {per_target} connections to one server port per source IP; \
             Linux's default ip_local_port_range (32768-60999) has 28232 ports"
        );
    }
    println!(
        "scenario={:?} conns={} targets={:?} threads={} concurrency/thread={}",
        args.kind, args.conns, args.targets, args.threads, args.concurrency
    );

    let (pids0, stats0, _) = all_stats(&args.stats);
    let pids: Vec<u32> = args.server_pid.map_or(pids0, |p| vec![p]);
    if pids.is_empty() {
        eprintln!("[ws-load] warning: no server pid (no /stats, no --server-pid): no RSS/CPU");
    }
    let rss0 = total_rss(&pids);

    // --- spawn client shards
    let (ev_tx, ev_rx) = mpsc::channel::<Event>();
    let mut cmd_txs: Vec<MailboxSender<Cmd>> = Vec::new();
    let mut threads = Vec::new();
    let connect_started = Instant::now();
    for index in 0..args.threads {
        let ids: Vec<usize> = (index..args.conns).step_by(args.threads).collect();
        let plan = Plan {
            ids,
            args: args.clone(),
        };
        let (tx, rx) = mailbox::<Cmd>(8);
        cmd_txs.push(tx);
        let events = ev_tx.clone();
        threads.push(
            std::thread::Builder::new()
                .name(format!("ws-load-{index}"))
                .spawn(move || {
                    let ex = rt_core::Executor::new().expect("executor");
                    ex.block_on(shard_main(index, plan, rx, events))
                        .expect("client shard");
                })
                .expect("spawn"),
        );
    }
    drop(ev_tx);

    // --- connect phase
    let mut connect = Histogram::new();
    let (mut ok, mut failed) = (0usize, 0usize);
    let mut errors = Vec::new();
    let mut groups: Vec<(usize, String)> = Vec::new();
    for _ in 0..args.threads {
        match ev_rx.recv() {
            Ok(Event::Connected {
                ok: o,
                failed: f,
                errors: e,
                connect: h,
                groups: g,
            }) => {
                ok += o;
                failed += f;
                errors.extend(e);
                connect.merge(&h);
                groups.extend(g);
            }
            _ => die("a client shard died while connecting"),
        }
    }
    let connect_secs = connect_started.elapsed().as_secs_f64();
    println!("\n== connect");
    println!(
        "  opened {ok}/{} in {connect_secs:.2}s = {:.0} conn/s, failed {failed}",
        args.conns,
        ok as f64 / connect_secs
    );
    for e in errors.iter().take(5) {
        println!("  error: {e}");
    }
    print_latency("connect+upgrade latency", &connect);
    // Where the connections landed (macOS SO_REUSEPORT does not balance).
    let (_, _, raw) = all_stats(&args.stats);
    let spread: Vec<String> = raw
        .iter()
        .flat_map(|v| v["shards"].as_array().cloned().unwrap_or_default())
        .filter(|s| s["conns_open"].as_u64().unwrap_or(0) > 0)
        .map(|s| {
            format!(
                "{}:{}={}",
                s["port"],
                s["shard"],
                s["conns_open"].as_u64().unwrap_or(0)
            )
        })
        .collect();
    println!(
        "  server conns_open per port:shard (non-empty): {}",
        spread.join(" ")
    );

    // --- idle: settle, then sample RSS
    if args.kind == Kind::Idle {
        std::thread::sleep(Duration::from_secs(2));
        let (_, s1, _) = all_stats(&args.stats);
        let rss1 = total_rss(&pids);
        let client_rss = sys::rss_kib(std::process::id());
        let t0 = wire::now_ns();
        let end = t0 + args.hold.as_nanos() as u64;
        for tx in &cmd_txs {
            let _ = tx.try_send(Cmd::Start {
                t0_ns: t0,
                end_ns: end,
                senders: Vec::new(),
            });
        }
        let mut disconnects = 0;
        for _ in 0..args.threads {
            if let Ok(Event::Report(r)) = ev_rx.recv() {
                disconnects += r.disconnects;
            }
        }
        let rss2 = total_rss(&pids);
        let (_, s2, _) = all_stats(&args.stats);
        println!("\n== idle (held {:?})", args.hold);
        println!(
            "  server conns_open: before {} -> connected {} -> end of hold {}",
            stats0.conns_open, s1.conns_open, s2.conns_open
        );
        if let (Some(a), Some(b), Some(c)) = (rss0, rss1, rss2) {
            let per = (b as f64 - a as f64) * 1024.0 / ok.max(1) as f64;
            println!("  server RSS: before {a} KiB -> connected {b} KiB -> end of hold {c} KiB");
            println!(
                "  server RSS delta per connection: {per:.0} bytes ({:.2} KiB)",
                per / 1024.0
            );
        }
        if let Some(c) = client_rss {
            println!(
                "  client RSS: {c} KiB ({:.2} KiB/conn)",
                c as f64 / ok.max(1) as f64
            );
        }
        println!("  unexpected disconnects during hold: {disconnects}");
    } else {
        // --- echo / fanout: pick senders, run, collect
        let mut group_size: HashMap<String, usize> = HashMap::new();
        let mut sender_of_group: HashMap<String, usize> = HashMap::new();
        for (g, key) in &groups {
            *group_size.entry(key.clone()).or_default() += 1;
            let e = sender_of_group.entry(key.clone()).or_insert(*g);
            *e = (*e).min(*g);
        }
        let mut fan: HashMap<usize, usize> = HashMap::new(); // sender -> receivers
        let mut senders_by_shard: Vec<Vec<usize>> = vec![Vec::new(); args.threads];
        match args.kind {
            Kind::Echo => {
                for g in 0..args.conns {
                    senders_by_shard[g % args.threads].push(g);
                    fan.insert(g, 1);
                }
            }
            _ => {
                for (key, &g) in &sender_of_group {
                    senders_by_shard[g % args.threads].push(g);
                    fan.insert(g, group_size[key]);
                }
                let mut sizes: Vec<usize> = group_size.values().copied().collect();
                sizes.sort_unstable();
                println!(
                    "  fanout groups (room x server shard): {} groups, size min {} / median {} / max {}",
                    sizes.len(),
                    sizes.first().unwrap_or(&0),
                    sizes.get(sizes.len() / 2).unwrap_or(&0),
                    sizes.last().unwrap_or(&0)
                );
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        let (_, s1, _) = all_stats(&args.stats);
        let cpu_srv0 = total_cpu(&pids);
        let cpu_cli0 = sys::self_cpu_secs();
        let t0 = wire::now_ns() + 200_000_000;
        let end = t0 + args.duration.as_nanos() as u64;
        for (tx, senders) in cmd_txs.iter().zip(senders_by_shard) {
            let _ = tx.try_send(Cmd::Start {
                t0_ns: t0,
                end_ns: end,
                senders,
            });
        }
        // CPU is sampled at the end of the send window, not after the drain.
        let wait = wire::instant_at(end).saturating_duration_since(Instant::now());
        std::thread::sleep(wait);
        let cpu_srv1 = total_cpu(&pids);
        let cpu_cli1 = sys::self_cpu_secs();
        let rss_run = total_rss(&pids);
        let mut rtt = Histogram::new();
        let mut sched = Histogram::new();
        let (mut received, mut unstamped, mut disconnects) = (0u64, 0u64, 0u64);
        let mut sent_total = 0u64;
        let mut expected = 0u64;
        for _ in 0..args.threads {
            if let Ok(Event::Report(r)) = ev_rx.recv() {
                rtt.merge(&r.rtt);
                sched.merge(&r.sched);
                received += r.received;
                unstamped += r.unstamped;
                disconnects += r.disconnects;
                for (g, n) in r.sent {
                    sent_total += n;
                    expected += n * fan.get(&g).copied().unwrap_or(0) as u64;
                }
            }
        }
        let (_, s2, _) = all_stats(&args.stats);
        let secs = args.duration.as_secs_f64();
        let label = if args.kind == Kind::Echo {
            "echo"
        } else {
            "fanout"
        };
        println!(
            "\n== {label} ({:.0}s, size {} B, rate {}/s per sender)",
            secs, args.size, args.rate
        );
        println!(
            "  sent {sent_total} msgs = {:.0} msg/s (target {:.0})",
            sent_total as f64 / secs,
            fan.len() as f64 * args.rate
        );
        println!(
            "  received {received} of {expected} expected = {:.0} msg/s; missing {} ({:.3}%)",
            received as f64 / secs,
            expected.saturating_sub(received),
            100.0 * expected.saturating_sub(received) as f64 / expected.max(1) as f64
        );
        let lat = if args.kind == Kind::Echo {
            "round trip"
        } else {
            "delivery"
        };
        print_latency(&format!("{lat} (from write)"), &rtt);
        print_latency(&format!("{lat} (from schedule)"), &sched);
        println!(
            "  server: msgs_in +{} msgs_out +{} drops(outbox full) +{} send_closed +{} conns_open {}",
            s2.msgs_in - s1.msgs_in,
            s2.msgs_out - s1.msgs_out,
            s2.drops - s1.drops,
            s2.send_closed - s1.send_closed,
            s2.conns_open
        );
        if let (Some(a), Some(b)) = (cpu_srv0, cpu_srv1) {
            println!(
                "  server CPU: {:.2} core-seconds/s during the send window",
                (b - a) / secs
            );
        }
        println!(
            "  client CPU: {:.2} core-seconds/s (all {} client threads)",
            (cpu_cli1 - cpu_cli0) / secs,
            args.threads
        );
        if let Some(r) = rss_run {
            println!("  server RSS at end of window: {r} KiB");
        }
        println!("  unstamped frames {unstamped}, unexpected disconnects {disconnects}");
    }

    for tx in &cmd_txs {
        let _ = tx.try_send(Cmd::Stop);
    }
    for t in threads {
        let _ = t.join();
    }
}
