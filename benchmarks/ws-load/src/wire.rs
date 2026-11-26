//! The client side of RFC 6455, just enough for load generation: masked frame
//! encoding (fresh random mask per frame), incremental parsing of the server's
//! unmasked frames, and the timestamp layout of benchmark payloads.

use std::sync::OnceLock;
use std::time::Instant;

/// Frame opcodes used by the client.
pub mod op {
    /// UTF-8 text.
    pub const TEXT: u8 = 0x1;
    /// Binary data.
    pub const BINARY: u8 = 0x2;
    /// Close.
    pub const CLOSE: u8 = 0x8;
    /// Ping.
    pub const PING: u8 = 0x9;
    /// Pong.
    pub const PONG: u8 = 0xA;
}

/// xorshift64* — a fast per-shard PRNG for masks and phase offsets.
pub struct Rng(u64);

impl Rng {
    /// Seeded generator (a zero seed is replaced).
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Append one masked client frame (FIN set) carrying `payload` to `out`.
pub fn encode_client_frame(out: &mut Vec<u8>, opcode: u8, payload: &[u8], mask: [u8; 4]) {
    out.push(0x80 | opcode);
    let len = payload.len();
    if len < 126 {
        out.push(0x80 | len as u8);
    } else if len <= u16::MAX as usize {
        out.push(0x80 | 126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(0x80 | 127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(&mask);
    let start = out.len();
    out.extend_from_slice(payload);
    for (i, b) in out[start..].iter_mut().enumerate() {
        *b ^= mask[i & 3];
    }
}

/// One complete server frame found at the start of a buffer.
#[derive(Debug, PartialEq, Eq)]
pub struct ServerFrame {
    /// FIN bit.
    pub fin: bool,
    /// Opcode.
    pub opcode: u8,
    /// Payload range within the buffer.
    pub payload: std::ops::Range<usize>,
    /// Total bytes the frame occupies.
    pub len: usize,
}

/// Why a server frame was rejected.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameError {
    /// Servers must not mask (RFC 6455 §5.1).
    Masked,
}

/// Parse the server frame at the start of `buf`; `Ok(None)` if it is not all
/// buffered yet.
pub fn parse_server_frame(buf: &[u8]) -> Result<Option<ServerFrame>, FrameError> {
    if buf.len() < 2 {
        return Ok(None);
    }
    if buf[1] & 0x80 != 0 {
        return Err(FrameError::Masked);
    }
    let (len, hdr) = match buf[1] & 0x7f {
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            (usize::from(u16::from_be_bytes([buf[2], buf[3]])), 4)
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[2..10]);
            (u64::from_be_bytes(b) as usize, 10)
        }
        n => (usize::from(n), 2),
    };
    if buf.len() < hdr + len {
        return Ok(None);
    }
    Ok(Some(ServerFrame {
        fin: buf[0] & 0x80 != 0,
        opcode: buf[0] & 0x0f,
        payload: hdr..hdr + len,
        len: hdr + len,
    }))
}

static BASE: OnceLock<Instant> = OnceLock::new();

/// Monotonic nanoseconds since the process's benchmark epoch. Every client
/// shard thread shares the epoch, so a stamp taken on one shard can be
/// subtracted on another.
pub fn now_ns() -> u64 {
    BASE.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// The [`Instant`] that [`now_ns`] would report as `ns`.
pub fn instant_at(ns: u64) -> Instant {
    *BASE.get_or_init(Instant::now) + std::time::Duration::from_nanos(ns)
}

/// Smallest payload a timed message can have: `[scheduled_ns][sent_ns]`.
pub const STAMP_LEN: usize = 16;

/// Write the two stamps at the front of `payload` (which must be at least
/// [`STAMP_LEN`] bytes).
pub fn stamp(payload: &mut [u8], scheduled_ns: u64, sent_ns: u64) {
    payload[..8].copy_from_slice(&scheduled_ns.to_le_bytes());
    payload[8..16].copy_from_slice(&sent_ns.to_le_bytes());
}

/// Read `(scheduled_ns, sent_ns)` back, if the payload is long enough.
#[must_use]
pub fn read_stamp(payload: &[u8]) -> Option<(u64, u64)> {
    if payload.len() < STAMP_LEN {
        return None;
    }
    let mut a = [0u8; 8];
    let mut b = [0u8; 8];
    a.copy_from_slice(&payload[..8]);
    b.copy_from_slice(&payload[8..16]);
    Some((u64::from_le_bytes(a), u64::from_le_bytes(b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unmask a client frame the way a server does, to check the encoder.
    fn decode_client(f: &[u8]) -> (u8, Vec<u8>) {
        assert_eq!(f[1] & 0x80, 0x80, "client frames must be masked");
        let (len, mut at) = match f[1] & 0x7f {
            126 => (usize::from(u16::from_be_bytes([f[2], f[3]])), 4),
            127 => (
                u64::from_be_bytes(f[2..10].try_into().unwrap()) as usize,
                10,
            ),
            n => (usize::from(n), 2),
        };
        let mask = [f[at], f[at + 1], f[at + 2], f[at + 3]];
        at += 4;
        assert_eq!(f.len(), at + len);
        let p = f[at..]
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[i & 3])
            .collect();
        (f[0] & 0x0f, p)
    }

    #[test]
    fn masked_frames_round_trip_at_every_length_class() {
        for len in [0usize, 5, 125, 126, 200, 65_535, 65_536, 70_000] {
            let payload: Vec<u8> = (0..len).map(|i| (i * 7) as u8).collect();
            let mut out = Vec::new();
            encode_client_frame(&mut out, op::BINARY, &payload, [9, 8, 7, 6]);
            assert_eq!(decode_client(&out), (op::BINARY, payload), "len {len}");
        }
    }

    #[test]
    fn parses_server_frames_incrementally() {
        let mut f = vec![0x82, 126, 0x01, 0x00];
        f.extend(std::iter::repeat_n(0xAB, 256));
        for cut in 0..f.len() {
            assert_eq!(parse_server_frame(&f[..cut]), Ok(None), "cut {cut}");
        }
        let got = parse_server_frame(&f).unwrap().unwrap();
        assert_eq!(
            (got.opcode, got.fin, got.payload, got.len),
            (2, true, 4..260, 260)
        );
        assert_eq!(
            parse_server_frame(&[0x81, 0x81, 0, 0, 0, 0, 0]),
            Err(FrameError::Masked)
        );
    }

    #[test]
    fn stamps_round_trip() {
        let mut p = vec![0u8; 32];
        stamp(&mut p, 11, 22);
        assert_eq!(read_stamp(&p), Some((11, 22)));
        assert_eq!(read_stamp(&p[..10]), None);
    }
}
