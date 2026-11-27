//! Record batches (message format v2): encoding for produce, decoding of
//! fetched batches — CRC-32C checked, gzip / snappy decompressed, control
//! batches skipped, a truncated trailing batch ignored (a fetch may cut one).

use crate::protocol::{R, W};
use kernway_messaging::MsgError;

/// One record.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Record {
    /// Its offset (set when read).
    pub offset: i64,
    /// Milliseconds since the epoch.
    pub timestamp: i64,
    /// Key.
    pub key: Option<Vec<u8>>,
    /// Value.
    pub value: Option<Vec<u8>>,
    /// Headers (values may be null).
    pub headers: Vec<(String, Option<Vec<u8>>)>,
}

/// Compression codecs (attributes bits 0–2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    /// None.
    #[default]
    None,
    /// gzip.
    Gzip,
    /// Snappy (xerial framing, as the Java client writes it, or raw).
    Snappy,
}

/// The idempotent producer's identity for a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProducerIds {
    /// Producer id (-1: none).
    pub id: i64,
    /// Its epoch.
    pub epoch: i16,
    /// The first record's sequence number in its partition.
    pub base_sequence: i32,
}

impl ProducerIds {
    /// Not idempotent.
    pub const NONE: ProducerIds = ProducerIds { id: -1, epoch: -1, base_sequence: -1 };
}

/// CRC-32C (Castagnoli), as record batches use.
pub fn crc32c(data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let t = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0x82F6_3B78 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    });
    let mut crc = !0u32;
    for &b in data {
        crc = t[((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Encode records as one uncompressed batch (offsets from 0).
pub fn encode_batch(records: &[Record], ids: ProducerIds) -> Vec<u8> {
    let base_ts = records.first().map_or(0, |r| r.timestamp);
    let max_ts = records.iter().map(|r| r.timestamp).max().unwrap_or(base_ts);
    let mut body = W::new(false);
    for (i, r) in records.iter().enumerate() {
        let mut rec = W::new(false);
        rec.i8(0).varint(r.timestamp - base_ts).varint(i as i64);
        match &r.key {
            Some(k) => {
                rec.varint(k.len() as i64);
                rec.b.extend_from_slice(k);
            }
            None => {
                rec.varint(-1);
            }
        }
        match &r.value {
            Some(v) => {
                rec.varint(v.len() as i64);
                rec.b.extend_from_slice(v);
            }
            None => {
                rec.varint(-1);
            }
        }
        rec.varint(r.headers.len() as i64);
        for (k, v) in &r.headers {
            rec.varint(k.len() as i64);
            rec.b.extend_from_slice(k.as_bytes());
            match v {
                Some(v) => {
                    rec.varint(v.len() as i64);
                    rec.b.extend_from_slice(v);
                }
                None => {
                    rec.varint(-1);
                }
            }
        }
        body.varint(rec.b.len() as i64);
        body.b.extend_from_slice(&rec.b);
    }
    // From attributes to the end: what the CRC covers.
    let mut tail = W::new(false);
    tail.i16(0) // attributes: no compression, create time, not transactional
        .i32(records.len() as i32 - 1)
        .i64(base_ts)
        .i64(max_ts)
        .i64(ids.id)
        .i16(ids.epoch)
        .i32(ids.base_sequence)
        .i32(records.len() as i32);
    tail.b.extend_from_slice(&body.b);
    let mut out = W::new(false);
    out.i64(0) // base offset (the broker assigns)
        .i32((4 + 1 + 4 + tail.b.len()) as i32) // batch length: from the leader epoch on
        .i32(-1) // partition leader epoch
        .i8(2) // magic
        .i32(crc32c(&tail.b) as i32);
    out.b.extend_from_slice(&tail.b);
    out.b
}

fn decompress(codec: i16, data: &[u8], max: usize) -> Result<Vec<u8>, MsgError> {
    let bad = |m: String| MsgError::Protocol(format!("Kafka record batch: {m}"));
    match codec {
        0 => Ok(data.to_vec()),
        1 => {
            use std::io::Read;
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(data).take(max as u64 + 1).read_to_end(&mut out).map_err(|e| bad(format!("gzip: {e}")))?;
            if out.len() > max {
                return Err(bad("gzip output too large".into()));
            }
            Ok(out)
        }
        2 => {
            // xerial framing: magic, version, compatible version, then
            // (length, raw snappy block) pairs.
            const MAGIC: &[u8] = b"\x82SNAPPY\x00";
            if let Some(rest) = data.strip_prefix(MAGIC) {
                let mut r = R::new(rest, false);
                r.i32()?;
                r.i32()?;
                let mut out = Vec::new();
                while r.remaining() > 0 {
                    let n = r.i32()?;
                    let block = r.raw(usize::try_from(n).map_err(|_| bad("negative snappy block".into()))?)?;
                    out.extend(kernway_wire::snappy::decompress(block, max.saturating_sub(out.len())).map_err(bad)?);
                }
                Ok(out)
            } else {
                kernway_wire::snappy::decompress(data, max).map_err(bad)
            }
        }
        3 => Err(bad("lz4 compression is not supported".into())),
        4 => Err(bad("zstd compression is not supported".into())),
        c => Err(bad(format!("unknown compression {c}"))),
    }
}

/// Decode every complete batch in a fetched record set, skipping control
/// batches; records before `from` are dropped (a batch may start earlier).
pub fn decode_batches(data: &[u8], from: i64, max_uncompressed: usize) -> Result<Vec<Record>, MsgError> {
    let mut out = Vec::new();
    let mut r = R::new(data, false);
    while r.remaining() >= 12 {
        let base_offset = r.i64()?;
        let len = r.i32()?;
        if len < 0 || len as usize > r.remaining() {
            // A partial batch at the end of a fetch: the next fetch brings it.
            break;
        }
        let batch = r.raw(len as usize)?;
        let mut b = R::new(batch, false);
        b.i32()?; // leader epoch
        let magic = b.i8()?;
        if magic != 2 {
            return Err(MsgError::Protocol(format!("Kafka: record batch magic {magic} at offset {base_offset} (only v2 is read)")));
        }
        let crc = b.u32()?;
        let covered = &batch[b.pos..];
        if crc32c(covered) != crc {
            return Err(MsgError::Protocol(format!("Kafka: record batch at offset {base_offset} fails its CRC-32C")));
        }
        let attributes = b.i16()?;
        b.i32()?; // last offset delta
        let base_ts = b.i64()?;
        b.i64()?; // max timestamp
        b.i64()?; // producer id
        b.i16()?; // producer epoch
        b.i32()?; // base sequence
        let count = b.i32()?;
        if attributes & 0x20 != 0 {
            continue; // control batch (transaction markers)
        }
        let body = decompress(attributes & 0x07, &batch[b.pos..], max_uncompressed)?;
        let mut rr = R::new(&body, false);
        for _ in 0..count.max(0) {
            let rec_len = rr.varint()?;
            let rec = rr.raw(usize::try_from(rec_len).map_err(|_| MsgError::Protocol("Kafka: negative record length".into()))?)?;
            let mut x = R::new(rec, false);
            x.i8()?;
            let ts = base_ts + x.varint()?;
            let offset = base_offset + x.varint()?;
            let field = |x: &mut R<'_>| -> Result<Option<Vec<u8>>, MsgError> {
                let n = x.varint()?;
                if n < 0 {
                    return Ok(None);
                }
                Ok(Some(x.raw(n as usize)?.to_vec()))
            };
            let key = field(&mut x)?;
            let value = field(&mut x)?;
            let nh = x.varint()?;
            if nh < 0 || nh as usize > x.remaining() {
                return Err(MsgError::Protocol("Kafka: bad header count".into()));
            }
            let mut headers = Vec::with_capacity(nh as usize);
            for _ in 0..nh {
                let k = field(&mut x)?.unwrap_or_default();
                headers.push((String::from_utf8_lossy(&k).into_owned(), field(&mut x)?));
            }
            if offset >= from {
                out.push(Record { offset, timestamp: ts, key, value, headers });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_known_values() {
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
    }

    #[test]
    fn batches_round_trip_and_are_checked() {
        let recs = vec![
            Record { offset: 0, timestamp: 1_700_000_000_000, key: Some(b"k".to_vec()), value: Some(b"v1".to_vec()), headers: vec![("h".into(), Some(b"x".to_vec())), ("n".into(), None)] },
            Record { offset: 1, timestamp: 1_700_000_000_005, key: None, value: None, headers: vec![] },
            Record { offset: 2, timestamp: 1_700_000_000_001, key: Some(vec![]), value: Some(vec![0; 300]), headers: vec![] },
        ];
        let mut batch = encode_batch(&recs, ProducerIds { id: 7, epoch: 0, base_sequence: 3 });
        // As the broker stores it: base offset 40.
        batch[..8].copy_from_slice(&40i64.to_be_bytes());
        let back = decode_batches(&batch, 0, 1 << 20).unwrap();
        let want: Vec<Record> = recs.iter().enumerate().map(|(i, r)| Record { offset: 40 + i as i64, ..r.clone() }).collect();
        assert_eq!(back, want);
        assert_eq!(decode_batches(&batch, 41, 1 << 20).unwrap().len(), 2, "records before the fetch offset dropped");
        // Two batches back to back, the second cut short: only the first.
        let mut two = batch.clone();
        two.extend_from_slice(&batch[..batch.len() - 5]);
        assert_eq!(decode_batches(&two, 0, 1 << 20).unwrap().len(), 3);
        // A flipped bit fails the CRC.
        let mut bad = batch.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(decode_batches(&bad, 0, 1 << 20).is_err());
    }

    #[test]
    fn compressed_batches_decode() {
        let recs = vec![Record { timestamp: 5, value: Some(b"compressed value".repeat(50)), ..Record::default() }];
        let plain = encode_batch(&recs, ProducerIds::NONE);
        // Rebuild the batch with a compressed body (as another client writes it).
        let header_len = 8 + 4 + 4 + 1 + 4 + 2 + 4 + 8 + 8 + 8 + 2 + 4 + 4;
        let body = &plain[header_len..];
        for (codec, compressed) in [
            (1i16, {
                use std::io::Write;
                let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                e.write_all(body).unwrap();
                e.finish().unwrap()
            }),
            (2, kernway_wire::snappy::compress(body)),
            (2, {
                let block = kernway_wire::snappy::compress(body);
                let mut x = b"\x82SNAPPY\x00".to_vec();
                x.extend_from_slice(&1i32.to_be_bytes());
                x.extend_from_slice(&1i32.to_be_bytes());
                x.extend_from_slice(&(block.len() as i32).to_be_bytes());
                x.extend_from_slice(&block);
                x
            }),
        ] {
            let mut tail = plain[21..header_len].to_vec();
            tail[..2].copy_from_slice(&codec.to_be_bytes());
            tail.extend_from_slice(&compressed);
            let mut b = plain[..17].to_vec();
            b[8..12].copy_from_slice(&((4 + 1 + 4 + tail.len()) as i32).to_be_bytes());
            b.extend_from_slice(&crc32c(&tail).to_be_bytes());
            b.extend_from_slice(&tail);
            let got = decode_batches(&b, 0, 1 << 20).unwrap();
            assert_eq!(got[0].value, recs[0].value, "codec {codec}");
            assert!(decode_batches(&b, 0, 100).is_err(), "codec {codec}: size cap");
        }
    }
}
