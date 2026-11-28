//! SSH data types (RFC 4251 §5): `byte`, `boolean`, `uint32`, `uint64`, `string`,
//! `mpint` and `name-list`, read from and written to byte buffers.

use crate::SshError;

/// Reads SSH data types from a byte slice, front to back.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
}

fn short() -> SshError {
    SshError::Protocol("a message ended early".into())
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8], SshError> {
        if self.buf.len() < n {
            return Err(short());
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, SshError> {
        Ok(self.bytes(1)?[0])
    }

    pub(crate) fn bool(&mut self) -> Result<bool, SshError> {
        Ok(self.u8()? != 0)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, SshError> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, SshError> {
        let b = self.bytes(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }

    /// A `string`: arbitrary bytes behind a `uint32` length.
    pub(crate) fn string(&mut self) -> Result<&'a [u8], SshError> {
        let n = self.u32()? as usize;
        self.bytes(n)
    }

    /// A `string` that must be UTF-8 (lossily decoded otherwise).
    pub(crate) fn utf8(&mut self) -> Result<String, SshError> {
        Ok(String::from_utf8_lossy(self.string()?).into_owned())
    }

    /// A `name-list`: comma-separated ASCII names.
    pub(crate) fn name_list(&mut self) -> Result<Vec<String>, SshError> {
        let s = self.string()?;
        let s = std::str::from_utf8(s).map_err(|_| SshError::Protocol("a name-list that is not ASCII".into()))?;
        Ok(if s.is_empty() { Vec::new() } else { s.split(',').map(str::to_owned).collect() })
    }

    /// An `mpint` as its magnitude: big-endian bytes without the sign byte.
    /// Negative values are refused (none appear in SSH key material).
    pub(crate) fn mpint(&mut self) -> Result<&'a [u8], SshError> {
        let b = self.string()?;
        if b.first().is_some_and(|&x| x & 0x80 != 0) {
            return Err(SshError::Protocol("a negative mpint".into()));
        }
        let start = b.iter().position(|&x| x != 0).unwrap_or(b.len());
        Ok(&b[start..])
    }
}

/// Writes SSH data types into a growing buffer.
#[derive(Default)]
pub(crate) struct Writer {
    pub(crate) buf: Vec<u8>,
}

impl Writer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A writer whose buffer starts with a message number.
    pub(crate) fn msg(number: u8) -> Self {
        Self { buf: vec![number] }
    }

    pub(crate) fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    pub(crate) fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(u8::from(v))
    }

    pub(crate) fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub(crate) fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub(crate) fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }

    pub(crate) fn string(&mut self, b: impl AsRef<[u8]>) -> &mut Self {
        let b = b.as_ref();
        self.u32(u32::try_from(b.len()).expect("an SSH string under 4 GiB"));
        self.raw(b)
    }

    pub(crate) fn name_list(&mut self, names: &[&str]) -> &mut Self {
        self.string(names.join(","))
    }

    /// An unsigned big-endian magnitude as an `mpint`: leading zeros dropped, a zero
    /// byte added when the top bit is set.
    pub(crate) fn mpint(&mut self, magnitude: &[u8]) -> &mut Self {
        let start = magnitude.iter().position(|&x| x != 0).unwrap_or(magnitude.len());
        let m = &magnitude[start..];
        if m.first().is_some_and(|&x| x & 0x80 != 0) {
            self.u32(u32::try_from(m.len() + 1).expect("small"));
            self.u8(0);
            self.raw(m)
        } else {
            self.string(m)
        }
    }

    pub(crate) fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpints_follow_rfc_4251_examples() {
        // RFC 4251 §5: 0, 0x9a378f9b2e332a7, 0x80, -1 (refused), -0xdeadbeef (refused).
        let enc = |m: &[u8]| {
            let mut w = Writer::new();
            w.mpint(m);
            w.into_inner()
        };
        assert_eq!(enc(&[]), [0, 0, 0, 0]);
        assert_eq!(enc(&[0, 0]), [0, 0, 0, 0]);
        assert_eq!(enc(&[0x09, 0xa3, 0x78, 0xf9, 0xb2, 0xe3, 0x32, 0xa7]), [0, 0, 0, 8, 0x09, 0xa3, 0x78, 0xf9, 0xb2, 0xe3, 0x32, 0xa7]);
        assert_eq!(enc(&[0x80]), [0, 0, 0, 2, 0, 0x80]);
        assert!(Reader::new(&[0, 0, 0, 1, 0xff]).mpint().is_err(), "-1");
        assert_eq!(Reader::new(&[0, 0, 0, 2, 0, 0x80]).mpint().unwrap(), [0x80]);
    }

    #[test]
    fn strings_name_lists_and_short_input() {
        let mut w = Writer::msg(20);
        w.string("abc").name_list(&["zlib", "none"]).name_list(&[]).bool(true).u64(1 << 40);
        let b = w.into_inner();
        let mut r = Reader::new(&b[1..]);
        assert_eq!(r.string().unwrap(), b"abc");
        assert_eq!(r.name_list().unwrap(), ["zlib", "none"]);
        assert!(r.name_list().unwrap().is_empty());
        assert!(r.bool().unwrap());
        assert_eq!(r.u64().unwrap(), 1 << 40);
        assert!(r.is_empty());
        assert!(Reader::new(&[0, 0, 0, 9, 1]).string().is_err(), "a length past the end");
    }
}
