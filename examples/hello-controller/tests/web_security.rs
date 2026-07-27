//! Central, path-based web security over a real socket (Spring's HttpSecurity):
//! a public path is open, an unlisted path needs a login (401), and an /admin path
//! needs the ADMIN role (403 otherwise).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use hello_controller::build_app_secured;

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

fn get(port: u16, path: &str, role: Option<&str>) -> String {
    let mut stream = connect(port);
    let header = role.map(|r| format!("X-Role: {r}\r\n")).unwrap_or_default();
    let raw = format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{header}\r\n");
    stream.write_all(raw.as_bytes()).unwrap();
    let mut got = String::new();
    stream.read_to_string(&mut got).unwrap();
    got
}

fn with_server<T>(f: impl FnOnce(u16) -> T) -> T {
    let port = free_port();
    let app = build_app_secured(&format!("127.0.0.1:{port}"));
    let stop = app.shutdown_handle();
    let server = std::thread::spawn(move || app.run_until_shutdown());
    let out = f(port);
    stop.trigger();
    server.join().unwrap().unwrap();
    out
}

#[test]
fn path_rules_are_enforced_centrally() {
    with_server(|port| {
        // permit_all("/public/**") — open to anyone.
        let public = get(port, "/public/info", None);
        assert!(public.starts_with("HTTP/1.1 200"), "public open: {public}");

        // any_request(Authenticated) — an unlisted path needs a login → 401.
        let anon = get(port, "/secret/data", None);
        assert!(anon.starts_with("HTTP/1.1 401"), "anonymous → 401: {anon}");
        let logged_in = get(port, "/secret/data", Some("USER"));
        assert!(logged_in.starts_with("HTTP/1.1 200"), "authenticated → 200: {logged_in}");

        // has_role("/admin/**", "ADMIN") — role gate.
        let user = get(port, "/admin/panel", Some("USER"));
        assert!(user.starts_with("HTTP/1.1 403"), "USER on /admin → 403: {user}");
        let admin = get(port, "/admin/panel", Some("ADMIN"));
        assert!(admin.starts_with("HTTP/1.1 200"), "ADMIN on /admin → 200: {admin}");
    });
}
