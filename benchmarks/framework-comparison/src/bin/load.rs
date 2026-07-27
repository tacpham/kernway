//! Concurrent HTTP/1.1 keep-alive load generator for the framework comparison.
//!
//! Usage:
//!   load <host:port> <download|upload|multipart> <concurrency> <secs> [payload_mib]
//!
//! Each worker holds one keep-alive connection and issues requests back-to-back for
//! the duration, so this measures a framework's steady-state throughput under N
//! concurrent connections — the regime where thread-per-core either pays off or does
//! not. Reports requests/s and MB/s (body bytes for download, sent bytes for upload).

use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MIB: usize = 1024 * 1024;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!("usage: load <host:port> <download|upload|multipart> <concurrency> <secs> [payload_mib]");
        std::process::exit(2);
    }
    let addr = args[1].clone();
    let mode = args[2].clone();
    let concurrency: usize = args[3].parse().expect("concurrency");
    let secs: u64 = args[4].parse().expect("secs");
    let payload_mib: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(8);

    // Build the request bytes once, shared across workers.
    let request: Arc<Vec<u8>> = Arc::new(build_request(&mode, payload_mib));
    // For upload/multipart the "transferred" bytes are what we send; for download,
    // what we receive (filled in per response).
    let sent_per_req = if mode == "download" { 0 } else { request.len() as u64 };

    let stop = Arc::new(AtomicBool::new(false));
    let total_reqs = Arc::new(AtomicU64::new(0));
    let total_bytes = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));

    let start = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..concurrency {
        let addr = addr.clone();
        let request = Arc::clone(&request);
        let stop = Arc::clone(&stop);
        let total_reqs = Arc::clone(&total_reqs);
        let total_bytes = Arc::clone(&total_bytes);
        let errors = Arc::clone(&errors);
        let download = mode == "download";
        handles.push(std::thread::spawn(move || {
            let Ok(stream) = TcpStream::connect(&addr) else {
                errors.fetch_add(1, Ordering::Relaxed);
                return;
            };
            stream.set_nodelay(true).ok();
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);
            let mut reqs = 0u64;
            let mut bytes = 0u64;
            while !stop.load(Ordering::Relaxed) {
                if writer.write_all(&request).is_err() {
                    errors.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                match read_response(&mut reader) {
                    Ok(body_len) => {
                        reqs += 1;
                        bytes += if download { body_len as u64 } else { sent_per_req };
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                }
            }
            total_reqs.fetch_add(reqs, Ordering::Relaxed);
            total_bytes.fetch_add(bytes, Ordering::Relaxed);
        }));
    }

    std::thread::sleep(Duration::from_secs(secs));
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().ok();
    }
    let elapsed = start.elapsed().as_secs_f64();

    let reqs = total_reqs.load(Ordering::Relaxed);
    let bytes = total_bytes.load(Ordering::Relaxed);
    let errs = errors.load(Ordering::Relaxed);
    let rps = reqs as f64 / elapsed;
    let mbps = (bytes as f64 / MIB as f64) / elapsed;

    // One machine-readable line the orchestration parses.
    println!(
        "RESULT mode={mode} conc={concurrency} secs={secs} payload_mib={payload_mib} reqs={reqs} rps={rps:.0} mbps={mbps:.1} errors={errs}"
    );
}

/// Build the full request bytes for a mode (payload baked in for upload/multipart).
fn build_request(mode: &str, payload_mib: usize) -> Vec<u8> {
    match mode {
        "download" => b"GET /download HTTP/1.1\r\nHost: x\r\n\r\n".to_vec(),
        "upload" => {
            let payload = vec![b'x'; payload_mib * MIB];
            let mut r = format!(
                "POST /upload HTTP/1.1\r\nHost: x\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
                payload.len()
            )
            .into_bytes();
            r.extend_from_slice(&payload);
            r
        }
        "multipart" => {
            let boundary = "----kwbench";
            let file = vec![b'x'; payload_mib * MIB];
            let mut body = Vec::new();
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                b"Content-Disposition: form-data; name=\"file\"; filename=\"blob.bin\"\r\n",
            );
            body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
            body.extend_from_slice(&file);
            body.extend_from_slice(b"\r\n");
            body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
            let mut r = format!(
                "POST /multipart HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/form-data; boundary={boundary}\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .into_bytes();
            r.extend_from_slice(&body);
            r
        }
        other => {
            eprintln!("unknown mode: {other}");
            std::process::exit(2);
        }
    }
}

/// Read one HTTP/1.1 response on a keep-alive connection, returning the body length.
/// Assumes a `Content-Length` (all three servers send one for these routes).
fn read_response<R: Read>(reader: &mut BufReader<R>) -> std::io::Result<usize> {
    use std::io::BufRead;
    let mut content_length: Option<usize> = None;
    let mut status_ok = false;
    let mut line = String::new();
    // Status line + headers.
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof in head"));
        }
        if line == "\r\n" || line == "\n" {
            break; // end of headers
        }
        if line.starts_with("HTTP/") {
            status_ok = line.contains(" 200") || line.contains(" 201") || line.contains(" 204");
        } else if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().ok();
            }
        }
    }
    if !status_ok {
        return Err(std::io::Error::new(std::io::ErrorKind::Other, format!("bad status: {line:?}")));
    }
    // Consume exactly the body so the connection is ready for the next request.
    let len = content_length.unwrap_or(0);
    let mut remaining = len;
    let mut buf = [0u8; 64 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len());
        let got = reader.read(&mut buf[..want])?;
        if got == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof in body"));
        }
        remaining -= got;
    }
    Ok(len)
}
