//! htmx over a real socket — the "test khi chạy thật" gate for M3.
//!
//! The `kernway-htmx` unit tests prove the header logic in isolation. This proves
//! the whole path: an `Htmx`/`HtmxResponse` handler wired into a running
//! `KernwayApp`, driven by a real TCP client, so the `HX-*` headers and the
//! fragment/page choice survive parsing, dispatch, and encoding over the wire.
//!
//! Only built with `--features htmx`, since that is where the API lives.
#![cfg(feature = "htmx")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use kernway::prelude::*;

/// Grab a free port by binding `:0`, then release it for the app to take. A
/// tiny TOCTOU window, but the standard way to get an ephemeral port for a test
/// server that has no `local_addr()` accessor of its own.
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// Send one request and read the whole response. `Connection: close` makes the
/// server hang up at the end, so `read_to_string` returns at EOF instead of
/// waiting out the keep-alive idle timeout.
fn request(port: u16, extra_headers: &str) -> String {
    // Retry the connect: the server thread may not have bound yet.
    let mut stream = None;
    for _ in 0..300 {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
            stream = Some(s);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut stream = stream.expect("server never came up");
    let req = format!("GET /htmx/greet HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{extra_headers}\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut got = String::new();
    stream.read_to_string(&mut got).unwrap();
    got
}

#[test]
fn htmx_endpoint_serves_fragment_vs_page_over_a_socket() {
    let port = free_port();

    let app = KernwayApp::builder()
        .bind(&format!("127.0.0.1:{port}"))
        .get("/htmx/greet", |req: Request, _ctx: &RequestScope| async move {
            Htmx::from(&req)
                .respond(
                    || "<div id=\"greeting\">FRAGMENT</div>".to_string(),
                    || "<!doctype html><div id=\"greeting\">PAGE</div>".to_string(),
                )
                .trigger("greeted")
                .into_response()
        })
        .build();

    let stop = app.shutdown_handle();
    let server = std::thread::spawn(move || app.run_until_shutdown());

    // htmx request → the fragment, with the HX-* headers intact.
    let htmx = request(port, "HX-Request: true\r\n");
    assert!(htmx.starts_with("HTTP/1.1 200 OK\r\n"), "status line: {htmx:?}");
    assert!(htmx.contains("hx-trigger: greeted\r\n"), "missing HX-Trigger: {htmx:?}");
    assert!(htmx.contains("vary: HX-Request\r\n"), "missing Vary: {htmx:?}");
    assert!(htmx.ends_with("<div id=\"greeting\">FRAGMENT</div>"), "body: {htmx:?}");

    // Plain browser request → the full page, same URL, still Vary-marked.
    let page = request(port, "");
    assert!(page.contains("vary: HX-Request\r\n"), "missing Vary: {page:?}");
    assert!(page.ends_with("<!doctype html><div id=\"greeting\">PAGE</div>"), "body: {page:?}");

    stop.trigger();
    server.join().unwrap().unwrap();
}
