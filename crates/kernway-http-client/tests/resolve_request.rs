//! Combination test for slice 4: a real HTTP request through `HttpClient`,
//! driven end to end by the async resolver (IP-literal and `localhost` paths) →
//! async connect → send → parse. A tiny loopback TCP server stands in for the
//! remote; no external network is touched.

use kernway_http_client::{HttpClient, Method, Request, Url};
use rt_core::Executor;
use rt_net::AsyncTcpListener;

const RESPONSE: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello";

/// Bind a one-shot HTTP server on loopback; returns its port. It accepts a single
/// connection, reads the request head, and writes a canned 200 response.
fn spawn_one_shot_server() -> u16 {
    let mut listener = AsyncTcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = listener.local_addr().unwrap().port();
    rt_core::spawn(async move {
        let (mut conn, _peer) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        // One read is enough for a small GET request head.
        let _ = conn.read(&mut buf).await.unwrap();
        conn.write_all(RESPONSE).await.unwrap();
        let _ = conn.shutdown(std::net::Shutdown::Write);
    });
    port
}

fn get(url: &str) -> (u16, String) {
    let ex = Executor::new().unwrap();
    ex.block_on(async move {
        let port = spawn_one_shot_server();
        let real = url.replace("{port}", &port.to_string());
        let client = HttpClient::new();
        let resp = client
            .send(Request::new(Method::Get, Url::parse(&real).unwrap()))
            .await
            .unwrap();
        (resp.status, String::from_utf8_lossy(&resp.body).into_owned())
    })
    .unwrap()
}

#[test]
fn get_over_loopback_via_ip_literal() {
    let (status, body) = get("http://127.0.0.1:{port}/");
    assert_eq!(status, 200);
    assert_eq!(body, "hello");
}

#[test]
fn get_over_loopback_via_localhost() {
    // Exercises the /etc/hosts + localhost resolve path, not the IP fast path.
    let (status, body) = get("http://localhost:{port}/");
    assert_eq!(status, 200);
    assert_eq!(body, "hello");
}
