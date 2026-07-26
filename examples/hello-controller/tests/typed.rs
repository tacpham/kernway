//! Typed argument extraction over a real socket: a `Path<u64>` parses (or 400s), a
//! `Validated<T>` body is validated before the method runs (400 with field errors
//! otherwise), and a `SecurityContext` argument reflects the request's identity.

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

/// Send a request (optional `X-Role`, optional JSON body); return the raw response.
fn send(port: u16, method: &str, path: &str, role: Option<&str>, body: Option<&str>) -> String {
    let mut stream = connect(port);
    let role_h = role.map(|r| format!("X-Role: {r}\r\n")).unwrap_or_default();
    let body_h = body
        .map(|b| format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()))
        .unwrap_or_default();
    let raw = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{role_h}{body_h}\r\n{}",
        body.unwrap_or("")
    );
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
fn path_argument_is_parsed_and_typed() {
    with_server(|port| {
        // Path<u64> parses a numeric id.
        let ok = send(port, "GET", "/items/42", None, None);
        assert!(ok.starts_with("HTTP/1.1 200"), "numeric id → 200: {ok}");
        assert!(ok.contains(r#""item":42"#), "typed id in body: {ok}");

        // A non-numeric id fails to parse → 400, the method never runs.
        let bad = send(port, "GET", "/items/not-a-number", None, None);
        assert!(bad.starts_with("HTTP/1.1 400"), "bad id → 400: {bad}");
    });
}

#[test]
fn validated_body_argument_gates_the_method() {
    with_server(|port| {
        // A valid body reaches the method.
        let ok = send(port, "POST", "/items", None, Some(r#"{"name":"Widget"}"#));
        assert!(ok.starts_with("HTTP/1.1 200"), "valid body → 200: {ok}");
        assert!(ok.contains(r#""created":"Widget""#), "echoes the name: {ok}");

        // Too short → 400 with the field error, before the method body.
        let short = send(port, "POST", "/items", None, Some(r#"{"name":"W"}"#));
        assert!(short.starts_with("HTTP/1.1 400"), "too short → 400: {short}");
        assert!(short.contains(r#""field":"name""#), "field error present: {short}");

        // Blank → 400.
        let blank = send(port, "POST", "/items", None, Some(r#"{"name":""}"#));
        assert!(blank.starts_with("HTTP/1.1 400"), "blank → 400: {blank}");

        // Malformed JSON → 400.
        let malformed = send(port, "POST", "/items", None, Some("not json"));
        assert!(malformed.starts_with("HTTP/1.1 400"), "malformed → 400: {malformed}");
    });
}

#[test]
fn security_context_argument_reflects_the_identity() {
    with_server(|port| {
        // Authenticated as ADMIN (the static /items/whoami wins over /items/{id}).
        let admin = send(port, "GET", "/items/whoami", Some("ADMIN"), None);
        assert!(admin.starts_with("HTTP/1.1 200"), "whoami → 200: {admin}");
        assert!(admin.contains(r#""user":"demo-user""#), "principal: {admin}");
        assert!(admin.contains(r#""admin":true"#), "ADMIN role seen: {admin}");

        // Anonymous.
        let anon = send(port, "GET", "/items/whoami", None, None);
        assert!(anon.contains(r#""user":"anonymous""#), "anonymous principal: {anon}");
        assert!(anon.contains(r#""admin":false"#), "no ADMIN role: {anon}");
    });
}
