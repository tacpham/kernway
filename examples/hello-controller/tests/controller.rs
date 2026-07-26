//! Role-based access over a real socket: the public route is open, the admin route
//! requires the ADMIN role, and everyone else gets a 403.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use hello_controller::build_app;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn connect(port: u16) -> TcpStream {
    for _ in 0..100 {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
            return s;
        }
        std::thread::yield_now();
    }
    panic!("server never came up");
}

/// Send a request with an optional `X-Role` header; return the whole response.
fn request(port: u16, method: &str, path: &str, role: Option<&str>) -> String {
    let mut stream = connect(port);
    let header = role.map(|r| format!("X-Role: {r}\r\n")).unwrap_or_default();
    let raw = format!("{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{header}\r\n");
    stream.write_all(raw.as_bytes()).unwrap();
    let mut got = String::new();
    stream.read_to_string(&mut got).unwrap();
    got
}

fn with_server<T>(f: impl FnOnce(u16) -> T) -> T {
    let port = free_port();
    let app = build_app(&format!("127.0.0.1:{port}"));
    let stop = app.shutdown_handle();
    let server = std::thread::spawn(move || app.run_until_shutdown());
    let out = f(port);
    stop.trigger();
    server.join().unwrap().unwrap();
    out
}

#[test]
fn the_public_route_is_open() {
    let resp = with_server(|port| request(port, "GET", "/users/42", None));
    assert!(resp.starts_with("HTTP/1.1 200"), "public GET → 200: {resp}");
    assert!(resp.contains(r#""id":"42""#), "returns the user: {resp}");
}

#[test]
fn the_admin_route_allows_an_admin() {
    let resp = with_server(|port| request(port, "DELETE", "/users/42", Some("ADMIN")));
    assert!(resp.starts_with("HTTP/1.1 200"), "ADMIN DELETE → 200: {resp}");
    assert!(resp.contains(r#""deleted":"42""#), "performs the delete: {resp}");
}

#[test]
fn the_admin_route_forbids_a_non_admin() {
    let resp = with_server(|port| request(port, "DELETE", "/users/42", Some("USER")));
    assert!(resp.starts_with("HTTP/1.1 403"), "USER DELETE → 403: {resp}");
    assert!(resp.contains("Forbidden"), "RFC 7807 forbidden: {resp}");
}

#[test]
fn the_admin_route_forbids_anonymous() {
    let resp = with_server(|port| request(port, "DELETE", "/users/42", None));
    assert!(resp.starts_with("HTTP/1.1 403"), "anonymous DELETE → 403: {resp}");
}
