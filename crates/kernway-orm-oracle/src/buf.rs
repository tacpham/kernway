//! TTC value encodings: the writer for requests and a synchronous reader over a
//! complete (or partial) response.
//!
//! Integers on the wire are mostly "universal" (`ub2`/`ub4`/`ub8`): a length
//! byte followed by that many big-endian bytes. Byte strings carry a length
//! byte (`0`/`0xFF` = NULL), or `0xFE` then `ub4`-length chunks ending in a
//! `ub4` 0.

/// Longest byte string sent with a one-byte length.
pub(crate) const MAX_SHORT_LENGTH: usize = 252;
/// Marks a chunked long byte string.
pub(crate) const LONG_LENGTH_INDICATOR: u8 = 0xFE;
/// A NULL value's length byte.
pub(crate) const NULL_LENGTH_INDICATOR: u8 = 0xFF;
/// Largest chunk of a long byte string.
pub(crate) const CHUNK_SIZE: usize = 32767;

/// Builds a request payload.
#[derive(Default)]
pub(crate) struct Writer {
    pub(crate) buf: Vec<u8>,
}

impl Writer {
    pub(crate) fn new() -> Self {
        Self { buf: Vec::with_capacity(256) }
    }
    pub(crate) fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub(crate) fn u16be(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub(crate) fn u16le(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub(crate) fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }
    /// Universal unsigned integer (`ub2`/`ub4`/`ub8` share the encoding).
    pub(crate) fn ub(&mut self, v: u64) -> &mut Self {
        if v == 0 {
            self.buf.push(0);
        } else if v <= 0xFF {
            self.buf.extend_from_slice(&[1, v as u8]);
        } else if v <= 0xFFFF {
            self.buf.push(2);
            self.buf.extend_from_slice(&(v as u16).to_be_bytes());
        } else if v <= 0xFFFF_FFFF {
            self.buf.push(4);
            self.buf.extend_from_slice(&(v as u32).to_be_bytes());
        } else {
            self.buf.push(8);
            self.buf.extend_from_slice(&v.to_be_bytes());
        }
        self
    }
    /// Length-prefixed bytes (short form, or chunked long form).
    pub(crate) fn bytes_wl(&mut self, b: &[u8]) -> &mut Self {
        if b.len() <= MAX_SHORT_LENGTH {
            self.buf.push(b.len() as u8);
            self.buf.extend_from_slice(b);
        } else {
            self.buf.push(LONG_LENGTH_INDICATOR);
            for chunk in b.chunks(CHUNK_SIZE) {
                self.ub(chunk.len() as u64);
                self.buf.extend_from_slice(chunk);
            }
            self.ub(0);
        }
        self
    }
    /// `ub4` total length, then (if non-empty) the length-prefixed bytes.
    pub(crate) fn bytes_2l(&mut self, b: &[u8]) -> &mut Self {
        self.ub(b.len() as u64);
        if !b.is_empty() {
            self.bytes_wl(b);
        }
        self
    }
}

/// Why a parse stopped.
#[derive(Debug)]
pub(crate) enum ParseError {
    /// The bytes ran out: more packets of the response are needed.
    Incomplete,
    /// The bytes do not make sense.
    Invalid(String),
}

pub(crate) type PResult<T> = Result<T, ParseError>;

fn invalid<T>(msg: impl Into<String>) -> PResult<T> {
    Err(ParseError::Invalid(msg.into()))
}

/// A cursor over response bytes.
pub(crate) struct Reader<'a> {
    b: &'a [u8],
    pub(crate) pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }
    pub(crate) fn remaining(&self) -> usize {
        self.b.len() - self.pos
    }
    pub(crate) fn take(&mut self, n: usize) -> PResult<&'a [u8]> {
        if self.remaining() < n {
            return Err(ParseError::Incomplete);
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub(crate) fn skip(&mut self, n: usize) -> PResult<()> {
        self.take(n).map(|_| ())
    }
    /// The next byte, not consumed.
    pub(crate) fn peek(&self) -> PResult<u8> {
        self.b.get(self.pos).copied().ok_or(ParseError::Incomplete)
    }
    pub(crate) fn u8(&mut self) -> PResult<u8> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn u16be(&mut self) -> PResult<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    pub(crate) fn u16le(&mut self) -> PResult<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    /// Universal unsigned integer of at most `max` bytes.
    fn ub_max(&mut self, max: usize) -> PResult<u64> {
        let len = self.u8()?;
        if len & 0x80 != 0 {
            return invalid("unexpected negative universal integer");
        }
        let len = len as usize;
        if len > max {
            return invalid(format!("universal integer too long ({len} bytes)"));
        }
        Ok(self.take(len)?.iter().fold(0u64, |acc, &b| acc << 8 | u64::from(b)))
    }
    pub(crate) fn ub2(&mut self) -> PResult<u16> {
        self.ub_max(2).map(|v| v as u16)
    }
    pub(crate) fn ub4(&mut self) -> PResult<u32> {
        self.ub_max(4).map(|v| v as u32)
    }
    pub(crate) fn ub8(&mut self) -> PResult<u64> {
        self.ub_max(8)
    }
    /// Universal signed integer.
    pub(crate) fn sb(&mut self) -> PResult<i64> {
        let len = self.u8()?;
        let neg = len & 0x80 != 0;
        let len = (len & 0x7F) as usize;
        if len > 8 {
            return invalid(format!("universal integer too long ({len} bytes)"));
        }
        let v = self.take(len)?.iter().fold(0u64, |acc, &b| acc << 8 | u64::from(b)) as i64;
        Ok(if neg { -v } else { v })
    }
    /// Skip a universal integer.
    pub(crate) fn skip_ub(&mut self) -> PResult<()> {
        let len = (self.u8()? & 0x7F) as usize;
        self.skip(len)
    }
    /// Length-prefixed bytes; `None` for NULL (length 0 or 0xFF).
    pub(crate) fn bytes(&mut self) -> PResult<Option<Vec<u8>>> {
        match self.u8()? {
            0 | NULL_LENGTH_INDICATOR => Ok(None),
            LONG_LENGTH_INDICATOR => {
                let mut out = Vec::new();
                loop {
                    let n = self.ub4()? as usize;
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(self.take(n)?);
                }
                Ok(Some(out))
            }
            n => Ok(Some(self.take(n as usize)?.to_vec())),
        }
    }
    /// Skip length-prefixed bytes (0xFF treated as NULL).
    pub(crate) fn skip_bytes(&mut self) -> PResult<()> {
        match self.u8()? {
            0 | NULL_LENGTH_INDICATOR => Ok(()),
            LONG_LENGTH_INDICATOR => loop {
                let n = self.ub4()? as usize;
                if n == 0 {
                    return Ok(());
                }
                self.skip(n)?;
            },
            n => self.skip(n as usize),
        }
    }
    /// UTF-8 string with a one-byte (or chunked) length.
    pub(crate) fn str(&mut self) -> PResult<Option<String>> {
        Ok(self.bytes()?.map(|b| String::from_utf8_lossy(&b).into_owned()))
    }
    /// `ub4` length, then (if non-zero) a length-prefixed UTF-8 string.
    pub(crate) fn str_2l(&mut self) -> PResult<Option<String>> {
        if self.ub4()? > 0 {
            self.str()
        } else {
            Ok(None)
        }
    }
    /// `ub4` length, then (if non-zero) length-prefixed bytes to skip.
    pub(crate) fn skip_bytes_2l(&mut self) -> PResult<()> {
        if self.ub4()? > 0 {
            self.skip_bytes()
        } else {
            Ok(())
        }
    }
    /// Bytes up to and including the next NUL.
    pub(crate) fn null_terminated(&mut self) -> PResult<&'a [u8]> {
        let rest = &self.b[self.pos..];
        match rest.iter().position(|&c| c == 0) {
            Some(i) => self.take(i + 1),
            None => Err(ParseError::Incomplete),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn universal_integers_round_trip() {
        for v in [0u64, 1, 0xFF, 0x100, 0xFFFF, 0x1_0000, 0xFFFF_FFFF, 0x1_0000_0000] {
            let mut w = Writer::new();
            w.ub(v);
            assert_eq!(Reader::new(&w.buf).ub8().unwrap(), v, "{v}");
        }
        // Signed: the length byte's 0x80 bit is the sign.
        assert_eq!(Reader::new(&[0x81, 5]).sb().unwrap(), -5);
        assert_eq!(Reader::new(&[0x02, 1, 0x2C]).sb().unwrap(), 300);
        // Readers accept odd lengths writers never emit.
        assert_eq!(Reader::new(&[3, 1, 2, 3]).ub4().unwrap(), 0x010203);
    }

    #[test]
    fn byte_strings_short_long_and_null() {
        let long: Vec<u8> = (0..40_000u32).map(|i| i as u8).collect();
        for v in [b"".to_vec(), b"ID".to_vec(), vec![7u8; 252], vec![9u8; 253], long] {
            let mut w = Writer::new();
            w.bytes_wl(&v);
            let got = Reader::new(&w.buf).bytes().unwrap();
            assert_eq!(got, if v.is_empty() { None } else { Some(v.clone()) });
            let mut r = Reader::new(&w.buf);
            r.skip_bytes().unwrap();
            assert_eq!(r.remaining(), 0);
        }
        assert_eq!(Reader::new(&[0xFF]).bytes().unwrap(), None);
        // "with two lengths": the spec's example for "ID".
        let mut w = Writer::new();
        w.bytes_2l(b"ID");
        assert_eq!(w.buf, [0x01, 0x02, 0x02, b'I', b'D']);
        assert_eq!(Reader::new(&w.buf).str_2l().unwrap().as_deref(), Some("ID"));
    }

    #[test]
    fn running_out_of_bytes_is_incomplete_not_invalid() {
        assert!(matches!(Reader::new(&[4, 1, 2]).ub4(), Err(ParseError::Incomplete)));
        assert!(matches!(Reader::new(&[5, b'a']).bytes(), Err(ParseError::Incomplete)));
        assert!(matches!(Reader::new(&[0x81, 1]).ub4(), Err(ParseError::Invalid(_))));
    }
}
