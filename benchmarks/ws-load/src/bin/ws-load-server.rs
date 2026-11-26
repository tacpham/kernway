//! `ws-load-server` — the server side of the WebSocket load test.
//!
//! - `GET /echo` — every message is sent straight back to its sender.
//! - `GET /room/{id}` — every message a member sends is encoded **once**
//!   ([`PreparedMessage`]) and queued to every member of that room *on the same
//!   shard* (sender included). A member whose outbox is full
//!   ([`SendError::Full`]) misses that message and the drop is counted. The
//!   first frame a member receives is a text `hello pid=… slot=… shard=… room=…`
//!   so the client knows which shard-local group it is in.
//! - `GET /stats` — JSON counters (connections, messages in/out, drops), totals
//!   and per shard.
//!
//! Rooms are a thread-local `HashMap` per shard: cross-shard rooms are out of
//! scope (the game service routes them through `rt_net::CoreMesh`).
//!
//! ```text
//! ws-load-server [--host 127.0.0.1] [--port 9001]... [--workers N]
//!                [--send-queue 256] [--idle-timeout-secs 60]
//! ```
//! `--port` may repeat (or be comma-separated): one `KernwayApp` per port, each
//! with `--workers` shards, all in this process. On Linux that multiplies the
//! connections one client IP can open (ports are unique per 4-tuple); on macOS
//! it is how to use several cores (`SO_REUSEPORT` does not balance there).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Duration;

use di_core::RequestScope;
use kernway_core::error::StatusCode;
use kernway_core::request::Request;
use kernway_core::response::Response;
use kernway_server::KernwayApp;
use kernway_ws::{Message, PreparedMessage, SendError, WebSocket, WsConfig};

// ------------------------------------------------------------------ stats

/// One shard's counters, on its own cache lines. Only the owning shard thread
/// writes a slot (plain load+store, no locked RMW on the hot path); `/stats`
/// reads every slot.
#[repr(align(128))]
struct Slot {
    conns_open: AtomicU64,
    conns_total: AtomicU64,
    msgs_in: AtomicU64,
    msgs_out: AtomicU64,
    drops: AtomicU64,
    send_closed: AtomicU64,
}

const MAX_SLOTS: usize = 512;
#[allow(clippy::declare_interior_mutable_const)] // only used as an array initialiser
const EMPTY: Slot = Slot {
    conns_open: AtomicU64::new(0),
    conns_total: AtomicU64::new(0),
    msgs_in: AtomicU64::new(0),
    msgs_out: AtomicU64::new(0),
    drops: AtomicU64::new(0),
    send_closed: AtomicU64::new(0),
};
static SLOTS: [Slot; MAX_SLOTS] = [EMPTY; MAX_SLOTS];
static NEXT_SLOT: AtomicUsize = AtomicUsize::new(0);
/// `(port, shard index)` of each claimed slot.
static LABELS: Mutex<Vec<(u16, usize)>> = Mutex::new(Vec::new());

thread_local! {
    static MY_SLOT: Cell<usize> = const { Cell::new(usize::MAX) };
    static ROOMS: RefCell<HashMap<String, Vec<kernway_ws::WsSender>>> = RefCell::new(HashMap::new());
}

fn bump(c: &AtomicU64, by: u64) {
    // Single writer per slot: no need for an atomic read-modify-write.
    c.store(c.load(Relaxed).wrapping_add(by), Relaxed);
}

fn slot() -> &'static Slot {
    let i = MY_SLOT.with(Cell::get);
    &SLOTS[i.min(MAX_SLOTS - 1)]
}

fn claim_slot(port: u16, shard: usize) {
    let i = NEXT_SLOT.fetch_add(1, Relaxed);
    assert!(i < MAX_SLOTS, "too many shards");
    LABELS.lock().unwrap().push((port, shard));
    MY_SLOT.with(|s| s.set(i));
}

fn stats_json() -> String {
    let labels = LABELS.lock().unwrap().clone();
    let mut shards = Vec::new();
    let mut tot = [0u64; 6];
    for (i, (port, shard)) in labels.iter().enumerate() {
        let s = &SLOTS[i];
        let v = [
            s.conns_open.load(Relaxed),
            s.conns_total.load(Relaxed),
            s.msgs_in.load(Relaxed),
            s.msgs_out.load(Relaxed),
            s.drops.load(Relaxed),
            s.send_closed.load(Relaxed),
        ];
        for (t, x) in tot.iter_mut().zip(v) {
            *t += x;
        }
        shards.push(format!(
            r#"{{"slot":{i},"port":{port},"shard":{shard},"conns_open":{},"conns_total":{},"msgs_in":{},"msgs_out":{},"drops":{},"send_closed":{}}}"#,
            v[0], v[1], v[2], v[3], v[4], v[5]
        ));
    }
    format!(
        r#"{{"pid":{},"conns_open":{},"conns_total":{},"msgs_in":{},"msgs_out":{},"drops":{},"send_closed":{},"shards":[{}]}}"#,
        std::process::id(),
        tot[0],
        tot[1],
        tot[2],
        tot[3],
        tot[4],
        tot[5],
        shards.join(",")
    )
}

/// Counts a connection open for as long as it lives (also on a panic/abort).
struct OpenGuard;

impl OpenGuard {
    fn new() -> Self {
        let s = slot();
        bump(&s.conns_open, 1);
        bump(&s.conns_total, 1);
        OpenGuard
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        bump(&slot().conns_open, u64::MAX); // wrapping -1
    }
}

fn count_send(r: Result<(), SendError>) -> bool {
    let s = slot();
    match r {
        Ok(()) => bump(&s.msgs_out, 1),
        Err(SendError::Full) => bump(&s.drops, 1),
        Err(SendError::Closed) => {
            bump(&s.send_closed, 1);
            return false;
        }
    }
    true
}

// ------------------------------------------------------------------ handlers

async fn echo(mut ws: WebSocket) {
    let _open = OpenGuard::new();
    let tx = ws.sender();
    while let Some(m) = ws.recv().await {
        bump(&slot().msgs_in, 1);
        let r = match m {
            Message::Text(t) => tx.send_text(&t),
            Message::Binary(b) => tx.send_binary(&b),
        };
        if !count_send(r) {
            break;
        }
    }
}

async fn room(id: String, mut ws: WebSocket) {
    let _open = OpenGuard::new();
    let tx = ws.sender();
    ROOMS.with(|r| {
        r.borrow_mut()
            .entry(id.clone())
            .or_default()
            .push(tx.clone())
    });
    let hello = format!(
        "hello pid={} slot={} shard={} room={id}",
        std::process::id(),
        MY_SLOT.with(Cell::get),
        rt_net::shard_index().unwrap_or(0),
    );
    let _ = tx.send_text(&hello);
    while let Some(m) = ws.recv().await {
        bump(&slot().msgs_in, 1);
        // Encode once; every member gets a pointer to the same bytes.
        let frame = match &m {
            Message::Text(t) => PreparedMessage::text(t),
            Message::Binary(b) => PreparedMessage::binary(b),
        };
        let (mut out, mut drops, mut closed) = (0u64, 0u64, 0u64);
        ROOMS.with(|r| {
            if let Some(members) = r.borrow().get(&id) {
                for member in members {
                    match member.send_prepared(&frame) {
                        Ok(()) => out += 1,
                        Err(SendError::Full) => drops += 1,
                        Err(SendError::Closed) => closed += 1,
                    }
                }
            }
        });
        let s = slot();
        bump(&s.msgs_out, out);
        bump(&s.drops, drops);
        bump(&s.send_closed, closed);
    }
    ROOMS.with(|r| {
        let mut rooms = r.borrow_mut();
        if let Some(members) = rooms.get_mut(&id) {
            members.retain(|m| !m.same_connection(&tx));
            if members.is_empty() {
                rooms.remove(&id);
            }
        }
    });
}

// ------------------------------------------------------------------ main

struct Args {
    host: String,
    ports: Vec<u16>,
    workers: Option<usize>,
    send_queue: usize,
    idle_timeout: Duration,
}

fn parse_args() -> Args {
    let mut a = Args {
        host: "127.0.0.1".into(),
        ports: Vec::new(),
        workers: None,
        send_queue: WsConfig::default().send_queue,
        idle_timeout: WsConfig::default().idle_timeout,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || {
            it.next()
                .unwrap_or_else(|| die(&format!("{flag} needs a value")))
        };
        match flag.as_str() {
            "--host" => a.host = val(),
            "--port" | "--ports" => {
                for p in val().split(',') {
                    a.ports
                        .push(p.trim().parse().unwrap_or_else(|_| die("bad --port")));
                }
            }
            "--workers" => a.workers = Some(val().parse().unwrap_or_else(|_| die("bad --workers"))),
            "--send-queue" => {
                a.send_queue = val().parse().unwrap_or_else(|_| die("bad --send-queue"))
            }
            "--idle-timeout-secs" => {
                a.idle_timeout = Duration::from_secs(
                    val()
                        .parse()
                        .unwrap_or_else(|_| die("bad --idle-timeout-secs")),
                );
            }
            "-h" | "--help" => {
                println!(
                    "ws-load-server [--host 127.0.0.1] [--port 9001]... [--workers N] \
                     [--send-queue 256] [--idle-timeout-secs 60]"
                );
                std::process::exit(0);
            }
            other => die(&format!("unknown flag {other}")),
        }
    }
    if a.ports.is_empty() {
        a.ports.push(9001);
    }
    a
}

fn die(msg: &str) -> ! {
    eprintln!("ws-load-server: {msg}");
    std::process::exit(2);
}

fn build_app(args: &Args, port: u16) -> KernwayApp {
    let config = WsConfig {
        send_queue: args.send_queue,
        idle_timeout: args.idle_timeout,
        ..WsConfig::default()
    };
    let echo_cfg = config.clone();
    let mut b = KernwayApp::builder()
        .bind(&format!("{}:{port}", args.host))
        .on_shard_start(move |i| claim_slot(port, i))
        .get("/echo", move |req: Request, _s: &RequestScope| {
            let r = kernway_ws::upgrade(&req, echo_cfg.clone(), echo);
            async move { r }
        })
        .get("/room/{id}", move |req: Request, _s: &RequestScope| {
            let id = req.path_params.get("id").cloned().unwrap_or_default();
            let r = kernway_ws::upgrade(&req, config.clone(), move |ws| room(id, ws));
            async move { r }
        })
        .get("/stats", |_req: Request, _s: &RequestScope| {
            let r = Response::new(StatusCode::OK)
                .content_type("application/json")
                .body(stats_json());
            async move { r }
        });
    if let Some(w) = args.workers {
        b = b.workers(w);
    }
    b.build()
}

fn main() {
    ws_load::sys::report_fd_limit("ws-load-server");
    let args = parse_args();
    eprintln!(
        "[ws-load-server] pid={} ports={:?} send_queue={} idle_timeout={:?}",
        std::process::id(),
        args.ports,
        args.send_queue,
        args.idle_timeout
    );
    let apps: Vec<KernwayApp> = args.ports.iter().map(|&p| build_app(&args, p)).collect();
    let stops: Vec<_> = apps.iter().map(KernwayApp::shutdown_handle).collect();
    if let Err(e) = rt_core::on_interrupt(move || stops.iter().for_each(|s| s.trigger())) {
        eprintln!("[ws-load-server] no Ctrl+C handler: {e}");
    }
    let threads: Vec<_> = apps
        .into_iter()
        .map(|app| std::thread::spawn(move || app.run_until_shutdown()))
        .collect();
    for t in threads {
        match t.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => eprintln!("[ws-load-server] server error: {e}"),
            Err(_) => eprintln!("[ws-load-server] server thread panicked"),
        }
    }
}
