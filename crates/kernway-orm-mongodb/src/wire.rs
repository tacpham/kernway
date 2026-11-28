//! The MongoDB wire protocol: `OP_MSG` (opcode 2013) framing, document
//! sequences for bulk payloads, and `OP_COMPRESSED` (2012) with snappy
//! (written here) or zlib.
//!
//! Message header (little-endian): `messageLength`, `requestID`,
//! `responseTo`, `opCode`. `OP_MSG` body: `flagBits` then sections — kind 0
//! (the command document) and kind 1 (`size`, identifier, documents: e.g.
//! the `documents` of an insert, sent without nesting them in the command).

use crate::bson::{BsonError, Document};
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
pub(crate) fn encode_msg(out: &mut Vec<u8>, request_id: i32, flags: u32, command: &Document, seqs: &[Sequence<'_>], compressor: Option<Compressor>) -> std::result::Result<(), BsonError> {
    out.clear();
    let mut body = Vec::with_capacity(256);
    body.extend_from_slice(&flags.to_le_bytes());
    body.push(0);
    command.encode_into(&mut body)?;
    for s in seqs {
        body.push(1);
        let at = body.len();
        body.extend_from_slice(&[0; 4]);
        body.extend_from_slice(s.identifier.as_bytes());
        body.push(0);
        for d in s.documents {
            d.encode_into(&mut body)?;
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
                Compressor::Snappy => (1u8, kernway_wire::snappy::compress(&body)),
                Compressor::Zlib(level) => (2u8, zlib_compress(&body, level)),
            };
            header(out, request_id, OP_COMPRESSED, 16 + 9 + data.len());
            out.extend_from_slice(&OP_MSG.to_le_bytes());
            out.extend_from_slice(&(body.len() as i32).to_le_bytes());
            out.push(id);
            out.extend_from_slice(&data);
        }
    }
    Ok(())
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
                1 => kernway_wire::snappy::decompress(&msg[25..], size as usize).map_err(Error::protocol)?,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc;

    #[test]
    fn op_msg_round_trips_compressed_or_not() {
        let cmd = doc! { "insert" => "items", "$db" => "test" };
        let docs = vec![doc! { "a" => 1 }, doc! { "a" => 2 }];
        for comp in [None, Some(Compressor::Snappy), Some(Compressor::Zlib(6))] {
            let mut out = Vec::new();
            encode_msg(&mut out, 7, 0, &cmd, &[Sequence { identifier: "documents", documents: &docs }], comp).unwrap();
            assert_eq!(i32::from_le_bytes(out[..4].try_into().unwrap()) as usize, out.len());
            let op = i32::from_le_bytes(out[12..16].try_into().unwrap());
            assert_eq!(op, if comp.is_some() { OP_COMPRESSED } else { OP_MSG });
            let r = decode_reply(&out, 1 << 20).unwrap();
            assert_eq!(r.document, cmd);
        }
        // hello is never compressed.
        let mut out = Vec::new();
        encode_msg(&mut out, 1, 0, &doc! { "hello" => 1 }, &[], Some(Compressor::Snappy)).unwrap();
        assert_eq!(i32::from_le_bytes(out[12..16].try_into().unwrap()), OP_MSG);
        assert!(decode_reply(&out[..10], 1 << 20).is_err());
    }
}
