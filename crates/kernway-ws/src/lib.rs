//! `kernway-ws` — WebSocket client tối giản (RFC 6455) trên runtime Kernway.
//!
//! Transport tái dùng `rt_net::AsyncTcpStream` + `kernway_http_client::tls::AsyncTlsStream`
//! (rustls của Kernway) → `wss://`. Client-only: gửi text frame (masked), nhận text +
//! tự trả lời ping/pong, gộp fragmentation. Không phụ thuộc framework ngoài.
//!
//! ```no_run
//! # async fn run() -> std::io::Result<()> {
//! use kernway_ws::{WsClient, WsMsg};
//! let mut ws = WsClient::connect("ws.okx.com", 8443, "/ws/v5/public").await?;
//! ws.send_text("{\"op\":\"subscribe\", ...}").await?;
//! loop {
//!     match ws.read().await? { WsMsg::Text(t) => { /* ... */ }, WsMsg::Close => break, _ => {} }
//! }
//! # Ok(()) }
//! ```

use std::io;
use std::net::ToSocketAddrs;

use kernway_http_client::tls::{default_config, AsyncTlsStream};
use rt_net::AsyncTcpStream;

/// Thông điệp nhận được.
pub enum WsMsg {
    Text(String),
    Close,
    /// Binary / opcode khác — bỏ qua ở tầng trên.
    Other,
}

/// Mask cố định (client→server BẮT BUỘC mask). Trực tiếp TLS tới server tin cậy nên
/// không cần mask ngẫu nhiên (chỉ chống cache-poisoning qua proxy — ta không qua proxy).
const MASK: [u8; 4] = [0xA5, 0x5A, 0x3C, 0xC3];

pub struct WsClient {
    tls: AsyncTlsStream,
    buf: Vec<u8>, // byte đã đọc dư, chưa tiêu thụ
}

impl WsClient {
    /// Kết nối `wss://{host}:{port}{path}` — DNS + TCP + TLS + WS handshake (kiểm 101).
    pub async fn connect(host: &str, port: u16, path: &str) -> io::Result<Self> {
        // DNS blocking (getaddrinfo) — 1 lần mỗi kết nối, chấp nhận được.
        let addr = (host, port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "dns: no address"))?;
        let tcp = AsyncTcpStream::connect(addr).await?;
        let tls = AsyncTlsStream::connect(tcp, default_config(), host).await?;
        let mut c = WsClient { tls, buf: Vec::new() };

        // Handshake. Key cố định hợp lệ + BỎ verify Sec-WebSocket-Accept (client→server tin cậy).
        let req = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        c.tls.write_all(req.as_bytes()).await?;

        let head = c.read_http_head().await?;
        let status_ok = head
            .lines()
            .next()
            .map(|l| l.contains(" 101"))
            .unwrap_or(false);
        if !status_ok {
            let line = head.lines().next().unwrap_or("").to_string();
            return Err(io::Error::new(io::ErrorKind::ConnectionRefused, format!("ws handshake: {line}")));
        }
        Ok(c)
    }

    /// Gửi một text frame (masked).
    pub async fn send_text(&mut self, text: &str) -> io::Result<()> {
        self.send_frame(0x81, text.as_bytes()).await // FIN + opcode text
    }

    /// Nhận thông điệp kế tiếp; tự trả lời ping (control 0x9) + gộp continuation.
    pub async fn read(&mut self) -> io::Result<WsMsg> {
        let mut acc: Vec<u8> = Vec::new();
        let mut msg_opcode = 0u8;
        loop {
            let b0 = self.take(1).await?[0];
            let fin = b0 & 0x80 != 0;
            let opcode = b0 & 0x0f;
            let b1 = self.take(1).await?[0];
            let masked = b1 & 0x80 != 0;
            let mut len = (b1 & 0x7f) as usize;
            if len == 126 {
                let e = self.take(2).await?;
                len = u16::from_be_bytes([e[0], e[1]]) as usize;
            } else if len == 127 {
                let e = self.take(8).await?;
                len = u64::from_be_bytes([e[0], e[1], e[2], e[3], e[4], e[5], e[6], e[7]]) as usize;
            }
            let mask = if masked { Some(self.take(4).await?) } else { None };
            let mut payload = self.take(len).await?;
            if let Some(m) = &mask {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= m[i % 4];
                }
            }

            match opcode {
                0x9 => {
                    // ping (WS control) → pong, giữ payload
                    self.send_frame(0x8A, &payload).await?;
                    continue;
                }
                0xA => continue,                 // pong → bỏ
                0x8 => return Ok(WsMsg::Close),   // close
                0x0 => acc.extend_from_slice(&payload), // continuation
                _ => {
                    // frame mở đầu (text 0x1 / binary 0x2)
                    msg_opcode = opcode;
                    acc.extend_from_slice(&payload);
                }
            }
            if fin {
                return Ok(if msg_opcode == 0x1 {
                    WsMsg::Text(String::from_utf8_lossy(&acc).into_owned())
                } else {
                    WsMsg::Other
                });
            }
        }
    }

    // --- nội bộ ---

    async fn send_frame(&mut self, b0: u8, payload: &[u8]) -> io::Result<()> {
        let mut f = Vec::with_capacity(payload.len() + 14);
        f.push(b0);
        let len = payload.len();
        if len < 126 {
            f.push(0x80 | len as u8);
        } else if len < 65536 {
            f.push(0x80 | 126);
            f.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            f.push(0x80 | 127);
            f.extend_from_slice(&(len as u64).to_be_bytes());
        }
        f.extend_from_slice(&MASK);
        for (i, b) in payload.iter().enumerate() {
            f.push(b ^ MASK[i % 4]);
        }
        self.tls.write_all(&f).await
    }

    async fn read_http_head(&mut self) -> io::Result<String> {
        loop {
            if let Some(pos) = find_subseq(&self.buf, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&self.buf[..pos]).into_owned();
                self.buf.drain(..pos + 4);
                return Ok(head);
            }
            self.fill_more().await?;
        }
    }

    async fn take(&mut self, n: usize) -> io::Result<Vec<u8>> {
        while self.buf.len() < n {
            self.fill_more().await?;
        }
        let out = self.buf[..n].to_vec();
        self.buf.drain(..n);
        Ok(out)
    }

    async fn fill_more(&mut self) -> io::Result<()> {
        let mut tmp = [0u8; 8192];
        let n = self.tls.read(&mut tmp).await?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"));
        }
        self.buf.extend_from_slice(&tmp[..n]);
        Ok(())
    }
}

fn find_subseq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}
