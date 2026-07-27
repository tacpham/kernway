//! Large-upload streaming — sweep the read/write chunk size to see inbound throughput,
//! and confirm the server streams a multi-GB body to disk at O(chunk) memory rather than
//! buffering it (the inbound mirror of `stream_chunk`).
//!
//! POSTs one large body over a real socket at each candidate `file_chunk` size and
//! measures upload throughput. The body is streamed straight to a temp file on the
//! blocking pool, so per-connection memory is a single chunk, independent of the body
//! size — raising the chunk trades syscalls/thread-pool hops against that memory.
//!
//! Not a criterion micro-bench (moving hundreds of MiB many times is the wrong shape) —
//! its own `main`. Run: `cargo bench -p kernway-server --bench upload_chunk`.
//! Size via `KERNWAY_BENCH_MB` (default 128), reps via `KERNWAY_BENCH_REPS` (5).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Instant;

use di_core::RequestScope;
use kernway_core::{error::StatusCode, request::Request, response::Response};
use kernway_server::KernwayApp;

const KIB: usize = 1024;
const MIB: usize = 1024 * 1024;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn connect_retry(port: u16) -> TcpStream {
    for _ in 0..1000 {
        if let Ok(c) = TcpStream::connect(("127.0.0.1", port)) {
            return c;
        }
        std::thread::yield_now();
    }
    panic!("server never came up");
}

/// Upload `total` bytes to `/up` once, returning (bytes_sent, elapsed_seconds).
fn upload(port: u16, total: usize) -> (usize, f64) {
    let mut sock = connect_retry(port);
    let head = format!(
        "POST /up HTTP/1.1\r\nHost: x\r\ncontent-length: {total}\r\nConnection: close\r\n\r\n"
    );

    let start = Instant::now();
    sock.write_all(head.as_bytes()).unwrap();
    let block = vec![b'u'; MIB];
    let mut sent = 0usize;
    while sent < total {
        let take = (total - sent).min(block.len());
        sock.write_all(&block[..take]).unwrap();
        sent += take;
    }
    let mut resp = Vec::new();
    sock.read_to_end(&mut resp).unwrap();
    let secs = start.elapsed().as_secs_f64();

    let text = String::from_utf8_lossy(&resp);
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "upload failed: {}",
        &text[..text.len().min(80)]
    );
    (sent, secs)
}

fn main() {
    let mb: usize = std::env::var("KERNWAY_BENCH_MB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(128);
    let reps: usize = std::env::var("KERNWAY_BENCH_REPS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let total = mb * MIB;

    let chunk_sizes = [
        64 * KIB,
        128 * KIB,
        256 * KIB,
        512 * KIB,
        MIB,
        2 * MIB,
        4 * MIB,
        8 * MIB,
    ];

    println!("\nUploading {mb} MiB over loopback, {reps} reps each, best of.");
    println!("(body streamed to a temp file — per-connection memory is one chunk)\n");
    println!("{:>10}   {:>10}   {:>10}", "chunk", "best MB/s", "GiB/s");
    println!("{:>10}   {:>10}   {:>10}", "-----", "---------", "-----");

    for &cs in &chunk_sizes {
        let port = free_port();
        let app = KernwayApp::builder()
            .bind(&format!("127.0.0.1:{port}"))
            .workers(1) // one shard: a single connection on one core, no noise
            .file_chunk_size(cs)
            .max_inmemory_body(0) // force every body to stream to disk
            .post("/up", |req: Request, _ctx: &RequestScope| async move {
                let n = req
                    .body_spool
                    .as_ref()
                    .map(|s| s.len)
                    .unwrap_or(req.body.len() as u64);
                Response::new(StatusCode::OK).body(format!("{n}").into_bytes())
            })
            .build();
        let stop = app.shutdown_handle();
        let server = std::thread::spawn(move || app.run_until_shutdown());

        let mut best_mbps = 0.0f64;
        for _ in 0..reps {
            let (got, secs) = upload(port, total);
            assert_eq!(got, total, "short write");
            let mbps = (total as f64 / MIB as f64) / secs;
            if mbps > best_mbps {
                best_mbps = mbps;
            }
        }

        stop.trigger();
        let _ = server.join().unwrap();

        let label = if cs >= MIB {
            format!("{} MiB", cs / MIB)
        } else {
            format!("{} KiB", cs / KIB)
        };
        println!(
            "{:>10}   {:>10.0}   {:>10.2}",
            label,
            best_mbps,
            best_mbps / 1024.0
        );
    }

    println!();
}
