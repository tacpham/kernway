//! The client against servers that misbehave on purpose — the cases no
//! well-behaved server produces on demand: interim 1xx responses, a head that
//! never ends, a compression bomb, conflicting lengths, a connection that dies
//! after the request was sent, an address that refuses. Each server is a few
//! lines of loopback TCP that records exactly what reached it.

use kernway_http_client::{HttpClient, HttpError, Method, Request, Url};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the server does with one request.
enum Reply {
    /// Write these bytes and keep the connection.
    Send(Vec<u8>),
    /// Write these bytes, then close.
    SendClose(Vec<u8>),
    /// Close without answering.
    Close,
}

/// Every request a server saw: (connection number, raw request).
type Log = Arc<Mutex<Vec<(usize, String)>>>;

/// A loopback server on `addr` calling `reply(connection, request number on it,
/// raw request)` for every request; returns its port and the log.
fn serve(addr: &str, reply: impl Fn(usize, usize, &str) -> Reply + Send + Sync + 'static) -> (u16, Log) {
    let listener = TcpListener::bind(addr).unwrap();
    let port = listener.local_addr().unwrap().port();
    let log: Log = Arc::default();
    let (log2, reply) = (log.clone(), Arc::new(reply));
    std::thread::spawn(move || {
        for (conn_no, sock) in listener.incoming().enumerate() {
            let Ok(sock) = sock else { return };
            let (log, reply) = (log2.clone(), reply.clone());
            std::thread::spawn(move || handle(sock, conn_no, &log, &*reply));
        }
    });
    (port, log)
}

fn handle(mut sock: TcpStream, conn_no: usize, log: &Log, reply: &dyn Fn(usize, usize, &str) -> Reply) {
    sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut buf = Vec::new();
    for req_no in 0.. {
        // One request: the head, then a Content-Length body.
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let mut chunk = [0u8; 4096];
            match sock.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let len = head
            .lines()
            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
            .unwrap_or(0);
        while buf.len() < head_end + len {
            let mut chunk = [0u8; 4096];
            match sock.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let request = String::from_utf8_lossy(&buf[..head_end + len]).into_owned();
        buf.drain(..head_end + len);
        log.lock().unwrap().push((conn_no, request.clone()));
        match reply(conn_no, req_no, &request) {
            Reply::Send(bytes) => {
                if sock.write_all(&bytes).is_err() {
                    return;
                }
            }
            Reply::SendClose(bytes) => {
                let _ = sock.write_all(&bytes);
                return;
            }
            Reply::Close => return,
        }
    }
}

fn ok(body: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

fn run<T>(f: impl std::future::Future<Output = T>) -> T {
    rt_core::Executor::new().unwrap().block_on(f).unwrap()
}

fn url(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{port}{path}")
}

#[test]
fn interim_1xx_responses_are_read_past() {
    let (port, _) = serve("127.0.0.1:0", |_, _, _| {
        let mut r = b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 103 Early Hints\r\nLink: </s.css>; rel=preload\r\n\r\n".to_vec();
        r.extend(ok("final"));
        Reply::Send(r)
    });
    let c = HttpClient::new();
    run(async {
        for _ in 0..2 {
            let r = c.get(&url(port, "/")).await.unwrap();
            assert_eq!((r.status, r.text().as_str()), (200, "final"));
        }
        let s = c.get_streaming(&url(port, "/")).await.unwrap();
        assert_eq!(s.status(), 200);
    });
}

#[test]
fn a_head_that_never_ends_is_refused() {
    let (port, _) = serve("127.0.0.1:0", |_, _, _| {
        let mut r = b"HTTP/1.1 200 OK\r\n".to_vec();
        for i in 0..20_000 {
            r.extend(format!("X-Filler-{i}: aaaaaaaaaa\r\n").as_bytes());
        }
        Reply::SendClose(r)
    });
    let e = run(HttpClient::new().get(&url(port, "/"))).unwrap_err();
    assert!(matches!(e, HttpError::TooLarge(_)), "{e}");
}

#[test]
fn a_compression_bomb_is_stopped_at_the_limit() {
    // 100 MiB of zeros in ~100 KiB of gzip.
    let bomb = kernway_compress::encode(&vec![0u8; 100 << 20], kernway_compress::Encoding::Gzip);
    assert!(bomb.len() < 200 << 10, "{}", bomb.len());
    let (port, _) = serve("127.0.0.1:0", move |_, _, _| {
        let mut r = format!("HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n", bomb.len()).into_bytes();
        r.extend(&bomb);
        Reply::Send(r)
    });
    let e = run(HttpClient::new().max_response_size(1 << 20).get(&url(port, "/"))).unwrap_err();
    assert!(matches!(e, HttpError::TooLarge(_)), "{e}");
    let e = run(HttpClient::new().get(&url(port, "/"))).unwrap_err();
    assert!(matches!(e, HttpError::TooLarge(_)), "the default limit holds too: {e}");
    let r = run(HttpClient::new().max_response_size(100 << 20).get(&url(port, "/"))).unwrap();
    assert_eq!(r.body.len(), 100 << 20, "a limit that allows it inflates it all");
}

#[test]
fn a_body_longer_than_the_limit_is_refused_before_it_is_read() {
    let (port, _) = serve("127.0.0.1:0", |_, _, _| Reply::SendClose(b"HTTP/1.1 200 OK\r\nContent-Length: 999999999999\r\n\r\nabc".to_vec()));
    let e = run(HttpClient::new().get(&url(port, "/"))).unwrap_err();
    assert!(matches!(e, HttpError::TooLarge(_)), "{e}");
}

#[test]
fn conflicting_lengths_are_an_error_not_a_guess() {
    let (port, _) = serve("127.0.0.1:0", |_, _, _| {
        Reply::Send(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nContent-Length: 30\r\n\r\nabcHTTP/1.1 200 OK\r\n".to_vec())
    });
    let e = run(HttpClient::new().get(&url(port, "/"))).unwrap_err();
    assert!(matches!(&e, HttpError::Protocol(m) if m.contains("Content-Length")), "{e}");
}

#[test]
fn an_injected_header_never_reaches_the_wire() {
    let (port, log) = serve("127.0.0.1:0", |_, _, _| Reply::Send(ok("x")));
    let c = HttpClient::new();
    run(async {
        let attack = Request::new(Method::Get, Url::parse(&url(port, "/")).unwrap()).header("X-Name", "a\r\nX-Admin: true");
        assert!(matches!(c.send(attack).await, Err(HttpError::InvalidRequest(_))));
        let smuggle = Request::new(Method::Get, Url::parse(&url(port, "/")).unwrap()).header("X-Name", "a\r\n\r\nGET /admin HTTP/1.1");
        assert!(matches!(c.send_streaming(smuggle).await, Err(HttpError::InvalidRequest(_))));
        assert!(Url::parse(&url(port, "/a\r\nX: y")).is_err());
        assert_eq!(c.get(&url(port, "/after")).await.unwrap().status, 200);
    });
    let seen = log.lock().unwrap();
    assert_eq!(seen.len(), 1, "only the honest request was sent");
    assert!(!seen[0].1.contains("X-Admin"));
}

#[test]
fn a_post_that_got_out_is_never_sent_twice() {
    // The server reads the second request on the kept-alive connection, then
    // dies without answering: the POST may have been acted on.
    let (port, log) = serve("127.0.0.1:0", |conn, req, _| if conn == 0 && req == 1 { Reply::Close } else { Reply::Send(ok("ok")) });
    let c = HttpClient::new();
    run(async {
        assert_eq!(c.get(&url(port, "/warm")).await.unwrap().status, 200);
        let e = c.send(Request::new(Method::Post, Url::parse(&url(port, "/pay")).unwrap()).body("text/plain", "1 USD")).await.unwrap_err();
        assert!(matches!(&e, HttpError::Protocol(m) if m.contains("closed")), "{e}");
    });
    let posts = log.lock().unwrap().iter().filter(|(_, r)| r.starts_with("POST")).count();
    assert_eq!(posts, 1, "the POST reached the server exactly once");
}

#[test]
fn a_get_on_a_connection_that_died_is_resent_once() {
    let (port, log) = serve("127.0.0.1:0", |conn, req, _| if conn == 0 && req == 1 { Reply::Close } else { Reply::Send(ok("ok")) });
    let c = HttpClient::new();
    run(async {
        assert_eq!(c.get(&url(port, "/warm")).await.unwrap().status, 200);
        assert_eq!(c.get(&url(port, "/again")).await.unwrap().text(), "ok", "retried on a new connection");
        let keyed = Request::new(Method::Post, Url::parse(&url(port, "/keyed")).unwrap()).header("Idempotency-Key", "k-1");
        assert_eq!(c.send(keyed).await.unwrap().status, 200);
    });
    let seen = log.lock().unwrap();
    let again: Vec<usize> = seen.iter().filter(|(_, r)| r.starts_with("GET /again")).map(|(c, _)| *c).collect();
    assert_eq!(again, [0, 1], "sent on the dead connection, then once on a fresh one");
}

#[test]
fn connections_are_reused_only_when_in_step() {
    // Per request path: what the server answers. The log shows which connection
    // each request came on.
    let (port, log) = serve("127.0.0.1:0", |_, _, req| {
        let path = req.split(' ').nth(1).unwrap_or("");
        Reply::Send(match path {
            "/trailers" => b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\nX-Sum: 9\r\n\r\n".to_vec(),
            "/extra" => b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nokGARBAGE".to_vec(),
            "/http10" => b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec(),
            "/close" => b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nok".to_vec(),
            _ => ok("ok"),
        })
    });
    let c = HttpClient::new();
    let conn_of = |path: &str| {
        let seen = log.lock().unwrap();
        seen.iter().rev().find(|(_, r)| r.split(' ').nth(1) == Some(path)).map(|(c, _)| *c).unwrap()
    };
    run(async {
        let r = c.get(&url(port, "/trailers")).await.unwrap();
        assert_eq!((r.text().as_str(), r.trailer("x-sum")), ("abc", Some("9")));
        c.get(&url(port, "/a")).await.unwrap();
        assert_eq!(conn_of("/a"), conn_of("/trailers"), "after a trailer the connection is in step and reused");

        c.get(&url(port, "/extra")).await.unwrap();
        c.get(&url(port, "/b")).await.unwrap();
        assert_ne!(conn_of("/b"), conn_of("/extra"), "bytes past the body: not reused");

        c.get(&url(port, "/http10")).await.unwrap();
        c.get(&url(port, "/c")).await.unwrap();
        assert_ne!(conn_of("/c"), conn_of("/http10"), "HTTP/1.0 without keep-alive closes");

        c.get(&url(port, "/close")).await.unwrap();
        c.get(&url(port, "/d")).await.unwrap();
        assert_ne!(conn_of("/d"), conn_of("/close"));
    });
}

#[test]
fn every_resolved_address_is_tried() {
    // The server is only on ::1; `localhost` resolves to 127.0.0.1 first on most
    // systems, which refuses — the client must go on to ::1.
    let Ok(listener) = TcpListener::bind("[::1]:0") else {
        panic!("this host has no IPv6 loopback");
    };
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let (port, _) = serve(&format!("[::1]:{port}"), |_, _, _| Reply::Send(ok("v6")));
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err(), "nothing on the IPv4 side");
    let r = run(HttpClient::new().get(&format!("http://localhost:{port}/"))).unwrap();
    assert_eq!(r.text(), "v6");
}

#[test]
fn a_server_that_never_answers_the_connect_cannot_use_the_whole_budget_per_address() {
    // 10.255.255.1 is unroutable: connects hang until the timeout.
    let started = std::time::Instant::now();
    let e = run(HttpClient::new().connect_timeout(Duration::from_millis(600)).get("http://10.255.255.1:81/")).unwrap_err();
    assert!(matches!(e, HttpError::Timeout | HttpError::Io(_)), "{e}");
    assert!(started.elapsed() < Duration::from_secs(3), "bounded by the connect timeout: {:?}", started.elapsed());
}
