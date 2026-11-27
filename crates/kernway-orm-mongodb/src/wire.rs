//! The MongoDB wire protocol: `OP_MSG` (opcode 2013) framing, document
//! sequences for bulk payloads, and `OP_COMPRESSED` (2012) with snappy
//! (written here) or zlib.
//!
//! Message header (little-endian): `messageLength`, `requestID`,
//! `responseTo`, `opCode`. `OP_MSG` body: `flagBits` then sections — kind 0
//! (the command document) and kind 1 (`size`, identifier, documents: e.g.
//! the `documents` of an insert, sent without nesting them in the command).

use crate::bson::Document;
use crate::error::{Error, Result};
use crate::options::Compressor;

pub(crate) const OP_COMPRESSED: i32 = 2012;
pub(crate) const OP_MSG: i32 = 2013;
/// `flagBits`: a CRC-32C checksum follows the sections.
pub(crate) const CHECKSUM_PRESENT: u32 = 1;
/// `flagBits`: another reply follows without a request (exhaust / streaming hello).
pub(crate) const MORE_TO_COME: u32 = 1 << 1;

/// Commands never compressed (they carry credentials or set up compression).
const UNCOMPRESSIBLE: &[&str] =
    &["hello", "ismaster", "saslstart", "saslcontinue", "getnonce", "authenticate", "createuser", "updateuser", "copydbsaslstart", "copydbgetnonce", "copydb"];

/// A document sequence (kind-1 section).
pub(crate) struct Sequence<'a> {
    pub identifier: &'a str,
    pub documents: &'a [Document],
}

/// Encode one `OP_MSG` request into `out` (cleared), compressed with
/// `compressor` unless the command must not be.
pub(crate) fn encode_msg(out: &mut Vec<u8>, request_id: i32, flags: u32, command: &Document, seqs: &[Sequence<'_>], compressor: Option<Compressor>) {
    out.clear();
    let mut body = Vec::with_capacity(256);
    body.extend_from_slice(&flags.to_le_bytes());
    body.push(0);
    command.encode_into(&mut body);
    for s in seqs {
        body.push(1);
        let at = body.len();
        body.extend_from_slice(&[0; 4]);
        body.extend_from_slice(s.identifier.as_bytes());
        body.push(0);
        for d in s.documents {
            d.encode_into(&mut body);
        }
        let size = (body.len() - at) as i32;
        body[at..at + 4].copy_from_slice(&size.to_le_bytes());
    }
    let name = command.first_key().unwrap_or("").to_ascii_lowercase();
    let compressor = compressor.filter(|_| !UNCOMPRESSIBLE.contains(&name.as_str()));
    match compressor {
        None => {
            header(out, request_id, OP_MSG, 16 + body.len());
            out.extend_from_slice(&body);
        }
        Some(c) => {
            let (id, data) = match c {
                Compressor::Snappy => (1u8, snappy_compress(&body)),
                Compressor::Zlib(level) => (2u8, zlib_compress(&body, level)),
            };
            header(out, request_id, OP_COMPRESSED, 16 + 9 + data.len());
            out.extend_from_slice(&OP_MSG.to_le_bytes());
            out.extend_from_slice(&(body.len() as i32).to_le_bytes());
            out.push(id);
            out.extend_from_slice(&data);
        }
    }
}

fn header(out: &mut Vec<u8>, request_id: i32, op: i32, len: usize) {
    out.extend_from_slice(&(len as i32).to_le_bytes());
    out.extend_from_slice(&request_id.to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes());
    out.extend_from_slice(&op.to_le_bytes());
}

/// A decoded reply.
pub(crate) struct Reply {
    pub response_to: i32,
    pub flags: u32,
    pub document: Document,
}

/// Decode a whole message (header included) into its reply document.
pub(crate) fn decode_reply(msg: &[u8], max_size: usize) -> Result<Reply> {
    if msg.len() < 16 {
        return Err(Error::protocol("message shorter than its header"));
    }
    let i32_at = |b: &[u8], at: usize| i32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]);
    let response_to = i32_at(msg, 8);
    let op = i32_at(msg, 12);
    let decompressed;
    let body = match op {
        OP_MSG => &msg[16..],
        OP_COMPRESSED => {
            if msg.len() < 25 {
                return Err(Error::protocol("short OP_COMPRESSED"));
            }
            let original = i32_at(msg, 16);
            let size = i32_at(msg, 20);
            if original != OP_MSG {
                return Err(Error::protocol(format!("compressed opcode {original} is not OP_MSG")));
            }
            if size < 0 || size as usize > max_size {
                return Err(Error::protocol(format!("uncompressed size {size} out of range")));
            }
            decompressed = match msg[24] {
                0 => msg[25..].to_vec(),
                1 => snappy_decompress(&msg[25..], size as usize)?,
                2 => zlib_decompress(&msg[25..], size as usize)?,
                other => return Err(Error::protocol(format!("unknown compressor id {other}"))),
            };
            if decompressed.len() != size as usize {
                return Err(Error::protocol("decompressed size does not match"));
            }
            &decompressed[..]
        }
        other => return Err(Error::protocol(format!("unexpected opcode {other}"))),
    };
    if body.len() < 5 {
        return Err(Error::protocol("OP_MSG without sections"));
    }
    let flags = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    let mut sections = &body[4..];
    if flags & CHECKSUM_PRESENT != 0 {
        if sections.len() < 4 {
            return Err(Error::protocol("checksum missing"));
        }
        sections = &sections[..sections.len() - 4];
    }
    let mut document = None;
    while let Some((&kind, rest)) = sections.split_first() {
        match kind {
            0 => {
                let (d, used) = Document::from_prefix(rest)?;
                document = Some(d);
                sections = &rest[used..];
            }
            1 => {
                // Servers do not send sequences in replies today; skip one if they do.
                if rest.len() < 4 {
                    return Err(Error::protocol("short document sequence"));
                }
                let size = i32_at(rest, 0);
                if size < 4 || size as usize > rest.len() {
                    return Err(Error::protocol("bad document sequence size"));
                }
                sections = &rest[size as usize..];
            }
            other => return Err(Error::protocol(format!("unknown section kind {other}"))),
        }
    }
    let document = document.ok_or_else(|| Error::protocol("OP_MSG reply without a body section"))?;
    Ok(Reply { response_to, flags, document })
}

// ---- zlib -----------------------------------------------------------------

fn zlib_compress(data: &[u8], level: i32) -> Vec<u8> {
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;
    let level = if level < 0 { Compression::default() } else { Compression::new(level as u32) };
    let mut e = ZlibEncoder::new(Vec::with_capacity(data.len() / 2 + 16), level);
    let _ = e.write_all(data);
    e.finish().unwrap_or_default()
}

fn zlib_decompress(data: &[u8], size: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::with_capacity(size);
    flate2::read::ZlibDecoder::new(data)
        .take(size as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| Error::protocol(format!("zlib: {e}")))?;
    Ok(out)
}

// ---- snappy (raw format) -------------------------------------------------------

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn emit_literal(out: &mut Vec<u8>, lit: &[u8]) {
    if lit.is_empty() {
        return;
    }
    let n = lit.len() - 1;
    if n < 60 {
        out.push((n as u8) << 2);
    } else if n < 1 << 8 {
        out.push(60 << 2);
        out.push(n as u8);
    } else if n < 1 << 16 {
        out.push(61 << 2);
        out.extend_from_slice(&(n as u16).to_le_bytes());
    } else if n < 1 << 24 {
        out.push(62 << 2);
        out.extend_from_slice(&(n as u32).to_le_bytes()[..3]);
    } else {
        out.push(63 << 2);
        out.extend_from_slice(&(n as u32).to_le_bytes());
    }
    out.extend_from_slice(lit);
}

fn emit_copy(out: &mut Vec<u8>, offset: usize, mut len: usize) {
    // Copies of up to 64 bytes with a 2-byte offset (tag 10); longer split.
    while len > 0 {
        let n = len.min(64);
        if offset < 1 << 16 {
            out.push((((n - 1) as u8) << 2) | 0b10);
            out.extend_from_slice(&(offset as u16).to_le_bytes());
        } else {
            out.push((((n - 1) as u8) << 2) | 0b11);
            out.extend_from_slice(&(offset as u32).to_le_bytes());
        }
        len -= n;
    }
}

/// Snappy compression: a greedy matcher over a 4-byte hash table.
pub(crate) fn snappy_compress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2 + 16);
    varint(&mut out, data.len() as u64);
    const BITS: u32 = 14;
    let mut table = vec![0usize; 1 << BITS];
    let hash = |b: &[u8]| (u32::from_le_bytes([b[0], b[1], b[2], b[3]]).wrapping_mul(0x1e35_a7bd) >> (32 - BITS)) as usize;
    let mut lit_start = 0;
    let mut i = 0;
    while i + 4 <= data.len() {
        let h = hash(&data[i..]);
        let cand = table[h];
        table[h] = i + 1; // +1: 0 means empty
        if cand > 0 {
            let c = cand - 1;
            if i - c < 1 << 31 && data[c..c + 4] == data[i..i + 4] {
                let mut len = 4;
                while i + len < data.len() && data[c + len] == data[i + len] {
                    len += 1;
                }
                emit_literal(&mut out, &data[lit_start..i]);
                emit_copy(&mut out, i - c, len);
                i += len;
                lit_start = i;
                continue;
            }
        }
        i += 1;
    }
    emit_literal(&mut out, &data[lit_start..]);
    out
}

/// Snappy decompression, refusing output beyond `max`.
pub(crate) fn snappy_decompress(src: &[u8], max: usize) -> Result<Vec<u8>> {
    let bad = |m: &str| Error::protocol(format!("snappy: {m}"));
    let mut pos = 0;
    let mut len: u64 = 0;
    let mut shift = 0;
    loop {
        let b = *src.get(pos).ok_or_else(|| bad("truncated length"))?;
        pos += 1;
        len |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 35 {
            return Err(bad("length varint too long"));
        }
    }
    if len as usize > max {
        return Err(bad("output larger than announced"));
    }
    let mut out = Vec::with_capacity(len as usize);
    while pos < src.len() {
        let tag = src[pos];
        pos += 1;
        match tag & 0b11 {
            0 => {
                let mut n = (tag >> 2) as usize;
                if n >= 60 {
                    let extra = n - 59;
                    let b = src.get(pos..pos + extra).ok_or_else(|| bad("truncated literal length"))?;
                    n = b.iter().rev().fold(0usize, |a, &x| (a << 8) | x as usize);
                    pos += extra;
                }
                let n = n + 1;
                let lit = src.get(pos..pos + n).ok_or_else(|| bad("truncated literal"))?;
                out.extend_from_slice(lit);
                pos += n;
            }
            kind => {
                let (n, offset) = match kind {
                    1 => {
                        let b = *src.get(pos).ok_or_else(|| bad("truncated copy"))?;
                        pos += 1;
                        ((((tag >> 2) & 0b111) + 4) as usize, (((tag >> 5) as usize) << 8) | b as usize)
                    }
                    2 => {
                        let b = src.get(pos..pos + 2).ok_or_else(|| bad("truncated copy"))?;
                        pos += 2;
                        (((tag >> 2) + 1) as usize, u16::from_le_bytes([b[0], b[1]]) as usize)
                    }
                    _ => {
                        let b = src.get(pos..pos + 4).ok_or_else(|| bad("truncated copy"))?;
                        pos += 4;
                        (((tag >> 2) + 1) as usize, u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
                    }
                };
                if offset == 0 || offset > out.len() {
                    return Err(bad("copy offset out of range"));
                }
                let start = out.len() - offset;
                for k in 0..n {
                    let byte = out[start + k];
                    out.push(byte);
                }
            }
        }
        if out.len() > len as usize {
            return Err(bad("output overruns its length"));
        }
    }
    if out.len() != len as usize {
        return Err(bad("output shorter than its length"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc;

    #[test]
    fn snappy_round_trips() {
        let samples: Vec<Vec<u8>> = vec![
            vec![],
            b"a".to_vec(),
            b"abcabcabcabcabcabcabcabcabcabcabcabc".to_vec(),
            (0..100_000u32).map(|i| (i % 251) as u8).collect(),
            (0..70_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect(),
            vec![7; 200_000],
        ];
        for s in samples {
            let c = snappy_compress(&s);
            assert_eq!(snappy_decompress(&c, s.len()).unwrap(), s);
        }
        // Known encoding from the format description: "Wikipedia" literal.
        assert_eq!(snappy_decompress(&[9, 0x20, b'W', b'i', b'k', b'i', b'p', b'e', b'd', b'i', b'a'], 9).unwrap(), b"Wikipedia");
        // Corrupt input is an error, never a panic.
        for bad in [&[5u8, 0x0d, 1][..], &[10, 0x01, 0x00], &[2, 0x00], &[0xff, 0xff, 0xff, 0xff, 0xff, 0x01]] {
            assert!(snappy_decompress(bad, 1 << 20).is_err());
        }
        assert!(snappy_decompress(&snappy_compress(&[1; 1000]), 10).is_err(), "size cap");
    }

    #[test]
    fn op_msg_round_trips_compressed_or_not() {
        let cmd = doc! { "insert" => "items", "$db" => "test" };
        let docs = vec![doc! { "a" => 1 }, doc! { "a" => 2 }];
        for comp in [None, Some(Compressor::Snappy), Some(Compressor::Zlib(6))] {
            let mut out = Vec::new();
            encode_msg(&mut out, 7, 0, &cmd, &[Sequence { identifier: "documents", documents: &docs }], comp);
            assert_eq!(i32::from_le_bytes(out[..4].try_into().unwrap()) as usize, out.len());
            let op = i32::from_le_bytes(out[12..16].try_into().unwrap());
            assert_eq!(op, if comp.is_some() { OP_COMPRESSED } else { OP_MSG });
            let r = decode_reply(&out, 1 << 20).unwrap();
            assert_eq!(r.document, cmd);
        }
        // hello is never compressed.
        let mut out = Vec::new();
        encode_msg(&mut out, 1, 0, &doc! { "hello" => 1 }, &[], Some(Compressor::Snappy));
        assert_eq!(i32::from_le_bytes(out[12..16].try_into().unwrap()), OP_MSG);
        assert!(decode_reply(&out[..10], 1 << 20).is_err());
    }
}
