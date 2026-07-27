//! Live activity over a real socket: one visitor navigates `/hello` → `/reports`,
//! carrying their `kw_visitor` cookie, and the shared `InMemoryActivity` shows a
//! single active visitor whose page is the most recent one — the "who's on the site
//! and where" view, end to end.

#![cfg(feature = "presence")]

use std::future::Future;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hello_controller::build_app_activity;
use kernway_server::{Activity, InMemoryActivity};

/// The in-memory store resolves on the first poll — no runtime needed.
fn block<T>(fut: impl Future<Output = T>) -> T {
    let mut fut = Box::pin(fut);
    let mut cx = Context::from_waker(Waker::noop());
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("in-memory activity must resolve synchronously"),
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn connect(port: u16) -> TcpStream {
    for _ in 0..300 {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
            return s;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("server never came up");
}

/// GET `path`, optionally sending a Cookie header, and return the raw response.
fn get(port: u16, path: &str, cookie: Option<&str>) -> String {
    let mut stream = connect(port);
    let cookie_line = cookie.map(|c| format!("Cookie: {c}\r\n")).unwrap_or_default();
    let raw = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{cookie_line}\r\n");
    stream.write_all(raw.as_bytes()).unwrap();
    let mut got = String::new();
    stream.read_to_string(&mut got).unwrap();
    got
}

/// Pull the `kw_visitor=...` pair out of a Set-Cookie response header.
fn visitor_cookie(response: &str) -> String {
    let line = response
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:") && l.contains("kw_visitor="))
        .expect("a kw_visitor cookie");
    let start = line.find("kw_visitor=").unwrap();
    let rest = &line[start..];
    rest.split(';').next().unwrap().to_string() // "kw_visitor=<id>"
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

#[test]
fn one_visitor_navigating_two_pages_is_one_active_row_on_the_latest_page() {
    let activity = Arc::new(InMemoryActivity::new(Duration::from_secs(60)));
    let port = free_port();
    let app = build_app_activity(&format!("127.0.0.1:{port}"), activity.clone());
    let stop = app.shutdown_handle();
    let server = std::thread::spawn(move || app.run_until_shutdown());

    // First page — get a visitor cookie, then reuse it for the second page.
    let first = get(port, "/hello", None);
    assert!(first.starts_with("HTTP/1.1 200"));
    let cookie = visitor_cookie(&first);
    let _ = get(port, "/reports", Some(&cookie));

    // The shared store (also held by the middleware) shows one visitor, on /reports.
    let live = block(activity.active(now())).unwrap();
    assert_eq!(live.len(), 1, "same cookie ⇒ one identity, not two: {live:?}");
    assert_eq!(live[0].path, "/reports", "we see the latest page they are on");
    assert!(!live[0].authenticated, "an anonymous visitor (tracked by visitor id)");

    stop.trigger();
    server.join().unwrap().unwrap();
}
