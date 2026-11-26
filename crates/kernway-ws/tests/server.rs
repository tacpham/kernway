//! End-to-end: a real TCP socket, the same 101 hand-over kernway-server does,
//! and a hand-rolled client speaking raw RFC 6455 frames.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use kernway_core::layer::BoxFuture;
use kernway_core::response::{Body, Response, Upgraded, UpgradedRead, UpgradedWrite};
use kernway_http::{encode_head, encode_response, parse_head, Connection, ParsedHead};
use kernway_ws::{close_code, upgrade, Message, PreparedMessage, SendError, WsConfig};
use rt_net::stream::{TcpReadHalf, TcpWriteHalf};

// ---------------------------------------------------------------- server side

struct R(TcpReadHalf);
impl UpgradedRead for R {
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> BoxFuture<'a, std::io::Result<usize>> {
        Box::pin(self.0.read(buf))
    }
}
struct W(TcpWriteHalf);
impl UpgradedWrite for W {
    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(self.0.write_all(buf))
    }
    fn shutdown(&mut self) {
        let _ = self.0.shutdown(std::net::Shutdown::Write);
    }
}

/// Accept one connection, read its request head, let `route` answer, and — like
/// kernway-server — hand a `Body::Upgrade` the socket after writing the 101.
fn serve_one(
    route: impl FnOnce(&kernway_core::request::Request) -> Response + Send + 'static,
) -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (sock, _) = listener.accept().unwrap();
        let ex = rt_core::Executor::new().unwrap();
        ex.block_on(async move {
            let mut stream = rt_net::AsyncTcpStream::from_std(sock).unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let (req, head_end) = loop {
                if let Ok(ParsedHead::Complete {
                    request, head_end, ..
                }) = parse_head(&buf)
                {
                    break (request, head_end);
                }
                let n = stream.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
            };
            let mut response = route(&req);
            if let Body::Upgrade { .. } = response.body {
                let Body::Upgrade { on_upgrade } =
                    std::mem::replace(&mut response.body, Body::Empty)
                else {
                    unreachable!()
                };
                stream
                    .write_all(&encode_head(&response, Connection::Upgrade, 0))
                    .await
                    .unwrap();
                let (r, w) = stream.into_split();
                on_upgrade(Upgraded {
                    reader: Box::new(R(r)),
                    writer: Box::new(W(w)),
                    buffered: buf[head_end..].to_vec(),
                    remote_addr: None,
                })
                .await;
            } else {
                stream.write_all(&encode_response(&response)).await.unwrap();
            }
            // Let the writer/ping tasks finish before the executor goes away.
            rt_core::sleep(Duration::from_millis(50)).await;
        })
        .unwrap();
    });
    addr
}

/// Echo server: text back as text, binary back as binary.
fn echo_server(config: WsConfig) -> std::net::SocketAddr {
    serve_one(move |req| {
        upgrade(req, config, |mut ws| async move {
            let tx = ws.sender();
            while let Some(m) = ws.recv().await {
                let _ = match m {
                    Message::Text(t) => tx.send_text(&t),
                    Message::Binary(b) => tx.send_binary(&b),
                };
            }
        })
    })
}

// ---------------------------------------------------------------- raw client

struct Client {
    sock: TcpStream,
    buf: Vec<u8>,
}

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

impl Client {
    fn connect(addr: std::net::SocketAddr, extra_headers: &str) -> (Self, String) {
        let mut sock = TcpStream::connect(addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            sock,
            "GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {KEY}\r\n{extra_headers}\r\n"
        )
        .unwrap();
        let mut c = Client {
            sock,
            buf: Vec::new(),
        };
        let head = loop {
            if let Some(p) = c.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&c.buf[..p]).into_owned();
                c.buf.drain(..p + 4);
                break head;
            }
            c.fill();
        };
        (c, head)
    }

    fn fill(&mut self) {
        let mut tmp = [0u8; 8192];
        let n = self.sock.read(&mut tmp).unwrap();
        assert!(n > 0, "server closed the socket");
        self.buf.extend_from_slice(&tmp[..n]);
    }

    fn send(&mut self, b0: u8, payload: &[u8], masked: bool) {
        let mask = [1u8, 2, 3, 4];
        let mut f = vec![b0];
        let mbit = if masked { 0x80 } else { 0 };
        if payload.len() < 126 {
            f.push(mbit | payload.len() as u8);
        } else {
            f.push(mbit | 126);
            f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        if masked {
            f.extend_from_slice(&mask);
            f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
        } else {
            f.extend_from_slice(payload);
        }
        self.sock.write_all(&f).unwrap();
    }

    /// Next server frame: (opcode, payload). Server frames are never masked.
    fn frame(&mut self) -> (u8, Vec<u8>) {
        loop {
            if self.buf.len() >= 2 {
                assert_eq!(self.buf[1] & 0x80, 0, "server frame masked");
                let (len, hdr) = match self.buf[1] & 0x7f {
                    126 if self.buf.len() >= 4 => {
                        (u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize, 4)
                    }
                    126 => (usize::MAX, 0),
                    127 => panic!("unexpected huge frame"),
                    n => (n as usize, 2),
                };
                if len != usize::MAX && self.buf.len() >= hdr + len {
                    let op = self.buf[0] & 0x0f;
                    let payload = self.buf[hdr..hdr + len].to_vec();
                    self.buf.drain(..hdr + len);
                    return (op, payload);
                }
            }
            self.fill();
        }
    }

    /// Next frame that is not a ping.
    fn data(&mut self) -> (u8, Vec<u8>) {
        loop {
            let f = self.frame();
            if f.0 != 0x9 {
                return f;
            }
        }
    }
}

fn close_code_of(payload: &[u8]) -> u16 {
    u16::from_be_bytes([payload[0], payload[1]])
}

// ---------------------------------------------------------------- tests

#[test]
fn handshake_and_echo() {
    let addr = echo_server(WsConfig::default());
    let (mut c, head) = Client::connect(addr, "");
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(
        head.to_ascii_lowercase()
            .contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo="),
        "{head}"
    );
    assert!(!head.to_ascii_lowercase().contains("content-length"));

    c.send(0x81, "xin chào 將".as_bytes(), true);
    assert_eq!(c.data(), (0x1, "xin chào 將".as_bytes().to_vec()));
    let big = vec![9u8; 5000];
    c.send(0x82, &big, true);
    assert_eq!(c.data(), (0x2, big));
}

#[test]
fn ping_is_answered_and_fragments_are_assembled() {
    let addr = echo_server(WsConfig::default());
    let (mut c, _) = Client::connect(addr, "");
    c.send(0x89, b"hb", true); // ping
    assert_eq!(c.data(), (0xA, b"hb".to_vec())); // pong with the same payload
    c.send(0x01, b"ab", true); // text, not final
    c.send(0x89, b"", true); // a ping may interleave fragments
    c.send(0x00, b"cd", true); // continuation
    c.send(0x80, b"ef", true); // final continuation
    assert_eq!(c.data(), (0xA, Vec::new()));
    assert_eq!(c.data(), (0x1, b"abcdef".to_vec()));
}

#[test]
fn close_handshake_echoes_the_code() {
    let addr = echo_server(WsConfig::default());
    let (mut c, _) = Client::connect(addr, "");
    let mut payload = 1000u16.to_be_bytes().to_vec();
    payload.extend_from_slice(b"bye");
    c.send(0x88, &payload, true);
    let (op, p) = c.data();
    assert_eq!(op, 0x8);
    assert_eq!(close_code_of(&p), 1000);
}

#[test]
fn protocol_errors_close_with_the_right_code() {
    let addr = echo_server(WsConfig::default());
    let (mut c, _) = Client::connect(addr, "");
    c.send(0x81, &[0xff, 0xfe], true); // invalid UTF-8 text
    let (op, p) = c.data();
    assert_eq!((op, close_code_of(&p)), (0x8, close_code::INVALID_DATA));

    let addr = echo_server(WsConfig::default());
    let (mut c, _) = Client::connect(addr, "");
    c.send(0x81, b"hi", false); // unmasked client frame
    let (op, p) = c.data();
    assert_eq!((op, close_code_of(&p)), (0x8, close_code::PROTOCOL));

    let addr = echo_server(WsConfig {
        max_message: 10,
        ..WsConfig::default()
    });
    let (mut c, _) = Client::connect(addr, "");
    c.send(0x01, b"0123456", true);
    c.send(0x80, b"789ab", true); // 12 bytes > 10
    let (op, p) = c.data();
    assert_eq!((op, close_code_of(&p)), (0x8, close_code::TOO_BIG));
}

#[test]
fn bad_handshakes_are_refused() {
    let addr = echo_server(WsConfig::default());
    let mut sock = TcpStream::connect(addr).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    sock.write_all(
        b"GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
          Sec-WebSocket-Version: 8\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
    )
    .unwrap();
    let mut out = String::new();
    let _ = sock.read_to_string(&mut out);
    assert!(out.starts_with("HTTP/1.1 426 Upgrade Required"), "{out}");
    assert!(out.contains("sec-websocket-version: 13"), "{out}");
}

#[test]
fn subprotocol_is_negotiated() {
    let addr = echo_server(WsConfig {
        protocols: vec!["xq.v1".into()],
        ..WsConfig::default()
    });
    let (_c, head) = Client::connect(addr, "Sec-WebSocket-Protocol: chat, xq.v1\r\n");
    assert!(head.contains("sec-websocket-protocol: xq.v1"), "{head}");
}

#[test]
fn prepared_broadcast_and_backpressure() {
    // On open: try to queue 10 prepared frames into an outbox of 4 before the
    // writer task has had a chance to run, then report what happened.
    let addr = serve_one(|req| {
        upgrade(
            req,
            WsConfig {
                send_queue: 4,
                ..WsConfig::default()
            },
            |mut ws| async move {
                let tx = ws.sender();
                let msg = PreparedMessage::text("move h2e2");
                let mut full = 0;
                for _ in 0..10 {
                    if tx.send_prepared(&msg) == Err(SendError::Full) {
                        full += 1;
                    }
                }
                // Wait for the client to ask for the report.
                if ws.recv().await.is_some() {
                    let _ = tx.send_text(&format!("full={full}"));
                }
                while ws.recv().await.is_some() {}
            },
        )
    });
    let (mut c, _) = Client::connect(addr, "");
    for _ in 0..4 {
        assert_eq!(c.data(), (0x1, b"move h2e2".to_vec()));
    }
    c.send(0x81, b"report", true);
    assert_eq!(c.data(), (0x1, b"full=6".to_vec()));
}

#[test]
fn server_closes_when_the_app_returns() {
    let addr = serve_one(|req| {
        upgrade(req, WsConfig::default(), |ws| async move {
            let _ = ws.sender().send_text("hello, bye");
        })
    });
    let (mut c, _) = Client::connect(addr, "");
    assert_eq!(c.data(), (0x1, b"hello, bye".to_vec()));
    let (op, p) = c.data();
    assert_eq!((op, close_code_of(&p)), (0x8, close_code::NORMAL));
}

#[test]
fn idle_client_is_closed() {
    let addr = echo_server(WsConfig {
        idle_timeout: Duration::from_millis(150),
        ping_interval: None,
        ..WsConfig::default()
    });
    let (mut c, _) = Client::connect(addr, "");
    let (op, p) = c.data();
    assert_eq!((op, close_code_of(&p)), (0x8, close_code::GOING_AWAY));
}
