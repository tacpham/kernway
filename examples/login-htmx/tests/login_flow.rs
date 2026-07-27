//! The walking-skeleton gate, over a real socket: the whole login → protected →
//! logout loop, with CSRF, sessions, and htmx redirects.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use login_htmx::build_app;

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

/// Send a raw HTTP/1.1 request (Connection: close) and return the whole response.
fn send(port: u16, raw: &str) -> String {
    let mut stream = connect(port);
    stream.write_all(raw.as_bytes()).unwrap();
    let mut got = String::new();
    stream.read_to_string(&mut got).unwrap();
    got
}

fn get(port: u16, path: &str, cookie: &str) -> String {
    let c = if cookie.is_empty() {
        String::new()
    } else {
        format!("Cookie: {cookie}\r\n")
    };
    send(
        port,
        &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{c}\r\n"),
    )
}

fn post(port: u16, path: &str, cookie: &str, body: &str) -> String {
    let c = if cookie.is_empty() {
        String::new()
    } else {
        format!("Cookie: {cookie}\r\n")
    };
    send(
        port,
        &format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{c}\
             Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
}

/// The text between `start` and the next `end` (case-sensitive).
fn extract(haystack: &str, start: &str, end: &str) -> String {
    let s = haystack
        .find(start)
        .map(|i| i + start.len())
        .unwrap_or_else(|| panic!("no `{start}` in response"));
    let e = haystack[s..].find(end).map(|i| s + i).expect("end marker");
    haystack[s..e].to_string()
}

/// Run `f` against a live server, then shut it down. Uses type inference for the
/// shutdown handle so the test does not name an unexported type.
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
fn get_login_serves_the_form_with_csrf_and_security_headers() {
    let resp = with_server(|port| get(port, "/login", ""));

    let lower = resp.to_ascii_lowercase();
    assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "status: {resp:?}");
    assert!(resp.contains("<form method=\"post\""), "form present");
    assert!(resp.contains("name=\"_csrf\""), "auto-CSRF field present");
    assert!(lower.contains("set-cookie: kw_csrf="), "CSRF cookie set");
    assert!(lower.contains("httponly"), "cookie is HttpOnly");
    assert!(lower.contains("x-frame-options: deny"), "security headers");
}

#[test]
fn the_full_login_protected_logout_flow() {
    with_server(|port| {
        // 1. GET /login — the CSRF token is in the form field and the cookie.
        let login = get(port, "/login", "");
        let csrf = extract(&login, "name=\"_csrf\" value=\"", "\"");
        assert_eq!(csrf.len(), 64, "real CSRF token");

        // 2. POST /login with the right credentials and a matching CSRF (cookie + field).
        let body = format!("username=alice&password=secret&_csrf={csrf}");
        let login_post = post(port, "/login", &format!("kw_csrf={csrf}"), &body);
        let lower = login_post.to_ascii_lowercase();
        assert!(
            lower.contains("hx-redirect: /protected"),
            "login → redirect: {login_post}"
        );
        assert!(
            lower.contains("set-cookie: kw_session="),
            "session cookie issued: {login_post}"
        );
        // Session token is base64url (mixed case) — extract from the original, not lowercased.
        let session = extract(&login_post, "set-cookie: kw_session=", ";");

        // 3. GET /protected with the session cookie — the admin sees their panel.
        let protected = get(port, "/protected", &format!("kw_session={session}"));
        assert!(
            protected.contains("Welcome, <span>alice</span>"),
            "shows the user: {protected}"
        );
        assert!(
            protected.contains("Admin panel"),
            "ADMIN sees the panel: {protected}"
        );

        // 4. GET /protected with no session — bounced to the login page.
        let anon = get(port, "/protected", "");
        assert!(
            anon.to_ascii_lowercase().contains("hx-redirect: /login"),
            "anonymous is bounced: {anon}"
        );

        // 5. POST /logout — the session is revoked and the cookie cleared…
        let logout = post(port, "/logout", &format!("kw_session={session}"), "");
        let ll = logout.to_ascii_lowercase();
        assert!(
            ll.contains("hx-redirect: /login"),
            "logout redirects: {logout}"
        );
        assert!(
            ll.contains("set-cookie: kw_session=; ") && ll.contains("max-age=0"),
            "cookie cleared: {logout}"
        );

        // 6. …so the same session token no longer works — the registry row is gone.
        let after = get(port, "/protected", &format!("kw_session={session}"));
        assert!(
            after.to_ascii_lowercase().contains("hx-redirect: /login"),
            "revoked session bounced: {after}"
        );
    });
}

#[test]
fn a_heartbeat_makes_the_user_show_up_online() {
    with_server(|port| {
        // The online list is gated: an anonymous caller does not see it.
        let anon = get(port, "/who", "");
        assert!(anon.contains("Log in to see"), "anonymous is gated: {anon}");

        // Log in for a session cookie.
        let login = get(port, "/login", "");
        let csrf = extract(&login, "name=\"_csrf\" value=\"", "\"");
        let body = format!("username=alice&password=secret&_csrf={csrf}");
        let login_post = post(port, "/login", &format!("kw_csrf={csrf}"), &body);
        let session = extract(&login_post, "set-cookie: kw_session=", ";");
        let cookie = format!("kw_session={session}");

        // Logged in but not beaten yet → the online fragment shows zero.
        let before = get(port, "/who", &cookie);
        assert!(
            before.contains("<strong>0</strong>"),
            "nobody online yet: {before}"
        );

        // The tab beats.
        let beat = post(port, "/heartbeat", &cookie, "");
        assert!(
            beat.starts_with("HTTP/1.1 204"),
            "heartbeat accepted: {beat}"
        );

        // Now the online fragment lists alice, count one — rendered by th:each.
        let who = get(port, "/who", &cookie);
        assert!(who.contains("<strong>1</strong>"), "one user online: {who}");
        assert!(
            who.contains("<li>alice</li>"),
            "alice is listed online: {who}"
        );
    });
}

#[test]
fn a_post_without_a_csrf_token_is_forbidden() {
    // No kw_csrf cookie, no _csrf field → CSRF check fails.
    let resp = with_server(|port| post(port, "/login", "", "username=alice&password=secret"));
    assert!(
        resp.starts_with("HTTP/1.1 403"),
        "missing CSRF → 403: {resp}"
    );
}

#[test]
fn wrong_credentials_do_not_log_in() {
    let resp = with_server(|port| {
        let login = get(port, "/login", "");
        let csrf = extract(&login, "name=\"_csrf\" value=\"", "\"");
        let body = format!("username=alice&password=WRONG&_csrf={csrf}");
        post(port, "/login", &format!("kw_csrf={csrf}"), &body)
    });
    assert!(
        resp.contains("Wrong username or password"),
        "rejected: {resp}"
    );
    assert!(
        !resp
            .to_ascii_lowercase()
            .contains("set-cookie: kw_session="),
        "no session issued: {resp}"
    );
}
