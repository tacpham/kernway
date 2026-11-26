//! Oracle's binary JSON (OSON) and VECTOR images, decoded to JSON text.
//! A JSON column arrives as OSON; numbers keep their exact decimal text,
//! dates / timestamps become ISO-8601 strings, binary values hex strings.
//! Ported from python-oracledb's `OsonDecoder` / `VectorDecoder`.

use crate::value::{decode_date, decode_double, decode_number};

type R<T> = Result<T, String>;

struct Oson<'a> {
    data: &'a [u8],
    pos: usize,
    relative_offsets: bool,
    field_id_length: usize,
    field_names: Vec<String>,
    tree_seg_pos: usize,
}

fn short() -> String {
    "truncated OSON".into()
}

impl<'a> Oson<'a> {
    fn raw(&mut self, n: usize) -> R<&'a [u8]> {
        let b = self.data.get(self.pos..self.pos + n).ok_or_else(short)?;
        self.pos += n;
        Ok(b)
    }
    fn u8(&mut self) -> R<u8> {
        Ok(self.raw(1)?[0])
    }
    fn u16(&mut self) -> R<u16> {
        let b = self.raw(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> R<u32> {
        let b = self.raw(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn skip_to(&mut self, pos: usize) -> R<()> {
        if pos > self.data.len() {
            return Err(short());
        }
        self.pos = pos;
        Ok(())
    }

    fn decode(data: &'a [u8]) -> R<String> {
        let mut d = Oson { data, pos: 0, relative_offsets: false, field_id_length: 1, field_names: Vec::new(), tree_seg_pos: 0 };
        if d.raw(3)? != [0xFF, b'J', b'Z'] {
            return Err("not an OSON image".into());
        }
        let version = d.u8()?;
        if version != 1 && version != 3 {
            return Err(format!("OSON version {version} not supported"));
        }
        let flags = d.u16()?;
        d.relative_offsets = flags & 0x01 != 0;
        let mut out = String::new();
        if flags & 0x10 != 0 {
            // A scalar document.
            d.raw(if flags & 0x1000 != 0 { 4 } else { 2 })?;
            d.node(&mut out)?;
            return Ok(out);
        }
        let num_short = if flags & 0x08 != 0 {
            d.field_id_length = 4;
            d.u32()?
        } else if flags & 0x0400 != 0 {
            d.field_id_length = 2;
            u32::from(d.u16()?)
        } else {
            d.field_id_length = 1;
            u32::from(d.u8()?)
        };
        let (short_offsets_size, short_seg_size) =
            if flags & 0x0800 != 0 { (4, d.u32()?) } else { (2, u32::from(d.u16()?)) };
        let (mut long_offsets_size, mut num_long, mut long_seg_size) = (0, 0, 0);
        if version == 3 {
            let secondary = d.u16()?;
            long_offsets_size = if secondary & 0x0100 != 0 { 2 } else { 4 };
            num_long = d.u32()?;
            long_seg_size = d.u32()?;
        }
        let _tree_seg_size = if flags & 0x1000 != 0 { d.u32()? } else { u32::from(d.u16()?) };
        let _tiny_nodes = d.u16()?;
        if num_short > 0 {
            d.field_names(num_short, 1, short_offsets_size, short_seg_size, false)?;
        }
        if num_long > 0 {
            d.field_names(num_long, 2, long_offsets_size, long_seg_size, true)?;
        }
        d.tree_seg_pos = d.pos;
        d.node(&mut out)?;
        Ok(out)
    }

    /// Read a field-name segment: hash ids, offsets, names (1-byte length
    /// prefix for short names, 2-byte for long ones).
    fn field_names(&mut self, n: u32, hash_size: usize, offsets_size: usize, seg_size: u32, long: bool) -> R<()> {
        let n = n as usize;
        self.raw(n * hash_size)?;
        let offsets_pos = self.pos;
        self.raw(n * offsets_size)?;
        let seg = self.raw(seg_size as usize)?;
        let end = self.pos;
        self.skip_to(offsets_pos)?;
        for _ in 0..n {
            let off = if offsets_size == 2 { usize::from(self.u16()?) } else { self.u32()? as usize };
            let (len, start) = if long {
                let b = seg.get(off..off + 2).ok_or_else(short)?;
                (usize::from(u16::from_be_bytes([b[0], b[1]])), off + 2)
            } else {
                (usize::from(*seg.get(off).ok_or_else(short)?), off + 1)
            };
            let name = seg.get(start..start + len).ok_or_else(short)?;
            self.field_names.push(String::from_utf8_lossy(name).into_owned());
        }
        self.skip_to(end)
    }

    fn num_children(&mut self, node_type: u8) -> R<(u32, bool)> {
        Ok(match node_type & 0x18 {
            0x00 => (u32::from(self.u8()?), false),
            0x08 => (u32::from(self.u16()?), false),
            0x10 => (self.u32()?, false),
            _ => (0, true),
        })
    }

    fn offset(&mut self, node_type: u8) -> R<usize> {
        Ok(if node_type & 0x20 != 0 { self.u32()? as usize } else { usize::from(self.u16()?) })
    }

    fn container(&mut self, node_type: u8, out: &mut String) -> R<()> {
        let is_object = node_type & 0x40 == 0;
        let container_offset = self.pos - self.tree_seg_pos - 1;
        let (mut n, shared) = self.num_children(node_type)?;
        let (mut field_ids_pos, mut offsets_pos);
        if shared {
            let off = self.offset(node_type)?;
            offsets_pos = self.pos;
            self.skip_to(self.tree_seg_pos + off)?;
            let t = self.u8()?;
            n = self.num_children(t)?.0;
            field_ids_pos = self.pos;
        } else if is_object {
            field_ids_pos = self.pos;
            offsets_pos = self.pos + self.field_id_length * n as usize;
        } else {
            field_ids_pos = 0;
            offsets_pos = self.pos;
        }
        out.push(if is_object { '{' } else { '[' });
        for i in 0..n {
            if i > 0 {
                out.push(',');
            }
            if is_object {
                self.skip_to(field_ids_pos)?;
                let id = match self.field_id_length {
                    1 => usize::from(self.u8()?),
                    2 => usize::from(self.u16()?),
                    _ => self.u32()? as usize,
                };
                field_ids_pos = self.pos;
                let name = self.field_names.get(id.wrapping_sub(1)).ok_or("bad OSON field id")?.clone();
                json_string(&name, out);
                out.push(':');
            }
            self.skip_to(offsets_pos)?;
            let mut off = self.offset(node_type)?;
            if self.relative_offsets {
                off += container_offset;
            }
            offsets_pos = self.pos;
            self.skip_to(self.tree_seg_pos + off)?;
            self.node(out)?;
        }
        out.push(if is_object { '}' } else { ']' });
        Ok(())
    }

    fn node(&mut self, out: &mut String) -> R<()> {
        let t = self.u8()?;
        if t & 0x80 != 0 {
            return self.container(t, out);
        }
        match t {
            0x30 => out.push_str("null"),
            0x31 => out.push_str("true"),
            0x32 => out.push_str("false"),
            0x3c | 0x7d => iso(self.raw(7)?, false, out)?, // DATE, TIMESTAMP7
            0x39 => iso(self.raw(11)?, false, out)?,       // TIMESTAMP
            0x7c => iso(self.raw(13)?, true, out)?,        // TIMESTAMP WITH TIME ZONE
            0x7f => {
                let b = self.raw(4)?;
                out.push_str(&json_float(decode_double(b)?));
            }
            0x36 => {
                let b = self.raw(8)?;
                out.push_str(&json_float(decode_double(b)?));
            }
            0x3e => {
                let b = self.raw(11)?;
                json_string(&interval_ds(b), out);
            }
            0x3d => return Err("INTERVAL YEAR TO MONTH inside JSON is not supported".into()),
            0x33 => {
                let n = usize::from(self.u8()?);
                json_string(&utf8(self.raw(n)?), out);
            }
            0x37 => {
                let n = usize::from(self.u16()?);
                json_string(&utf8(self.raw(n)?), out);
            }
            0x38 => {
                let n = self.u32()? as usize;
                json_string(&utf8(self.raw(n)?), out);
            }
            0x34 => {
                let n = usize::from(self.u8()?);
                out.push_str(&json_number(self.raw(n)?)?);
            }
            0x7e => {
                let n = usize::from(self.u8()?);
                json_string(&hex(self.raw(n)?), out);
            }
            0x3a => {
                let n = usize::from(self.u16()?);
                json_string(&hex(self.raw(n)?), out);
            }
            0x3b => {
                let n = self.u32()? as usize;
                json_string(&hex(self.raw(n)?), out);
            }
            0x7b => {
                if self.u8()? != 0x01 {
                    return Err("unknown OSON extended type".into());
                }
                let n = self.u32()? as usize;
                out.push_str(&decode_vector(self.raw(n)?)?);
            }
            _ if matches!(t & 0xF0, 0x20 | 0x60) => {
                let n = usize::from(t & 0x0F) + 1;
                out.push_str(&json_number(self.raw(n)?)?);
            }
            _ if matches!(t & 0xF0, 0x40 | 0x50) => {
                let n = usize::from(t & 0x0F);
                out.push_str(&json_number(self.raw(n)?)?);
            }
            _ if t & 0xE0 == 0 => {
                let s = utf8(self.raw(usize::from(t))?);
                json_string(&s, out);
            }
            _ => return Err(format!("OSON node type {t:#04x} not supported")),
        }
        Ok(())
    }
}

fn utf8(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

fn json_number(b: &[u8]) -> R<String> {
    let s = decode_number(b)?;
    // JSON has no leading "." or "-.": Oracle text never produces them, but
    // be safe.
    Ok(if let Some(rest) = s.strip_prefix("-.") { format!("-0.{rest}") } else if s.starts_with('.') { format!("0{s}") } else { s })
}

fn json_float(f: f64) -> String {
    if f.is_finite() { format!("{f}") } else { format!("\"{f}\"") }
}

/// An ISO-8601 string from an Oracle DATE / TIMESTAMP (+ offset) image.
fn iso(b: &[u8], tz: bool, out: &mut String) -> R<()> {
    let d = decode_date(b)?;
    let mut s = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", d.year, d.month, d.day, d.hour, d.minute, d.second);
    if d.nanos != 0 {
        s.push_str(format!(".{:09}", d.nanos).trim_end_matches('0'));
    }
    if tz {
        if let crate::value::Value::TimestampTz(local, off) = crate::value::decode_timestamp_tz(b)? {
            s = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", local.year, local.month, local.day, local.hour, local.minute, local.second);
            if local.nanos != 0 {
                s.push_str(format!(".{:09}", local.nanos).trim_end_matches('0'));
            }
            s.push_str(&crate::value::fmt_offset(off));
        }
    }
    json_string(&s, out);
    Ok(())
}

fn interval_ds(b: &[u8]) -> String {
    let days = i64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])) - 0x8000_0000;
    let (h, m, s) = (i64::from(b[4]) - 60, i64::from(b[5]) - 60, i64::from(b[6]) - 60);
    let ns = i64::from(u32::from_be_bytes([b[7], b[8], b[9], b[10]])) - 0x8000_0000;
    let neg = days < 0 || h < 0 || m < 0 || s < 0 || ns < 0;
    format!("{}{:02} {:02}:{:02}:{:02}.{:09}", if neg { '-' } else { '+' }, days.abs(), h.abs(), m.abs(), s.abs(), ns.abs())
}

pub(crate) fn json_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Decode an OSON image to JSON text.
pub(crate) fn decode_oson(data: &[u8]) -> R<String> {
    Oson::decode(data)
}

/// Decode a VECTOR image to its JSON array text (`[1.5,-2,3]`; a sparse
/// vector as `[dimensions,[indices],[values]]`, Oracle's sparse text form).
pub(crate) fn decode_vector(data: &[u8]) -> R<String> {
    let mut d = Oson { data, pos: 0, relative_offsets: false, field_id_length: 1, field_names: Vec::new(), tree_seg_pos: 0 };
    if d.u8()? != 0xDB {
        return Err("not a VECTOR image".into());
    }
    if d.u8()? > 2 {
        return Err("VECTOR image version not supported".into());
    }
    let flags = d.u16()?;
    let format = d.u8()?;
    let mut n = d.u32()?;
    if flags & 0x0012 != 0 {
        d.raw(8)?; // norm
    }
    let mut indices = None;
    if flags & 0x0020 != 0 {
        let dims = n;
        n = u32::from(d.u16()?);
        let idx: Vec<String> = (0..n).map(|_| d.u32().map(|i| i.to_string())).collect::<R<_>>()?;
        indices = Some((dims, idx));
    }
    let values: Vec<String> = match format {
        2 => (0..n).map(|_| d.raw(4).and_then(decode_double).map(|f| format!("{}", f as f32))).collect::<R<_>>()?,
        3 => (0..n).map(|_| d.raw(8).and_then(decode_double).map(json_float)).collect::<R<_>>()?,
        4 => (0..n).map(|_| d.u8().map(|b| (b as i8).to_string())).collect::<R<_>>()?,
        5 => (0..n / 8).map(|_| d.u8().map(|b| b.to_string())).collect::<R<_>>()?,
        f => return Err(format!("VECTOR format {f} not supported")),
    };
    Ok(match indices {
        Some((dims, idx)) => format!("[{dims},[{}],[{}]]", idx.join(","), values.join(",")),
        None => format!("[{}]", values.join(",")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_json_strings() {
        let mut s = String::new();
        json_string("a\"b\\c\n\u{1}✓", &mut s);
        assert_eq!(s, r#""a\"b\\c\n\u0001✓""#);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_oson(b"nope").is_err());
        assert!(decode_vector(&[0xDB, 0, 0, 0, 2, 0, 0, 0, 5]).is_err(), "truncated");
    }
}
