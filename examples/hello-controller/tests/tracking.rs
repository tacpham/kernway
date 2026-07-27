//! Visitor tracking + runtime ban/unban over a real socket: a first visit gets a
//! `kw_visitor` cookie, banning the client IP at runtime turns the next request into
//! a 403, and unbanning restores access.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use hello_controller::build_app_tracked;
use kernway_server::Bans;

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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

fn get(port: u16, path: &str) -> String {
    let mut stream = connect(port);
    let raw = format!(
        "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nUser-Agent: test/1.0\r\n\r\n"
    );
    stream.write_all(raw.as_bytes()).unwrap();
    let mut got = String::new();
    stream.read_to_string(&mut got).unwrap();
    got
}

#[test]
fn visitor_cookie_then_runtime_ban_and_unban() {
    let bans = Bans::new();
    let port = free_port();
    let app = build_app_tracked(&format!("127.0.0.1:{port}"), bans.clone());
    let stop = app.shutdown_handle();
    let server = std::thread::spawn(move || app.run_until_shutdown());

    // First visit: 200, and a kw_visitor cookie is issued.
    let first = get(port, "/hello");
    assert!(first.starts_with("HTTP/1.1 200"), "first visit ok: {first}");
    assert!(
        first
            .to_ascii_lowercase()
            .contains("set-cookie: kw_visitor="),
        "visitor cookie set: {first}"
    );

    // Ban this client's IP at runtime → the next request is 403.
    bans.ban_ip("127.0.0.1".parse().unwrap());
    let banned = get(port, "/hello");
    assert!(banned.starts_with("HTTP/1.1 403"), "banned → 403: {banned}");
    assert!(banned.contains("access denied"), "ban response: {banned}");

    // Unban → access restored.
    bans.unban_ip("127.0.0.1".parse().unwrap());
    let restored = get(port, "/hello");
    assert!(
        restored.starts_with("HTTP/1.1 200"),
        "unbanned → 200: {restored}"
    );

    stop.trigger();
    server.join().unwrap().unwrap();
}
