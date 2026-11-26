//! WebSocket framing (RFC 6455 §5) for the **server** side: parse masked client
//! frames incrementally out of a byte buffer, and encode unmasked server frames.

/// Frame opcodes (RFC 6455 §5.2).
pub mod opcode {
    pub const CONTINUATION: u8 = 0x0;
    pub const TEXT: u8 = 0x1;
    pub const BINARY: u8 = 0x2;
    pub const CLOSE: u8 = 0x8;
    pub const PING: u8 = 0x9;
    pub const PONG: u8 = 0xA;
}

/// Close status codes used by the server (RFC 6455 §7.4.1).
pub mod close_code {
    /// Normal closure.
    pub const NORMAL: u16 = 1000;
    /// The endpoint is going away (server shutdown, idle timeout).
    pub const GOING_AWAY: u16 = 1001;
    /// Protocol error.
    pub const PROTOCOL: u16 = 1002;
    /// Data type the endpoint cannot accept.
    pub const UNSUPPORTED: u16 = 1003;
    /// Payload not consistent with the message type (bad UTF-8 in a text message).
    pub const INVALID_DATA: u16 = 1007;
    /// Policy violation (e.g. a client too slow to keep up).
    pub const POLICY: u16 = 1008;
    /// Message too big.
    pub const TOO_BIG: u16 = 1009;
    /// Unexpected server condition.
    pub const INTERNAL: u16 = 1011;
}

/// One parsed frame, payload already unmasked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Final fragment of a message.
    pub fin: bool,
    /// See [`opcode`].
    pub opcode: u8,
    /// Unmasked payload.
    pub payload: Vec<u8>,
}

/// A frame the server must reject, with the close code to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameError {
    /// Close code for the reply.
    pub code: u16,
    /// Short reason (sent in the close frame).
    pub reason: &'static str,
}

const fn err(code: u16, reason: &'static str) -> FrameError {
    FrameError { code, reason }
}

/// The header of a client frame (RFC 6455 §5.2), parsed without touching the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Final fragment of a message.
    pub fin: bool,
    /// See [`opcode`].
    pub opcode: u8,
    /// Masking key.
    pub mask: [u8; 4],
    /// Offset of the payload from the start of the frame.
    pub payload_start: usize,
    /// Payload length.
    pub payload_len: usize,
}

impl Header {
    /// Whole frame length: header + payload.
    #[must_use]
    pub fn frame_len(&self) -> usize {
        self.payload_start + self.payload_len
    }
}

/// Parse a client frame **header** from the front of `buf`.
///
/// - `Ok(Some(h))` — the header is complete (the payload may not be yet: check
///   `buf.len() >= h.frame_len()`); knowing the size up front lets the reader
///   grow its buffer once instead of in steps;
/// - `Ok(None)` — the header itself is incomplete;
/// - `Err(e)` — protocol violation; close with `e.code`.
///
/// Enforces what a server must (§5.1–5.5): client frames are masked, RSV bits
/// are zero (no extensions negotiated), opcodes are known, control frames are
/// unfragmented and ≤ 125 bytes, and no frame exceeds `max_frame`.
pub fn parse_client_header(buf: &[u8], max_frame: usize) -> Result<Option<Header>, FrameError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let (b0, b1) = (buf[0], buf[1]);
    let fin = b0 & 0x80 != 0;
    if b0 & 0x70 != 0 {
        return Err(err(close_code::PROTOCOL, "reserved bits set"));
    }
    let op = b0 & 0x0f;
    let is_control = op & 0x08 != 0;
    match op {
        opcode::CONTINUATION
        | opcode::TEXT
        | opcode::BINARY
        | opcode::CLOSE
        | opcode::PING
        | opcode::PONG => {}
        _ => return Err(err(close_code::PROTOCOL, "unknown opcode")),
    }
    if b1 & 0x80 == 0 {
        return Err(err(close_code::PROTOCOL, "client frame not masked"));
    }
    let (len, pos) = match b1 & 0x7f {
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (u64::from(u16::from_be_bytes([buf[2], buf[3]])), 4)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[2..10]);
            let n = u64::from_be_bytes(b);
            if n >> 63 != 0 {
                return Err(err(close_code::PROTOCOL, "length overflow"));
            }
            (n, 10)
        }
        n => (u64::from(n), 2),
    };
    if is_control && (!fin || len > 125) {
        return Err(err(close_code::PROTOCOL, "bad control frame"));
    }
    if len > max_frame as u64 {
        return Err(err(close_code::TOO_BIG, "frame too big"));
    }
    if buf.len() < pos + 4 {
        return Ok(None);
    }
    Ok(Some(Header {
        fin,
        opcode: op,
        mask: [buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]],
        payload_start: pos + 4,
        payload_len: len as usize,
    }))
}

/// Unmask `data` in place (XOR with the 4-byte key, RFC 6455 §5.3).
///
/// Eight bytes per step with one `u64` XOR — the key repeated twice — instead of
/// a byte loop with `i % 4`; the remainder (< 8 bytes) is done byte-wise. The
/// chunk loop is branch-free, so the compiler vectorises it further.
pub fn unmask(data: &mut [u8], mask: [u8; 4]) {
    let wide = u64::from_ne_bytes([
        mask[0], mask[1], mask[2], mask[3], mask[0], mask[1], mask[2], mask[3],
    ]);
    let mut chunks = data.chunks_exact_mut(8);
    for chunk in &mut chunks {
        let mut word = [0u8; 8];
        word.copy_from_slice(chunk);
        chunk.copy_from_slice(&(u64::from_ne_bytes(word) ^ wide).to_ne_bytes());
    }
    // 8 is a multiple of 4: the key phase restarts at 0 for the tail.
    for (i, b) in chunks.into_remainder().iter_mut().enumerate() {
        *b ^= mask[i & 3];
    }
}

/// Parse one whole client frame from the front of `buf`, copying the unmasked
/// payload out. `Ok(Some((frame, used)))`, `Ok(None)` (need more), or `Err`.
///
/// Convenience over [`parse_client_header`] + [`unmask`]; the server's reader
/// uses those directly to unmask in place inside its own buffer.
pub fn parse_client_frame(
    buf: &[u8],
    max_frame: usize,
) -> Result<Option<(Frame, usize)>, FrameError> {
    let Some(h) = parse_client_header(buf, max_frame)? else {
        return Ok(None);
    };
    if buf.len() < h.frame_len() {
        return Ok(None);
    }
    let mut payload = buf[h.payload_start..h.frame_len()].to_vec();
    unmask(&mut payload, h.mask);
    Ok(Some((
        Frame {
            fin: h.fin,
            opcode: h.opcode,
            payload,
        },
        h.frame_len(),
    )))
}

/// Append one unmasked server frame (FIN set) to `out`.
pub fn encode_server_frame(out: &mut Vec<u8>, op: u8, payload: &[u8]) {
    out.push(0x80 | op);
    let len = payload.len();
    if len < 126 {
        out.push(len as u8);
    } else if len <= usize::from(u16::MAX) {
        out.push(126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
}

/// The payload of a close frame: code + UTF-8 reason (reason trimmed to fit 125 bytes).
#[must_use]
pub fn close_payload(code: u16, reason: &str) -> Vec<u8> {
    let mut p = code.to_be_bytes().to_vec();
    let mut r = reason;
    while r.len() > 123 {
        let mut cut = 123;
        while !r.is_char_boundary(cut) {
            cut -= 1;
        }
        r = &r[..cut];
    }
    p.extend_from_slice(r.as_bytes());
    p
}

/// Validate a received close payload; returns the peer's code (1005 when absent).
pub fn parse_close(payload: &[u8]) -> Result<u16, FrameError> {
    match payload.len() {
        0 => Ok(1005), // "no status received" — never sent on the wire
        1 => Err(err(close_code::PROTOCOL, "bad close payload")),
        _ => {
            let code = u16::from_be_bytes([payload[0], payload[1]]);
            let valid = matches!(code, 1000..=1003 | 1007..=1011 | 3000..=4999);
            if !valid {
                return Err(err(close_code::PROTOCOL, "bad close code"));
            }
            if std::str::from_utf8(&payload[2..]).is_err() {
                return Err(err(close_code::INVALID_DATA, "bad close reason"));
            }
            Ok(code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mask a payload the way a client does.
    fn client_frame(b0: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        let mut f = vec![b0];
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
        f.extend_from_slice(&mask);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i & 3]));
        f
    }

    #[test]
    fn wide_unmask_matches_the_byte_loop() {
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        for len in 0..70 {
            let original: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
            let mut fast = original.clone();
            unmask(&mut fast, mask);
            let slow: Vec<u8> = original
                .iter()
                .enumerate()
                .map(|(i, b)| b ^ mask[i & 3])
                .collect();
            assert_eq!(fast, slow, "len {len}");
            unmask(&mut fast, mask); // involution
            assert_eq!(fast, original);
        }
    }

    #[test]
    fn header_is_known_before_the_payload_arrives() {
        let f = client_frame(0x82, &[5u8; 300]);
        let h = parse_client_header(&f[..8], 1 << 20).unwrap().unwrap();
        assert_eq!(
            (h.payload_start, h.payload_len, h.frame_len()),
            (8, 300, 308)
        );
        assert_eq!(parse_client_header(&f[..7], 1 << 20).unwrap(), None);
    }

    #[test]
    fn rfc_example_masked_hello() {
        // RFC 6455 §5.7: a single-frame masked text message "Hello".
        let bytes = [
            0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58,
        ];
        let (frame, used) = parse_client_frame(&bytes, 1 << 20).unwrap().unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(
            frame,
            Frame {
                fin: true,
                opcode: opcode::TEXT,
                payload: b"Hello".to_vec()
            }
        );
        // Any prefix is "need more".
        for cut in 0..bytes.len() {
            assert_eq!(parse_client_frame(&bytes[..cut], 1 << 20).unwrap(), None);
        }
    }

    #[test]
    fn extended_lengths() {
        for len in [125usize, 126, 65535, 65536, 70000] {
            let payload = vec![7u8; len];
            let f = client_frame(0x82, &payload);
            let (frame, used) = parse_client_frame(&f, 1 << 20).unwrap().unwrap();
            assert_eq!(used, f.len());
            assert_eq!(frame.payload, payload);
        }
    }

    #[test]
    fn rejects_protocol_violations() {
        let unmasked = [0x81, 0x05, b'H', b'e', b'l', b'l', b'o'];
        assert_eq!(
            parse_client_frame(&unmasked, 1024).unwrap_err().code,
            close_code::PROTOCOL
        );
        let rsv = client_frame(0xC1, b"x");
        assert_eq!(
            parse_client_frame(&rsv, 1024).unwrap_err().code,
            close_code::PROTOCOL
        );
        let bad_op = client_frame(0x83, b"x");
        assert_eq!(
            parse_client_frame(&bad_op, 1024).unwrap_err().code,
            close_code::PROTOCOL
        );
        let fragmented_ping = client_frame(0x09, b"x");
        assert_eq!(
            parse_client_frame(&fragmented_ping, 1024).unwrap_err().code,
            close_code::PROTOCOL
        );
        let long_ping = client_frame(0x89, &[0u8; 126]);
        assert_eq!(
            parse_client_frame(&long_ping, 1024).unwrap_err().code,
            close_code::PROTOCOL
        );
        let big = client_frame(0x82, &[0u8; 2000]);
        assert_eq!(
            parse_client_frame(&big, 1024).unwrap_err().code,
            close_code::TOO_BIG
        );
    }

    #[test]
    fn server_frames_are_unmasked_with_right_lengths() {
        for (len, header) in [(5usize, 2usize), (200, 4), (70_000, 10)] {
            let mut out = Vec::new();
            encode_server_frame(&mut out, opcode::BINARY, &vec![1u8; len]);
            assert_eq!(out[0], 0x82);
            assert_eq!(out[1] & 0x80, 0, "server frames must not be masked");
            assert_eq!(out.len(), header + len);
        }
    }

    #[test]
    fn close_payloads() {
        assert_eq!(parse_close(&close_payload(1000, "bye")).unwrap(), 1000);
        assert_eq!(parse_close(&[]).unwrap(), 1005);
        assert!(parse_close(&[3]).is_err());
        assert!(parse_close(&1005u16.to_be_bytes()).is_err());
        assert!(parse_close(&[0x03, 0xe8, 0xff]).is_err()); // bad UTF-8 reason
        assert!(close_payload(1000, &"é".repeat(100)).len() <= 125);
    }
}
