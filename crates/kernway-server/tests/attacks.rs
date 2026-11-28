//! Attacks against a running Kernway server, over real sockets: request smuggling,
//! oversized and endless input, slow clients, path traversal, cross-origin reads,
//! forged and expired tokens, request floods. Each test starts the server on a free
//! loopback port and speaks raw bytes to it — what an attacker controls.

use kernway_core::request::Request;
use kernway_core::response::Response;
use kernway_server::{AppBuilder, Cors, CsrfProtection, HttpSecurity, KeepAliveConfig, KernwayApp, RateLimit, RequestScope, SecurityHeaders, StatusCode};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Start a server built by `f` on a free port; returns the port and the hit counter
/// the `/hit` route bumps (so a test can tell whether a smuggled request ran).
fn start(f: impl FnOnce(AppBuilder) -> AppBuilder + Send + 'static) -> (u16, Arc<AtomicUsize>) {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    std::thread::spawn(move || {
        let b = KernwayApp::builder()
            .bind(&format!("127.0.0.1:{port}"))
            .workers(1)
            .get("/hit", move |_req: Request, _s: &RequestScope| {
                h.fetch_add(1, Ordering::SeqCst);
                async { Response::new(StatusCode::OK).body(b"hit".to_vec()) }
            })
            .post("/echo", |req: Request, _s: &RequestScope| {
                let body = req.body.clone();
                let spool = req.body_spool.as_ref().map(|s| s.len);
                async move {
                    match spool {
                        Some(n) => Response::new(StatusCode::OK).body(format!("spooled {n}").into_bytes()),
                        None => Response::new(StatusCode::OK).body(body),
                    }
                }
            })
            .get("/", |_req: Request, _s: &RequestScope| async { Response::new(StatusCode::OK).body(b"home".to_vec()) });
        f(b).build().run_until_shutdown().unwrap();
    });
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return (port, hits);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the server did not start");
}

/// Write `raw`, then read until the server closes (or 5 s pass).
fn exchange(port: u16, raw: &[u8]) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(raw).unwrap();
    let mut out = Vec::new();
    let mut b = [0u8; 8192];
    loop {
        match s.read(&mut b) {
            Ok(0) | Err(_) => break,
            Ok(n) => out.extend_from_slice(&b[..n]),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn status(resp: &str) -> &str {
    resp.lines().next().unwrap_or("")
}

const SMUGGLED: &str = "GET /hit HTTP/1.1\r\nHost: x\r\n\r\n";

#[test]
fn request_smuggling_vectors_are_refused_and_nothing_behind_them_runs() {
    let (port, hits) = start(|b| b);
    let cases: &[(&str, &str, &str)] = &[
        ("CL.CL: two lengths", "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\nContent-Length: 40\r\n\r\nhello", "400"),
        ("CL list that disagrees", "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 5, 40\r\n\r\nhello", "400"),
        ("CL with a sign", "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: +5\r\n\r\nhello", "400"),
        ("CL in hex", "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 0x5\r\n\r\nhello", "400"),
        ("CL negative", "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: -1\r\n\r\n", "400"),
        ("CL not a number", "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: five\r\n\r\nhello", "400"),
        ("TE.CL: both", "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n", "400"),
        ("TE with space before the colon", "POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding : chunked\r\n\r\n0\r\n\r\n", "400"),
        ("CL with space before the colon", "POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length : 5\r\n\r\nhello", "400"),
        ("TE folded onto a second line", "POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: x\r\n chunked\r\n\r\n0\r\n\r\n", "400"),
        ("obsolete line folding", "GET / HTTP/1.1\r\nHost: x\r\nX-A: 1\r\n\tfolded\r\n\r\n", "400"),
        ("a coding other than chunked", "POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: gzip\r\n\r\nabc", "501"),
        ("chunked not alone", "POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: gzip, chunked\r\n\r\n0\r\n\r\n", "501"),
        ("a lookalike coding", "POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: xchunked\r\n\r\n0\r\n\r\n", "501"),
        ("TE in HTTP/1.0", "POST /echo HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n", "400"),
        ("a bad chunk size", "POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nhello\r\n0\r\n\r\n", "400"),
        ("a chunk without its CRLF", "POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhelloXX0\r\n\r\n", "400"),
        ("two Host headers", "GET / HTTP/1.1\r\nHost: a.example\r\nHost: b.example\r\n\r\n", "400"),
        ("a NUL in a header", "GET / HTTP/1.1\r\nHost: x\r\nX-A: a\0b\r\n\r\n", "400"),
        ("a bare CR in a header", "GET / HTTP/1.1\r\nHost: x\r\nX-A: a\rb\r\n\r\n", "400"),
        ("a header name with a space", "GET / HTTP/1.1\r\nHost: x\r\nX A: 1\r\n\r\n", "400"),
        ("a line without a colon", "GET / HTTP/1.1\r\nHost: x\r\nnonsense\r\n\r\n", "400"),
        ("HTTP/2.0 over HTTP/1", "GET / HTTP/2.0\r\nHost: x\r\n\r\n", "505"),
        ("a garbage version", "GET / HTTP/1.1x\r\nHost: x\r\n\r\n", "400"),
        ("a space inside the target", "GET /a b HTTP/1.1\r\nHost: x\r\n\r\n", "400"),
        ("a method that is not a token", "G(ET / HTTP/1.1\r\nHost: x\r\n\r\n", "400"),
    ];
    for (name, raw, want) in cases {
        let resp = exchange(port, format!("{raw}{SMUGGLED}").as_bytes());
        let line = status(&resp);
        assert!(line.starts_with(&format!("HTTP/1.1 {want} ")), "{name}: got {line:?}");
        assert_eq!(resp.matches("HTTP/1.1 ").count(), 1, "{name}: one answer, then the connection closes");
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0, "no smuggled request ever reached a handler");
    // The control: the same smuggled request sent honestly does run.
    assert!(status(&exchange(port, b"GET /hit HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")).starts_with("HTTP/1.1 200"));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn chunked_request_bodies_are_decoded_and_the_next_request_stays_in_step() {
    let (port, hits) = start(|b| b);
    let raw = "POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nX-Trailer: t\r\n\r\nGET /hit HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    let resp = exchange(port, raw.as_bytes());
    assert_eq!(resp.matches("HTTP/1.1 200").count(), 2, "{resp}");
    assert!(resp.contains("\r\n\r\nhello world"), "{resp}");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "the request after the chunked body ran once");
    // Equal Content-Length repeats are one length (RFC 9110 §8.6).
    let resp = exchange(port, b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 5, 5\r\nConnection: close\r\n\r\nhello");
    assert!(resp.ends_with("hello"), "{resp}");
}

#[test]
fn a_large_chunked_upload_is_spooled_and_one_past_the_ceiling_refused() {
    let (port, _) = start(|b| b.max_inmemory_body(1024).max_upload_size(200_000));
    let mut raw = b"POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    for _ in 0..100 {
        raw.extend(b"3e8\r\n");
        raw.extend(vec![b'z'; 1000]);
        raw.extend(b"\r\n");
    }
    raw.extend(b"0\r\n\r\n");
    let resp = exchange(port, &raw);
    assert!(resp.ends_with("spooled 100000"), "{}", &resp[..resp.len().min(300)]);

    let mut raw = b"POST /echo HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
    for _ in 0..300 {
        raw.extend(b"3e8\r\n");
        raw.extend(vec![b'z'; 1000]);
        raw.extend(b"\r\n");
    }
    let resp = exchange(port, &raw);
    assert!(status(&resp).starts_with("HTTP/1.1 413 Content Too Large"), "{}", status(&resp));
}

#[test]
fn oversized_heads_and_bodies_are_refused_before_they_are_read() {
    let (port, _) = start(|b| b.max_upload_size(1000));
    let mut raw = b"GET / HTTP/1.1\r\nHost: x\r\n".to_vec();
    for i in 0..400 {
        raw.extend(format!("X-Filler-{i}: aaaaaaaaaaaaaaaaaaaaaa\r\n").as_bytes());
    }
    raw.extend(b"\r\n");
    assert!(status(&exchange(port, &raw)).starts_with("HTTP/1.1 431 "), "a head over 8 KiB");
    let resp = exchange(port, b"POST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 999999999\r\n\r\n");
    assert!(status(&resp).starts_with("HTTP/1.1 413 Content Too Large"), "{}", status(&resp));
}

#[test]
fn a_slow_client_is_cut_off_at_the_idle_timeout() {
    let ka = KeepAliveConfig { idle_timeout: Duration::from_millis(800), ..KeepAliveConfig::default() };
    let (port, hits) = start(move |b| b.keep_alive(ka));
    // Slowloris: a head that never finishes, a byte at a time.
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let started = Instant::now();
    s.write_all(b"GET /hit HTTP/1.1\r\nHost: x\r\n").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    let _ = s.write_all(b"X-Slow: 1\r\n");
    let mut b = [0u8; 64];
    let n = s.read(&mut b).unwrap_or(0);
    assert_eq!(n, 0, "closed without an answer");
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[test]
fn pipelined_requests_are_answered_in_order() {
    let (port, hits) = start(|b| b);
    let raw = "GET /hit HTTP/1.1\r\nHost: x\r\n\r\nPOST /echo HTTP/1.1\r\nHost: x\r\nContent-Length: 6\r\n\r\nsecondGET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    let resp = exchange(port, raw.as_bytes());
    let bodies: Vec<&str> = resp.split("HTTP/1.1 200 OK").skip(1).map(|r| r.rsplit("\r\n\r\n").next().unwrap()).collect();
    assert_eq!(bodies, ["hit", "second", "home"], "{resp}");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn static_files_never_leave_their_root() {
    let dir = std::env::temp_dir().join(format!("kw-static-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("public/sub")).unwrap();
    std::fs::write(dir.join("public/ok.txt"), b"public file").unwrap();
    std::fs::write(dir.join("public/sub/inner.txt"), b"inner file").unwrap();
    std::fs::write(dir.join("secret.txt"), b"TOP SECRET").unwrap();
    let root = dir.join("public");
    let (port, _) = start(move |b| b.static_files(root));
    let get = |path: &str| exchange(port, format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes());
    assert!(get("/ok.txt").ends_with("public file"));
    assert!(get("/sub/inner.txt").ends_with("inner file"));
    for path in [
        "/../secret.txt",
        "/sub/../../secret.txt",
        "/%2e%2e/secret.txt",
        "/%2E%2E/secret.txt",
        "/..%2fsecret.txt",
        "/%2e%2e%2fsecret.txt",
        "/sub/%2e%2e/%2e%2e/secret.txt",
        "/..%5csecret.txt",
        "/..\\secret.txt",
        "/sub/..%252f..%252fsecret.txt",
        "//secret.txt",
        "/ok.txt%00.png",
    ] {
        let resp = get(path);
        assert!(!resp.contains("TOP SECRET"), "{path} leaked the file outside the root: {}", status(&resp));
        assert!(!status(&resp).starts_with("HTTP/1.1 200"), "{path}: {}", status(&resp));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn security_headers_are_on_every_response() {
    let (port, _) = start(|b| b.layer(SecurityHeaders::strict()));
    for raw in ["GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n", "GET /missing HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"] {
        let resp = exchange(port, raw.as_bytes()).to_ascii_lowercase();
        for h in ["x-content-type-options: nosniff", "x-frame-options:", "referrer-policy:", "content-security-policy:", "strict-transport-security:"] {
            assert!(resp.contains(h), "{h} missing on {}", status(&resp));
        }
    }
}

#[test]
fn cross_origin_reads_are_granted_only_to_listed_origins() {
    let (port, _) = start(|b| b.layer(Cors::new().allow_origin("https://app.example").allow_credentials(true)));
    let req = |origin: &str| exchange(port, format!("GET / HTTP/1.1\r\nHost: x\r\nOrigin: {origin}\r\nConnection: close\r\n\r\n").as_bytes()).to_ascii_lowercase();
    let ok = req("https://app.example");
    assert!(ok.contains("access-control-allow-origin: https://app.example\r\n"), "{ok}");
    assert!(ok.contains("access-control-allow-credentials: true"));
    assert!(ok.contains("vary: origin"), "a cache must not hand one origin's grant to another");
    for evil in ["https://evil.example", "https://app.example.evil.example", "https://evilapp.example", "null", "http://app.example"] {
        let r = req(evil);
        assert!(!r.contains("access-control-allow-origin"), "{evil} was granted: {r}");
    }
    let pre = exchange(port, b"OPTIONS / HTTP/1.1\r\nHost: x\r\nOrigin: https://evil.example\r\nAccess-Control-Request-Method: DELETE\r\nConnection: close\r\n\r\n").to_ascii_lowercase();
    assert!(!pre.contains("access-control-allow"), "a refused preflight grants nothing: {pre}");
}

#[test]
fn a_flood_from_one_client_is_slowed_down() {
    let (port, _) = start(|b| b.layer(RateLimit::new(3, Duration::from_secs(60))));
    let codes: Vec<String> = (0..5)
        .map(|_| status(&exchange(port, b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")).to_string())
        .collect();
    assert!(codes[..3].iter().all(|c| c.starts_with("HTTP/1.1 200")), "{codes:?}");
    assert!(codes[3..].iter().all(|c| c.starts_with("HTTP/1.1 429")), "{codes:?}");
}

#[test]
fn a_header_built_from_user_input_cannot_split_the_response() {
    // A handler that redirects to a URL from the query, as login pages do.
    let (port, _) = start(|b| {
        b.get("/go", |req: Request, _s: &RequestScope| {
            let to = req.query.get("to").map(str::to_owned).unwrap_or_default();
            async move {
                let mut r = Response::new(StatusCode(302));
                r.headers.insert("location", &to);
                r
            }
        })
    });
    let resp = exchange(port, b"GET /go?to=/home%0d%0aSet-Cookie:%20session=attacker%0d%0a%0d%0a<script>x</script> HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    let head = resp.split("\r\n\r\n").next().unwrap();
    assert!(!head.lines().any(|l| l.to_ascii_lowercase().starts_with("set-cookie")), "an injected header: {head}");
    assert!(head.contains("location: /home  Set-Cookie: session=attacker"), "kept, but on one line: {head}");
    assert_eq!(resp.matches("HTTP/1.1 ").count(), 1);
}

#[test]
fn a_forged_x_forwarded_for_does_not_escape_the_rate_limit() {
    // No proxy is trusted: the header is the attacker's, so every request counts
    // against the socket's own address whatever it claims.
    let (port, _) = start(|b| b.layer(RateLimit::new(3, Duration::from_secs(60))));
    let codes: Vec<String> = (0..6)
        .map(|i| status(&exchange(port, format!("GET / HTTP/1.1\r\nHost: x\r\nX-Forwarded-For: 203.0.113.{i}\r\nConnection: close\r\n\r\n").as_bytes())).to_string())
        .collect();
    assert!(codes[3..].iter().all(|c| c.starts_with("HTTP/1.1 429")), "{codes:?}");
}

#[test]
fn forged_cross_site_writes_are_refused() {
    let (port, hits) = start(|b| b.layer(CsrfProtection::new().exempt("/hooks/")).post("/hooks/in", |_r: Request, _s: &RequestScope| async { Response::new(StatusCode::OK).body(b"hook".to_vec()) }));
    // A first visit hands out the token cookie.
    let first = exchange(port, b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    let cookie = first.lines().find_map(|l| l.strip_prefix("set-cookie: ").or_else(|| l.strip_prefix("Set-Cookie: "))).expect("a token cookie").to_string();
    assert!(cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"), "{cookie}");
    let token = cookie.split(';').next().unwrap().split_once('=').unwrap().1.to_string();
    let post = |headers: &str, body: &str| {
        status(&exchange(port, format!("POST /echo HTTP/1.1\r\nHost: x\r\n{headers}Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())).to_string()
    };
    // What a forging site can send: the victim's cookie rides along, but not the token.
    assert!(post(&format!("Cookie: kw_csrf={token}\r\n"), "amount=100").starts_with("HTTP/1.1 403"));
    assert!(post("", "amount=100").starts_with("HTTP/1.1 403"));
    assert!(post(&format!("Cookie: kw_csrf={token}\r\n"), &format!("amount=100&_csrf={}", "0".repeat(64))).starts_with("HTTP/1.1 403"), "a wrong token");
    // The real page: the token comes back in the form or a header.
    assert!(post(&format!("Cookie: kw_csrf={token}\r\n"), &format!("amount=100&_csrf={token}")).starts_with("HTTP/1.1 200"));
    assert!(post(&format!("Cookie: kw_csrf={token}\r\nX-CSRF-Token: {token}\r\n"), "{}").starts_with("HTTP/1.1 200"));
    // An API call with its own credentials, and an exempt webhook, are not forms.
    assert!(post("Authorization: Bearer x\r\n", "{}").starts_with("HTTP/1.1 200"));
    assert!(status(&exchange(port, b"POST /hooks/in HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")).starts_with("HTTP/1.1 200"));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
}

#[cfg(feature = "jwt")]
#[test]
fn only_valid_tokens_with_the_right_role_get_in() {
    use kernway_server::{Claims, Jwt, JwtAuth};
    let secret = b"a long random server secret 0123456789".to_vec();
    let s2 = secret.clone();
    let (port, _) = start(move |b| {
        b.layer(JwtAuth::bearer(s2))
            .layer(HttpSecurity::new().has_role("/admin/**", "ADMIN").authenticated("/me").build())
            .get("/admin/panel", |_r: Request, _s: &RequestScope| async { Response::new(StatusCode::OK).body(b"admin".to_vec()) })
            .get("/me", |_r: Request, _s: &RequestScope| async { Response::new(StatusCode::OK).body(b"me".to_vec()) })
    });
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let jwt = Jwt::new(secret);
    let admin = jwt.encode(&Claims::new().subject("a").roles(["ADMIN"]).expires_at(now + 600)).unwrap();
    let user = jwt.encode(&Claims::new().subject("u").roles(["USER"]).expires_at(now + 600)).unwrap();
    let expired = jwt.encode(&Claims::new().subject("a").roles(["ADMIN"]).expires_at(now - 3600)).unwrap();
    let other_key = Jwt::new(b"not the server's secret".to_vec()).encode(&Claims::new().subject("a").roles(["ADMIN"])).unwrap();
    let b64 = |s: &str| kernway_wire_b64url(s.as_bytes());
    // alg:none with an ADMIN claim, and a "RS256" header over the HS signature (alg confusion).
    let none = format!("{}.{}.", b64(r#"{"alg":"none","typ":"JWT"}"#), b64(r#"{"sub":"a","roles":["ADMIN"]}"#));
    let parts: Vec<&str> = admin.split('.').collect();
    let confused = format!("{}.{}.{}", b64(r#"{"alg":"RS256","typ":"JWT"}"#), parts[1], parts[2]);
    // The payload swapped for one claiming ADMIN, keeping the user's signature.
    let uparts: Vec<&str> = user.split('.').collect();
    let tampered = format!("{}.{}.{}", uparts[0], parts[1], uparts[2]);
    let get = |path: &str, token: Option<&str>| {
        let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        status(&exchange(port, format!("GET {path} HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n").as_bytes())).to_string()
    };
    assert!(get("/admin/panel", Some(&admin)).starts_with("HTTP/1.1 200"));
    assert!(get("/me", Some(&user)).starts_with("HTTP/1.1 200"));
    assert!(get("/admin/panel", Some(&user)).starts_with("HTTP/1.1 403"), "authenticated, wrong role");
    for (name, t) in [("none", None), ("expired", Some(expired.as_str())), ("another key", Some(other_key.as_str())), ("alg none", Some(none.as_str())), ("alg confusion", Some(confused.as_str())), ("tampered payload", Some(tampered.as_str())), ("garbage", Some("a.b.c"))] {
        assert!(get("/admin/panel", t).starts_with("HTTP/1.1 401"), "{name}: {}", get("/admin/panel", t));
        assert!(get("/me", t).starts_with("HTTP/1.1 401"), "{name} on /me");
    }
}

/// base64url without padding (the JWT encoding), for building forged tokens.
#[cfg(feature = "jwt")]
fn kernway_wire_b64url(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..=c.len() {
            out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}
