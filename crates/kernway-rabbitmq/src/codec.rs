//! AMQP 0-9-1 wire format: frames, method arguments, field tables and the
//! basic content properties (RabbitMQ's dialect of the field types).

use kernway_messaging::MsgError;

/// Frame types.
pub(crate) const FRAME_METHOD: u8 = 1;
pub(crate) const FRAME_HEADER: u8 = 2;
pub(crate) const FRAME_BODY: u8 = 3;
pub(crate) const FRAME_HEARTBEAT: u8 = 8;
pub(crate) const FRAME_END: u8 = 0xCE;

/// The protocol header the client opens with.
pub(crate) const PROTOCOL_HEADER: &[u8] = b"AMQP\x00\x00\x09\x01";

fn short(what: &str) -> MsgError {
    MsgError::Protocol(format!("AMQP: truncated {what}"))
}

/// A field value (RabbitMQ's table types).
#[derive(Debug, Clone, PartialEq)]
pub enum Field {
    /// `t`
    Bool(bool),
    /// `b`
    I8(i8),
    /// `B`
    U8(u8),
    /// `s`
    I16(i16),
    /// `u`
    U16(u16),
    /// `I`
    I32(i32),
    /// `i`
    U32(u32),
    /// `l`
    I64(i64),
    /// `f`
    F32(f32),
    /// `d`
    F64(f64),
    /// `D`: scale, unscaled value.
    Decimal(u8, u32),
    /// `S` (binary-safe).
    Str(Vec<u8>),
    /// `x`
    Bytes(Vec<u8>),
    /// `A`
    Array(Vec<Field>),
    /// `T`: seconds since the epoch.
    Timestamp(u64),
    /// `F`
    Table(Table),
    /// `V`
    Void,
}

impl Field {
    /// A UTF-8 / binary string value.
    pub fn str(s: impl AsRef<[u8]>) -> Field {
        Field::Str(s.as_ref().to_vec())
    }

    /// As text (strings and byte arrays; numbers formatted).
    pub fn as_text(&self) -> Option<String> {
        Some(match self {
            Field::Str(b) | Field::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            Field::Bool(v) => v.to_string(),
            Field::I8(v) => v.to_string(),
            Field::U8(v) => v.to_string(),
            Field::I16(v) => v.to_string(),
            Field::U16(v) => v.to_string(),
            Field::I32(v) => v.to_string(),
            Field::U32(v) => v.to_string(),
            Field::I64(v) => v.to_string(),
            Field::Timestamp(v) => v.to_string(),
            Field::F32(v) => v.to_string(),
            Field::F64(v) => v.to_string(),
            _ => return None,
        })
    }

    /// As an integer.
    pub fn as_i64(&self) -> Option<i64> {
        Some(match *self {
            Field::I8(v) => v.into(),
            Field::U8(v) => v.into(),
            Field::I16(v) => v.into(),
            Field::U16(v) => v.into(),
            Field::I32(v) => v.into(),
            Field::U32(v) => v.into(),
            Field::I64(v) => v,
            Field::Timestamp(v) => v as i64,
            _ => return None,
        })
    }
}

/// A field table, in order.
pub type Table = Vec<(String, Field)>;

/// Writes AMQP values.
#[derive(Default)]
pub(crate) struct W {
    pub(crate) b: Vec<u8>,
}

impl W {
    pub(crate) fn u8(&mut self, v: u8) -> &mut Self {
        self.b.push(v);
        self
    }
    pub(crate) fn u16(&mut self, v: u16) -> &mut Self {
        self.b.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub(crate) fn u32(&mut self, v: u32) -> &mut Self {
        self.b.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub(crate) fn u64(&mut self, v: u64) -> &mut Self {
        self.b.extend_from_slice(&v.to_be_bytes());
        self
    }
    /// A short string (≤ 255 bytes).
    pub(crate) fn short(&mut self, s: &str) -> Result<&mut Self, MsgError> {
        let n = u8::try_from(s.len()).map_err(|_| MsgError::Config(format!("AMQP name longer than 255 bytes: {s:?}")))?;
        self.b.push(n);
        self.b.extend_from_slice(s.as_bytes());
        Ok(self)
    }
    pub(crate) fn long(&mut self, s: &[u8]) -> &mut Self {
        self.u32(s.len() as u32);
        self.b.extend_from_slice(s);
        self
    }
    pub(crate) fn table(&mut self, t: &Table) -> Result<&mut Self, MsgError> {
        let mut inner = W::default();
        for (k, v) in t {
            inner.short(k)?;
            inner.field(v)?;
        }
        self.long(&inner.b);
        Ok(self)
    }
    fn field(&mut self, f: &Field) -> Result<(), MsgError> {
        match f {
            Field::Bool(v) => self.u8(b't').u8(u8::from(*v)),
            Field::I8(v) => self.u8(b'b').u8(*v as u8),
            Field::U8(v) => self.u8(b'B').u8(*v),
            Field::I16(v) => self.u8(b's').u16(*v as u16),
            Field::U16(v) => self.u8(b'u').u16(*v),
            Field::I32(v) => self.u8(b'I').u32(*v as u32),
            Field::U32(v) => self.u8(b'i').u32(*v),
            Field::I64(v) => self.u8(b'l').u64(*v as u64),
            Field::F32(v) => self.u8(b'f').u32(v.to_bits()),
            Field::F64(v) => self.u8(b'd').u64(v.to_bits()),
            Field::Decimal(s, v) => self.u8(b'D').u8(*s).u32(*v),
            Field::Str(s) => self.u8(b'S').long(s),
            Field::Bytes(s) => self.u8(b'x').long(s),
            Field::Array(a) => {
                let mut inner = W::default();
                for x in a {
                    inner.field(x)?;
                }
                self.u8(b'A').long(&inner.b)
            }
            Field::Timestamp(v) => self.u8(b'T').u64(*v),
            Field::Table(t) => {
                self.u8(b'F');
                self.table(t)?
            }
            Field::Void => self.u8(b'V'),
        };
        Ok(())
    }
}

/// Reads AMQP values.
pub(crate) struct R<'a> {
    pub(crate) b: &'a [u8],
    pub(crate) pos: usize,
}

impl<'a> R<'a> {
    pub(crate) fn new(b: &'a [u8]) -> Self {
        R { b, pos: 0 }
    }
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], MsgError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.b.len()).ok_or_else(|| short(what))?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    pub(crate) fn u8(&mut self) -> Result<u8, MsgError> {
        Ok(self.take(1, "octet")?[0])
    }
    pub(crate) fn u16(&mut self) -> Result<u16, MsgError> {
        Ok(u16::from_be_bytes(self.take(2, "short")?.try_into().unwrap_or_default()))
    }
    pub(crate) fn u32(&mut self) -> Result<u32, MsgError> {
        Ok(u32::from_be_bytes(self.take(4, "long")?.try_into().unwrap_or_default()))
    }
    pub(crate) fn u64(&mut self) -> Result<u64, MsgError> {
        Ok(u64::from_be_bytes(self.take(8, "longlong")?.try_into().unwrap_or_default()))
    }
    pub(crate) fn short(&mut self) -> Result<String, MsgError> {
        let n = self.u8()? as usize;
        Ok(String::from_utf8_lossy(self.take(n, "short string")?).into_owned())
    }
    pub(crate) fn long(&mut self) -> Result<Vec<u8>, MsgError> {
        let n = self.u32()? as usize;
        Ok(self.take(n, "long string")?.to_vec())
    }
    pub(crate) fn table(&mut self) -> Result<Table, MsgError> {
        self.table_depth(0)
    }
    fn table_depth(&mut self, depth: usize) -> Result<Table, MsgError> {
        if depth > 32 {
            return Err(MsgError::Protocol("AMQP: field tables nested too deep".into()));
        }
        let n = self.u32()? as usize;
        let mut inner = R::new(self.take(n, "table")?);
        let mut t = Vec::new();
        while inner.pos < inner.b.len() {
            let k = inner.short()?;
            let v = inner.field(depth)?;
            t.push((k, v));
        }
        Ok(t)
    }
    fn field(&mut self, depth: usize) -> Result<Field, MsgError> {
        Ok(match self.u8()? {
            b't' => Field::Bool(self.u8()? != 0),
            b'b' => Field::I8(self.u8()? as i8),
            b'B' => Field::U8(self.u8()?),
            b's' => Field::I16(self.u16()? as i16),
            b'u' => Field::U16(self.u16()?),
            b'I' => Field::I32(self.u32()? as i32),
            b'i' => Field::U32(self.u32()?),
            b'l' => Field::I64(self.u64()? as i64),
            b'f' => Field::F32(f32::from_bits(self.u32()?)),
            b'd' => Field::F64(f64::from_bits(self.u64()?)),
            b'D' => Field::Decimal(self.u8()?, self.u32()?),
            b'S' => Field::Str(self.long()?),
            b'x' => Field::Bytes(self.long()?),
            b'A' => {
                let n = self.u32()? as usize;
                let mut inner = R::new(self.take(n, "array")?);
                let mut a = Vec::new();
                while inner.pos < inner.b.len() {
                    a.push(inner.field(depth + 1)?);
                }
                Field::Array(a)
            }
            b'T' => Field::Timestamp(self.u64()?),
            b'F' => Field::Table(self.table_depth(depth + 1)?),
            b'V' => Field::Void,
            t => return Err(MsgError::Protocol(format!("AMQP: unknown field type {:?}", t as char))),
        })
    }
}

/// The basic class's content properties.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Properties {
    /// MIME type.
    pub content_type: Option<String>,
    /// Encoding.
    pub content_encoding: Option<String>,
    /// Application headers.
    pub headers: Option<Table>,
    /// 1 transient, 2 persistent.
    pub delivery_mode: Option<u8>,
    /// 0–9.
    pub priority: Option<u8>,
    /// Correlation id.
    pub correlation_id: Option<String>,
    /// Reply queue.
    pub reply_to: Option<String>,
    /// Per-message TTL (milliseconds, as text).
    pub expiration: Option<String>,
    /// Message id.
    pub message_id: Option<String>,
    /// Timestamp (seconds).
    pub timestamp: Option<u64>,
    /// Type name.
    pub kind: Option<String>,
    /// User id (checked by RabbitMQ against the connection's user).
    pub user_id: Option<String>,
    /// Application id.
    pub app_id: Option<String>,
}

impl Properties {
    pub(crate) fn encode(&self, w: &mut W) -> Result<(), MsgError> {
        let mut flags = 0u16;
        let mut body = W::default();
        macro_rules! s {
            ($f:expr, $bit:expr) => {
                if let Some(v) = &$f {
                    flags |= 1 << $bit;
                    body.short(v)?;
                }
            };
        }
        s!(self.content_type, 15);
        s!(self.content_encoding, 14);
        if let Some(h) = &self.headers {
            flags |= 1 << 13;
            body.table(h)?;
        }
        if let Some(v) = self.delivery_mode {
            flags |= 1 << 12;
            body.u8(v);
        }
        if let Some(v) = self.priority {
            flags |= 1 << 11;
            body.u8(v);
        }
        s!(self.correlation_id, 10);
        s!(self.reply_to, 9);
        s!(self.expiration, 8);
        s!(self.message_id, 7);
        if let Some(v) = self.timestamp {
            flags |= 1 << 6;
            body.u64(v);
        }
        s!(self.kind, 5);
        s!(self.user_id, 4);
        s!(self.app_id, 3);
        w.u16(flags);
        w.b.extend_from_slice(&body.b);
        Ok(())
    }

    pub(crate) fn decode(r: &mut R<'_>) -> Result<Self, MsgError> {
        let mut flags = r.u16()?;
        // Continuation bit: more flag words follow (none are defined; skip them).
        let mut extra = flags & 1;
        while extra != 0 {
            extra = r.u16()? & 1;
        }
        flags &= !1;
        let has = |bit: u16| flags & (1 << bit) != 0;
        let mut p = Properties::default();
        if has(15) {
            p.content_type = Some(r.short()?);
        }
        if has(14) {
            p.content_encoding = Some(r.short()?);
        }
        if has(13) {
            p.headers = Some(r.table()?);
        }
        if has(12) {
            p.delivery_mode = Some(r.u8()?);
        }
        if has(11) {
            p.priority = Some(r.u8()?);
        }
        if has(10) {
            p.correlation_id = Some(r.short()?);
        }
        if has(9) {
            p.reply_to = Some(r.short()?);
        }
        if has(8) {
            p.expiration = Some(r.short()?);
        }
        if has(7) {
            p.message_id = Some(r.short()?);
        }
        if has(6) {
            p.timestamp = Some(r.u64()?);
        }
        if has(5) {
            p.kind = Some(r.short()?);
        }
        if has(4) {
            p.user_id = Some(r.short()?);
        }
        if has(3) {
            p.app_id = Some(r.short()?);
        }
        Ok(p)
    }
}

/// One frame: `type channel size payload 0xCE`.
pub(crate) fn frame(kind: u8, channel: u16, payload: &[u8], out: &mut Vec<u8>) {
    out.push(kind);
    out.extend_from_slice(&channel.to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out.push(FRAME_END);
}

/// A method frame's payload: class, method, arguments.
pub(crate) fn method(class: u16, id: u16, args: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + args.len());
    b.extend_from_slice(&class.to_be_bytes());
    b.extend_from_slice(&id.to_be_bytes());
    b.extend_from_slice(args);
    b
}

/// The frames of one published message: the method, the content header and
/// the body split to fit `frame_max`.
pub(crate) fn content_frames(channel: u16, method_payload: &[u8], props: &Properties, body: &[u8], frame_max: u32, out: &mut Vec<u8>) -> Result<(), MsgError> {
    frame(FRAME_METHOD, channel, method_payload, out);
    let mut h = W::default();
    h.u16(60).u16(0).u64(body.len() as u64);
    props.encode(&mut h)?;
    frame(FRAME_HEADER, channel, &h.b, out);
    // frame_max covers the 8 bytes of frame overhead.
    let chunk = (frame_max.max(4096) - 8) as usize;
    for part in body.chunks(chunk) {
        frame(FRAME_BODY, channel, part, out);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_and_properties_round_trip() {
        let t: Table = vec![
            ("bool".into(), Field::Bool(true)),
            ("i8".into(), Field::I8(-5)),
            ("u8".into(), Field::U8(250)),
            ("i16".into(), Field::I16(-300)),
            ("i32".into(), Field::I32(-70000)),
            ("u32".into(), Field::U32(4_000_000_000)),
            ("i64".into(), Field::I64(i64::MIN)),
            ("f32".into(), Field::F32(1.5)),
            ("f64".into(), Field::F64(-2.25)),
            ("dec".into(), Field::Decimal(2, 12345)),
            ("str".into(), Field::str("Việt ✓\0bin")),
            ("bytes".into(), Field::Bytes(vec![0, 255])),
            ("arr".into(), Field::Array(vec![Field::I32(1), Field::str("x"), Field::Array(vec![])])),
            ("ts".into(), Field::Timestamp(1_700_000_000)),
            ("tbl".into(), Field::Table(vec![("k".into(), Field::Void)])),
        ];
        let mut w = W::default();
        w.table(&t).unwrap();
        let mut r = R::new(&w.b);
        assert_eq!(r.table().unwrap(), t);
        assert_eq!(r.pos, w.b.len());

        let p = Properties {
            content_type: Some("application/json".into()),
            headers: Some(t),
            delivery_mode: Some(2),
            priority: Some(9),
            correlation_id: Some("c".into()),
            message_id: Some("m".into()),
            timestamp: Some(5),
            app_id: Some("kw".into()),
            ..Properties::default()
        };
        let mut w = W::default();
        p.encode(&mut w).unwrap();
        assert_eq!(Properties::decode(&mut R::new(&w.b)).unwrap(), p);
        let mut w = W::default();
        Properties::default().encode(&mut w).unwrap();
        assert_eq!(w.b, [0, 0], "no properties: just the flag word");
    }

    #[test]
    fn hostile_input_is_an_error() {
        for bad in [&b"\x00\x00\x00\x09\x01kZ"[..], b"\x00\x00\x00\xff", b"\x00\x00\x00\x03\x01k"] {
            assert!(R::new(bad).table().is_err(), "{bad:?}");
        }
        // 40 nested tables.
        let mut f = Field::Void;
        for _ in 0..40 {
            f = Field::Table(vec![("n".into(), f)]);
        }
        let mut w = W::default();
        w.table(&vec![("deep".into(), f)]).unwrap();
        assert!(R::new(&w.b).table().is_err());
        let mut w = W::default();
        assert!(w.short(&"x".repeat(256)).is_err());
    }

    #[test]
    fn large_bodies_are_split_to_frame_max() {
        let mut out = Vec::new();
        let body = vec![1u8; 300_000];
        content_frames(1, &method(60, 40, &[]), &Properties::default(), &body, 131_072, &mut out).unwrap();
        // method + header + 3 body frames (131064 + 131064 + 37872).
        let mut r = R::new(&out);
        let mut kinds = Vec::new();
        while r.pos < out.len() {
            let k = r.u8().unwrap();
            r.u16().unwrap();
            let n = r.u32().unwrap() as usize;
            assert!(n + 8 <= 131_072);
            r.pos += n;
            assert_eq!(r.u8().unwrap(), FRAME_END);
            kinds.push(k);
        }
        assert_eq!(kinds, [FRAME_METHOD, FRAME_HEADER, FRAME_BODY, FRAME_BODY, FRAME_BODY]);
    }
}
