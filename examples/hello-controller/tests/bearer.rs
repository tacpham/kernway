//! JWT bearer auth over a real socket: mint HS256 tokens, present them as
//! `Authorization: Bearer <jwt>`, and confirm the server authorizes by the token's
//! `sub`/`roles` — and rejects a missing, expired, or tampered token as anonymous.

#![cfg(feature = "jwt")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{SystemTime, UNIX_EPOCH};

use hello_controller::build_app_bearer;
use kernway_server::{Claims, Jwt};

const SECRET: &str = "test-signing-secret";

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

/// GET `path` with an optional bearer token; return the raw response.
fn get(port: u16, path: &str, token: Option<&str>) -> String {
    let mut stream = connect(port);
    let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    let raw = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{auth}\r\n");
    stream.write_all(raw.as_bytes()).unwrap();
    let mut got = String::new();
    stream.read_to_string(&mut got).unwrap();
    got
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn status(response: &str) -> &str {
    response.lines().next().unwrap_or("")
}

#[test]
fn bearer_jwt_drives_role_based_access() {
    let port = free_port();
    let app = build_app_bearer(&format!("127.0.0.1:{port}"), SECRET);
    let stop = app.shutdown_handle();
    let server = std::thread::spawn(move || app.run_until_shutdown());

    let jwt = Jwt::new(SECRET);
    let user = jwt.encode(&Claims::new().subject("bob").roles(["USER"]).expires_at(now() + 3600)).unwrap();
    let admin = jwt.encode(&Claims::new().subject("alice").roles(["ADMIN"]).expires_at(now() + 3600)).unwrap();
    let expired = jwt.encode(&Claims::new().subject("alice").roles(["ADMIN"]).expires_at(now() - 3600)).unwrap();

    // No token → anonymous → /me needs a login → 401.
    assert!(status(&get(port, "/me", None)).starts_with("HTTP/1.1 401"), "no token → 401");

    // A valid user token reaches /me but not /admin (lacks ADMIN → 403).
    assert!(status(&get(port, "/me", Some(&user))).starts_with("HTTP/1.1 200"), "user → /me 200");
    assert!(status(&get(port, "/admin/panel", Some(&user))).starts_with("HTTP/1.1 403"), "user → /admin 403");

    // An ADMIN token reaches /admin.
    assert!(status(&get(port, "/admin/panel", Some(&admin))).starts_with("HTTP/1.1 200"), "admin → /admin 200");

    // An expired token is not trusted → anonymous → 401.
    assert!(status(&get(port, "/me", Some(&expired))).starts_with("HTTP/1.1 401"), "expired → 401");

    // A tampered token (flip a signature char) → anonymous → 401.
    let mut chars: Vec<char> = admin.chars().collect();
    let last = chars.len() - 1;
    chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
    let tampered: String = chars.into_iter().collect();
    assert!(status(&get(port, "/me", Some(&tampered))).starts_with("HTTP/1.1 401"), "tampered → 401");

    stop.trigger();
    server.join().unwrap().unwrap();
}
