//! Snappy, raw format (the block format, no framing): written here, shared
//! by the drivers that speak it — MongoDB's OP_COMPRESSED, Kafka record
//! batches.

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

/// Compress (raw format): a greedy matcher over a 4-byte hash table.
pub fn compress(data: &[u8]) -> Vec<u8> {
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

/// Decompress (raw format), refusing output beyond `max`; corrupt input is
/// an error (its reason), never a panic.
pub fn decompress(src: &[u8], max: usize) -> Result<Vec<u8>, String> {
    let bad = |m: &str| format!("snappy: {m}");
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
            let c = compress(&s);
            assert_eq!(decompress(&c, s.len()).unwrap(), s);
        }
        // Known encoding from the format description: "Wikipedia" literal.
        assert_eq!(decompress(&[9, 0x20, b'W', b'i', b'k', b'i', b'p', b'e', b'd', b'i', b'a'], 9).unwrap(), b"Wikipedia");
        // Corrupt input is an error, never a panic.
        for bad in [&[5u8, 0x0d, 1][..], &[10, 0x01, 0x00], &[2, 0x00], &[0xff, 0xff, 0xff, 0xff, 0xff, 0x01]] {
            assert!(decompress(bad, 1 << 20).is_err());
        }
        assert!(decompress(&compress(&[1; 1000]), 10).is_err(), "size cap");
    }
}
