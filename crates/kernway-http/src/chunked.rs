//! The chunked transfer coding (RFC 9112 §7.1), decoded incrementally — shared by
//! the server (chunked request bodies) and the client (chunked responses).
//!
//! [`ChunkDecoder::pull`] consumes as much of a buffer as it can and leaves any
//! partial tail for the next call; it never re-parses from the start. A malformed
//! size line, a missing CRLF after a chunk's data, or an over-long size line or
//! trailer is an error rather than a guess: a reader that framed the body
//! differently from the sender would read the next message out of this one.

use std::fmt;

/// The longest chunk-size line accepted (the size plus any extensions).
pub const MAX_CHUNK_LINE: usize = 4096;

/// The largest trailer section accepted.
pub const MAX_TRAILER: usize = 64 * 1024;

/// Why a chunked body could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChunkError {
    /// The framing is broken (the reason says where).
    Malformed(&'static str),
    /// A size line or the trailer section is longer than allowed.
    TooLarge(&'static str),
}

impl fmt::Display for ChunkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChunkError::Malformed(m) => write!(f, "malformed chunked body: {m}"),
            ChunkError::TooLarge(m) => write!(f, "chunked body: {m} too large"),
        }
    }
}

impl std::error::Error for ChunkError {}

/// Where the decode sits between reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Size,
    Data(usize),
    DataCrlf,
    Trailer,
    Done,
}

/// An incremental chunked-body decoder.
#[derive(Debug, Clone)]
pub struct ChunkDecoder {
    state: State,
    /// The trailer field lines (`Name: value\r\n`…) after the last chunk.
    pub trailer: Vec<u8>,
}

impl Default for ChunkDecoder {
    fn default() -> Self {
        Self::new()
    }
}

fn find_crlf(b: &[u8]) -> Option<usize> {
    b.windows(2).position(|w| w == b"\r\n")
}

impl ChunkDecoder {
    /// A decoder at the start of a body.
    #[must_use]
    pub fn new() -> Self {
        Self { state: State::Size, trailer: Vec::new() }
    }

    /// Whether the last chunk and the trailer section have been consumed.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.state == State::Done
    }

    /// Consume as much of `buf` as possible, appending decoded body bytes to `out`.
    /// Unconsumed bytes stay at the front of `buf` — once [`is_done`](Self::is_done),
    /// they are the next message's.
    pub fn pull(&mut self, buf: &mut Vec<u8>, out: &mut Vec<u8>) -> Result<(), ChunkError> {
        let mut pos = 0;
        let result = loop {
            match self.state {
                State::Done => break Ok(()),
                State::Size => match find_crlf(&buf[pos..]) {
                    Some(i) => {
                        let line = &buf[pos..pos + i];
                        let hex = line.split(|&b| b == b';').next().unwrap_or(b"");
                        let hex = std::str::from_utf8(hex).map(|s| s.trim_matches([' ', '\t'])).unwrap_or("");
                        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                            break Err(ChunkError::Malformed("a bad chunk size"));
                        }
                        let Ok(size) = usize::from_str_radix(hex, 16) else {
                            break Err(ChunkError::TooLarge("a chunk size"));
                        };
                        pos += i + 2;
                        self.state = if size == 0 { State::Trailer } else { State::Data(size) };
                    }
                    None if buf.len() - pos > MAX_CHUNK_LINE => break Err(ChunkError::TooLarge("a chunk-size line")),
                    None => break Ok(()),
                },
                State::Data(rem) => {
                    let avail = buf.len() - pos;
                    if avail == 0 {
                        break Ok(());
                    }
                    let take = rem.min(avail);
                    out.extend_from_slice(&buf[pos..pos + take]);
                    pos += take;
                    self.state = if take == rem { State::DataCrlf } else { State::Data(rem - take) };
                }
                State::DataCrlf => {
                    if buf.len() - pos < 2 {
                        break Ok(());
                    }
                    if &buf[pos..pos + 2] != b"\r\n" {
                        break Err(ChunkError::Malformed("no CRLF after a chunk"));
                    }
                    pos += 2;
                    self.state = State::Size;
                }
                State::Trailer => match find_crlf(&buf[pos..]) {
                    Some(0) => {
                        pos += 2;
                        self.state = State::Done;
                    }
                    Some(i) => {
                        self.trailer.extend_from_slice(&buf[pos..pos + i + 2]);
                        pos += i + 2;
                        if self.trailer.len() > MAX_TRAILER {
                            break Err(ChunkError::TooLarge("the trailer"));
                        }
                    }
                    None if buf.len() - pos > MAX_TRAILER => break Err(ChunkError::TooLarge("the trailer")),
                    None => break Ok(()),
                },
            }
        };
        buf.drain(..pos);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bodies_trailers_and_what_follows() {
        let mut d = ChunkDecoder::new();
        let mut buf = b"4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\nX-Sum: 1\r\n\r\nNEXT".to_vec();
        let mut out = Vec::new();
        d.pull(&mut buf, &mut out).unwrap();
        assert!(d.is_done());
        assert_eq!((out.as_slice(), buf.as_slice(), d.trailer.as_slice()), (&b"Wikipedia"[..], &b"NEXT"[..], &b"X-Sum: 1\r\n"[..]));
        // Byte by byte, the same.
        let raw = b"3\r\nabc\r\n0\r\n\r\n";
        let (mut d, mut buf, mut out) = (ChunkDecoder::new(), Vec::new(), Vec::new());
        for &b in raw {
            buf.push(b);
            d.pull(&mut buf, &mut out).unwrap();
        }
        assert!(d.is_done() && out == b"abc" && buf.is_empty());
    }

    #[test]
    fn broken_framing_is_refused() {
        for bad in [&b"zz\r\n"[..], b"3\r\nabcXX", b"\r\n", b"-3\r\n", b"fffffffffffffffffffff\r\n", b"0x3\r\nabc\r\n"] {
            let r = ChunkDecoder::new().pull(&mut bad.to_vec(), &mut Vec::new());
            assert!(r.is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        let mut long = vec![b'1'; MAX_CHUNK_LINE + 1];
        assert_eq!(ChunkDecoder::new().pull(&mut long, &mut Vec::new()), Err(ChunkError::TooLarge("a chunk-size line")));
    }
}
