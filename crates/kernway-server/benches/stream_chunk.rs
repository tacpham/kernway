//! Large-file streaming — sweep the chunk size to set `FILE_CHUNK` from a
//! measurement, not a guess (the KEP-0002 TODO).
//!
//! Serves one large file over a real socket at each candidate chunk size and
//! measures download throughput. Each chunk is a `spawn_blocking` read plus a
//! socket write, so a small chunk means more thread-pool hops and syscalls per
//! byte; a large chunk means fewer, at the cost of per-connection memory. This
//! finds where throughput stops improving — the smallest chunk on the plateau is
//! the right default.
//!
//! Not a criterion micro-bench (moving 128 MiB many times is the wrong shape) —
//! its own `main`. Run: `cargo bench -p kernway-server --bench stream_chunk`.
//! Size via `KERNWAY_BENCH_MB` (default 128), reps via `KERNWAY_BENCH_REPS` (5).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Instant;

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

/// Download `/big.bin` once, returning (bytes_of_body, elapsed_seconds).
fn download(port: u16) -> (usize, f64) {
    // Retry the connect: the server thread may not have finished binding yet.
    let mut sock = {
        let mut s = None;
        for _ in 0..1000 {
            if let Ok(c) = TcpStream::connect(("127.0.0.1", port)) {
                s = Some(c);
                break;
            }
            std::thread::yield_now();
        }
        s.expect("server never came up")
    };
    sock.write_all(b"GET /big.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();

    let start = Instant::now();
    let mut buf = vec![0u8; 256 * KIB];
    let mut total = 0usize;
    loop {
        match sock.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) => panic!("read error: {e}"),
        }
    }
    let secs = start.elapsed().as_secs_f64();
    (total, secs)
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
    let file_bytes = mb * MIB;

    // One temp file, reused across every chunk size.
    let dir = std::env::temp_dir().join(format!("kernway-stream-bench-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("big.bin");
    {
        // Write the file in blocks so we do not hold 128 MiB twice in memory.
        let mut f = std::fs::File::create(&path).unwrap();
        let block = vec![b'k'; MIB];
        for _ in 0..mb {
            f.write_all(&block).unwrap();
        }
        f.flush().unwrap();
    }

    let chunk_sizes = [
        64 * KIB,
        128 * KIB,
        256 * KIB,
        512 * KIB,
        MIB,
        2 * MIB,
        4 * MIB,
        8 * MIB,
        16 * MIB,
    ];

    println!("\nStreaming {mb} MiB over loopback, {reps} reps each, best of.\n");
    println!("{:>10}   {:>10}   {:>10}", "chunk", "best MB/s", "GiB/s");
    println!("{:>10}   {:>10}   {:>10}", "-----", "---------", "-----");

    for &cs in &chunk_sizes {
        let port = free_port();
        let app = KernwayApp::builder()
            .bind(&format!("127.0.0.1:{port}"))
            .workers(1) // one shard: measure a single connection on one core, no noise
            .static_files(&dir)
            .file_chunk_size(cs)
            .build();
        let stop = app.shutdown_handle();
        let server = std::thread::spawn(move || app.run_until_shutdown());

        let mut best_mbps = 0.0f64;
        for _ in 0..reps {
            let (got, secs) = download(port);
            assert!(got >= file_bytes, "short read: got {got} of {file_bytes}");
            let mbps = (file_bytes as f64 / MIB as f64) / secs;
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

    std::fs::remove_dir_all(&dir).ok();
    println!();
}
