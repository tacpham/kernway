//! Validation end to end, over a real socket: a valid body is created, an invalid
//! one comes back as a 400 RFC 7807 listing every field error, before any handler
//! logic runs.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use hello_validate::build_app;

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

/// POST a JSON body and return the whole raw response.
fn post_json(port: u16, path: &str, body: &str) -> String {
    let mut stream = connect(port);
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
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
fn a_valid_body_is_created() {
    let resp = with_server(|port| {
        post_json(port, "/users", r#"{"name":"Alice","email":"alice@example.com","age":30}"#)
    });
    assert!(resp.starts_with("HTTP/1.1 201"), "valid → 201 Created: {resp}");
    assert!(resp.contains(r#""created":"Alice""#), "echoes the name: {resp}");
}

#[test]
fn an_invalid_body_is_400_listing_every_field_error() {
    let resp = with_server(|port| {
        // name blank, email malformed, age out of range — all three should report.
        post_json(port, "/users", r#"{"name":"","email":"nope","age":200}"#)
    });
    assert!(resp.starts_with("HTTP/1.1 400"), "invalid → 400: {resp}");
    assert!(resp.contains("Validation Failed"), "RFC 7807 title: {resp}");
    assert!(resp.contains(r#""field":"name""#), "name error present: {resp}");
    assert!(resp.contains(r#""field":"email""#), "email error present: {resp}");
    assert!(resp.contains(r#""field":"age""#), "age error present: {resp}");
}

#[test]
fn a_malformed_body_is_a_plain_400() {
    let resp = with_server(|port| post_json(port, "/users", "not json"));
    assert!(resp.starts_with("HTTP/1.1 400"), "malformed → 400: {resp}");
}
