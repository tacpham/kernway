//! Precompressed static serving over a real socket — the "test khi chạy thật"
//! gate for the M2b precompression slice.
//!
//! The negotiation logic is unit-tested in `kernway-static` and `kernway-server`.
//! This proves the whole path end to end: a `KernwayApp` with `.precompressed()`,
//! a `.gz` sitting on disk next to the original, driven by a real TCP client —
//! the `Content-Encoding`, `Vary`, and the variant's *bytes* have to survive
//! dispatch and encoding over the wire.
//!
//! The `.gz` here holds sentinel bytes, not real gzip: the server serves the
//! variant verbatim and only sets the header, so valid compression is the build
//! tool's job, not something this test needs to prove.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

use kernway::prelude::*;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// A unique temp dir for this test's static root.
fn temp_root() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kernway-precomp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn request(port: u16, extra_headers: &str) -> String {
    let mut stream = None;
    for _ in 0..300 {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
            stream = Some(s);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut stream = stream.expect("server never came up");
    let req = format!("GET /style.css HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{extra_headers}\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut got = String::new();
    stream.read_to_string(&mut got).unwrap();
    got
}

#[test]
fn precompressed_variant_is_negotiated_over_a_socket() {
    let root = temp_root();
    std::fs::write(root.join("style.css"), b"body { color: rebeccapurple; }").unwrap();
    std::fs::write(root.join("style.css.gz"), b"SENTINEL-GZIP-BYTES").unwrap();

    let port = free_port();
    let app = KernwayApp::builder()
        .bind(&format!("127.0.0.1:{port}"))
        .static_files(&root)
        .precompressed()
        .build();

    let stop = app.shutdown_handle();
    let server = std::thread::spawn(move || app.run_until_shutdown());

    // gzip accepted → the .gz bytes, Content-Encoding: gzip, original type, Vary.
    let gz = request(port, "Accept-Encoding: gzip\r\n");
    assert!(gz.starts_with("HTTP/1.1 200 OK\r\n"), "status: {gz:?}");
    assert!(gz.contains("content-encoding: gzip\r\n"), "missing CE: {gz:?}");
    assert!(gz.contains("vary: Accept-Encoding\r\n"), "missing Vary: {gz:?}");
    assert!(gz.contains("content-type: text/css; charset=utf-8\r\n"), "type: {gz:?}");
    assert!(gz.ends_with("SENTINEL-GZIP-BYTES"), "should serve the .gz bytes: {gz:?}");

    // No Accept-Encoding → the identity file, but still Vary-marked so a shared
    // cache never later feeds the .gz to a client that cannot decode it.
    let identity = request(port, "");
    assert!(identity.contains("vary: Accept-Encoding\r\n"), "missing Vary: {identity:?}");
    assert!(!identity.contains("content-encoding:"), "identity must not claim an encoding: {identity:?}");
    assert!(identity.ends_with("body { color: rebeccapurple; }"), "identity body: {identity:?}");

    stop.trigger();
    server.join().unwrap().unwrap();
    std::fs::remove_dir_all(&root).ok();
}
