//! RSA public-key encryption with OAEP (SHA-1) — just what
//! `caching_sha2_password` needs to send a password over a connection without
//! TLS: the server's public key (PEM), a Montgomery modular exponentiation,
//! and RFC 8017 OAEP padding. Encryption only; nothing secret is computed
//! here except the padding seed, drawn from the system RNG.

use sha1::{Digest, Sha1};

/// An RSA public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublicKey {
    /// Modulus, big-endian, no leading zero bytes.
    n: Vec<u8>,
    /// Public exponent, big-endian.
    e: Vec<u8>,
}

impl PublicKey {
    /// Parse a PEM `PUBLIC KEY` (SubjectPublicKeyInfo) or `RSA PUBLIC KEY`
    /// (PKCS#1) block.
    pub(crate) fn from_pem(pem: &[u8]) -> Result<Self, String> {
        let text = std::str::from_utf8(pem).map_err(|_| "public key PEM is not text")?;
        let pkcs1 = text.contains("BEGIN RSA PUBLIC KEY");
        // Only the base64 between the BEGIN and END lines (servers may send
        // a trailing NUL or other framing around the block).
        let begin = text.find("-----BEGIN").ok_or("no PEM BEGIN line")?;
        let body_start = begin + text[begin..].find('\n').ok_or("truncated PEM")? + 1;
        let body_end = body_start + text[body_start..].find("-----END").ok_or("no PEM END line")?;
        let b64: String = text[body_start..body_end].chars().filter(|c| !c.is_whitespace()).collect();
        let der = base64_decode(&b64)?;
        let rsa = if pkcs1 {
            der
        } else {
            // SubjectPublicKeyInfo ::= SEQUENCE { algorithm SEQUENCE, BIT STRING }
            let (spki, _) = der_take(&der, 0x30)?;
            let (_alg, rest) = der_take(spki, 0x30)?;
            let (bits, _) = der_take(rest, 0x03)?;
            bits.get(1..).ok_or("empty key bit string")?.to_vec() // skip "unused bits" byte
        };
        // RSAPublicKey ::= SEQUENCE { modulus INTEGER, publicExponent INTEGER }
        let (seq, _) = der_take(&rsa, 0x30)?;
        let (n, rest) = der_take(seq, 0x02)?;
        let (e, _) = der_take(rest, 0x02)?;
        let strip = |v: &[u8]| v.iter().skip_while(|b| **b == 0).copied().collect::<Vec<u8>>();
        let key = Self { n: strip(n), e: strip(e) };
        if key.n.len() < 64 || key.n.last().is_none_or(|b| b & 1 == 0) || key.e.is_empty() {
            return Err("not a usable RSA public key".into());
        }
        Ok(key)
    }

    /// Modulus length in bytes.
    fn size(&self) -> usize {
        self.n.len()
    }

    /// RSAES-OAEP-ENCRYPT with SHA-1 and MGF1-SHA-1 (RFC 8017 §7.1.1), the
    /// padding MySQL's `caching_sha2_password` expects.
    pub(crate) fn encrypt_oaep_sha1(&self, msg: &[u8], seed: [u8; 20]) -> Result<Vec<u8>, String> {
        const H: usize = 20;
        let k = self.size();
        if msg.len() + 2 * H + 2 > k {
            return Err("message too long for the RSA key".into());
        }
        let l_hash = Sha1::digest([]);
        // DB = lHash || PS || 0x01 || M
        let mut db = Vec::with_capacity(k - H - 1);
        db.extend_from_slice(&l_hash);
        db.resize(k - H - 1 - msg.len() - 1, 0);
        db.push(0x01);
        db.extend_from_slice(msg);
        for (d, m) in db.iter_mut().zip(mgf1(&seed, k - H - 1)) {
            *d ^= m;
        }
        let mut masked_seed = seed;
        for (s, m) in masked_seed.iter_mut().zip(mgf1(&db, H)) {
            *s ^= m;
        }
        let mut em = Vec::with_capacity(k);
        em.push(0);
        em.extend_from_slice(&masked_seed);
        em.extend_from_slice(&db);
        Ok(self.raw_encrypt(&em))
    }

    /// `m^e mod n` for `m < n`, as `k` big-endian bytes.
    fn raw_encrypt(&self, m: &[u8]) -> Vec<u8> {
        let n = Limbs::from_be(&self.n);
        let k = n.0.len();
        let out = modpow(&Limbs::from_be_len(m, k), &self.e, &n);
        out.to_be(self.size())
    }
}

/// MGF1 with SHA-1.
fn mgf1(seed: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 20);
    let mut counter = 0u32;
    while out.len() < len {
        let mut h = Sha1::new();
        h.update(seed);
        h.update(counter.to_be_bytes());
        out.extend_from_slice(&h.finalize());
        counter += 1;
    }
    out.truncate(len);
    out
}

/// One DER element with tag `tag` at the start of `der`: (contents, rest).
fn der_take(der: &[u8], tag: u8) -> Result<(&[u8], &[u8]), String> {
    if der.first() != Some(&tag) {
        return Err(format!("expected DER tag {tag:#04x}"));
    }
    let first = *der.get(1).ok_or("truncated DER")?;
    let (len, start) = if first < 0x80 {
        (usize::from(first), 2)
    } else {
        let n = usize::from(first & 0x7F);
        if n == 0 || n > 4 {
            return Err("bad DER length".into());
        }
        let bytes = der.get(2..2 + n).ok_or("truncated DER")?;
        (bytes.iter().fold(0usize, |a, b| a << 8 | usize::from(*b)), 2 + n)
    };
    let body = der.get(start..start + len).ok_or("truncated DER")?;
    Ok((body, &der[start + len..]))
}

fn base64_decode(s: &str) -> Result<Vec<u8>, String> {
    let val = |c: u8| -> Result<u32, String> {
        Ok(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err("bad base64 in public key".into()),
        }
        .into())
    };
    let data = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    for chunk in data.chunks(4) {
        let mut acc = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            acc |= val(*c)? << (18 - 6 * i);
        }
        let bytes = acc.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    Ok(out)
}

/// A non-negative integer as little-endian 32-bit limbs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Limbs(Vec<u32>);

impl Limbs {
    fn from_be(b: &[u8]) -> Self {
        Self::from_be_len(b, b.len().div_ceil(4))
    }

    /// `b` (big-endian) in exactly `limbs` limbs.
    fn from_be_len(b: &[u8], limbs: usize) -> Self {
        let mut v = vec![0u32; limbs];
        for (i, byte) in b.iter().rev().enumerate() {
            v[i / 4] |= u32::from(*byte) << (8 * (i % 4));
        }
        Self(v)
    }

    fn to_be(&self, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        for (i, slot) in out.iter_mut().rev().enumerate() {
            *slot = (self.0.get(i / 4).copied().unwrap_or(0) >> (8 * (i % 4))) as u8;
        }
        out
    }
}

/// `a >= b` for equal-length limb vectors.
fn ge(a: &[u32], b: &[u32]) -> bool {
    for (x, y) in a.iter().rev().zip(b.iter().rev()) {
        if x != y {
            return x > y;
        }
    }
    true
}

/// `a -= b` (equal lengths, `a >= b`).
fn sub_in_place(a: &mut [u32], b: &[u32]) {
    let mut borrow = 0i64;
    for (x, y) in a.iter_mut().zip(b) {
        let d = i64::from(*x) - i64::from(*y) - borrow;
        *x = d as u32;
        borrow = i64::from(d < 0);
    }
}

/// Montgomery product `a * b * R^-1 mod n` (R = 2^(32k)), CIOS method.
fn mont_mul(a: &[u32], b: &[u32], n: &[u32], n0inv: u32) -> Vec<u32> {
    let k = n.len();
    let mut t = vec![0u32; k + 2];
    for &ai in a {
        let mut c = 0u64;
        for j in 0..k {
            let s = u64::from(t[j]) + u64::from(ai) * u64::from(b[j]) + c;
            t[j] = s as u32;
            c = s >> 32;
        }
        let s = u64::from(t[k]) + c;
        t[k] = s as u32;
        t[k + 1] = (s >> 32) as u32;

        let m = t[0].wrapping_mul(n0inv);
        let mut c = (u64::from(t[0]) + u64::from(m) * u64::from(n[0])) >> 32;
        for j in 1..k {
            let s = u64::from(t[j]) + u64::from(m) * u64::from(n[j]) + c;
            t[j - 1] = s as u32;
            c = s >> 32;
        }
        let s = u64::from(t[k]) + c;
        t[k - 1] = s as u32;
        t[k] = t[k + 1] + (s >> 32) as u32;
        t[k + 1] = 0;
    }
    let mut r = t[..k].to_vec();
    if t[k] != 0 || ge(&r, n) {
        sub_in_place(&mut r, n);
    }
    r
}

/// `base^exp mod n` for odd `n` and `base < n` (`exp` big-endian).
fn modpow(base: &Limbs, exp: &[u8], n: &Limbs) -> Limbs {
    let n = &n.0;
    let k = n.len();
    // -n^-1 mod 2^32 by Newton iteration.
    let mut inv = 1u32;
    for _ in 0..5 {
        inv = inv.wrapping_mul(2u32.wrapping_sub(n[0].wrapping_mul(inv)));
    }
    let n0inv = inv.wrapping_neg();
    // R^2 mod n by doubling 1 a total of 2 * 32k times.
    let mut r2 = vec![0u32; k];
    r2[0] = 1;
    for _ in 0..64 * k {
        let mut carry = 0u32;
        for limb in r2.iter_mut() {
            let next = *limb >> 31;
            *limb = *limb << 1 | carry;
            carry = next;
        }
        if carry == 1 || ge(&r2, n) {
            sub_in_place(&mut r2, n);
        }
    }
    let mut one = vec![0u32; k];
    one[0] = 1;
    let b = mont_mul(&base.0, &r2, n, n0inv);
    let mut acc = mont_mul(&r2, &one, n, n0inv); // R mod n = Montgomery 1
    for byte in exp {
        for bit in (0..8).rev() {
            acc = mont_mul(&acc, &acc, n, n0inv);
            if byte >> bit & 1 == 1 {
                acc = mont_mul(&acc, &b, n, n0inv);
            }
        }
    }
    Limbs(mont_mul(&acc, &one, n, n0inv))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known answers computed independently with Python's pow().
    const N1: &str = "98f135d25f557203301850c5a38fd547923a736994e3bf911a61dbe22e44158bae97ba94d0eda82f8f6d05584ef8aa38922766581e27a1c08a6a63ec24ede6a46b4cb2424a23d5962217beaddbc496cb8e81973e0becd7b03898d190f9ebdacc0cb1e29c658cda1495e60af593bd04cf0fd630f1f29d0da9953f48f1a09f76b5a170b33839263059f28c105d1fb17c2390c192cfd3ac94af0f21ddb66cad4a268d116ece1738f7d93d9c172411e20b8f6b0d549b6f03675a1600a35a099950d836f675cc81e74ef5e8e25d940ed904759531985d5d9dc9f81818e811892f902bd23f0824128b2f330c5c7fd0a6a3a4506513270e269e0d37f2a74de452e6b439";
    const B1: &str = "7f261498289fcd59a54a7bb1fee08f571242425051c1ccd17f9acae01f5057ca02135e92b1d3f28ede0d7ac3baea9e13deef86ab1031d0f646e1f40a097c976bf46c697d2caf82eeeacbe226e875555790f82ec1d3fcff2a3af4d46b0a18e8830e07bc1e398f1012bd4acefaecbd389be4bcfc49b64a0872e6cc3ababced2057ee05cde00902c77ebff206867347214cdd2055930d6eaf14f4733f3e7d1bfbc7a2ea20b2f14c942e05319acb5c74273f98e2774cbd87ad5c90a9587403e430ec66a78795e761d17731af10506bf2efc6f877186d76b07e881ed162ae2eb1547f15052434b9b5df9e7769b10f4205b4907a70c31012f037b64ce4228c38fb29";
    const C1: &str = "46b9ab7c62969cf99632798a497c6b5f95d4c4465d304e77584368b4f5dfd02b4b6951025fd0cf0dd0fa0f715f2596ac78de2892f409518529bc23d3c7a60709cb27796d05bd67d6cac70032bd9dc9f3cd434a0845011d36fe5f5f7846ef3710099a69decdfd4a0a2792cf2d881c82aa57507a6b7f8f37d843f62876597a5ba297e14d0c89f906bd3ef9131db380c89a008844807405f1045844950ac16bf3f05288d2f775784d30cb4a143ade2fd048dd09615beac08b604d46015addc3496e98473a0d83eaad9b2d4345958500ab42221d5b5687abd00682d41e21d1000f126e0a736531bbb4274b332c7859b90f4a9c7bbef528de90acb051071b472d7a72";
    const N2: &str = "daffb2297631a992f0ce583505c6af0758d5563dab2cd31ee315128862c33a4fb774eb5248db40af72158370d269a9a5ae658f33fe3b890b93f448b3a5aa3c814f426dcbb394fb36bb2d420f0f88080b10a3d6b2aa05e11ab2715945795e8229451abd81f1d69ed617f5e837d70820fe119a72d174c9df6acc011cdd9474031b";
    const B2: &str = "daffb2297631a992f0ce583505c6af0758d5563dab2cd31ee315128862c33a4fb774eb5248db40af72158370d269a9a5ae658f33fe3b890b93f448b3a5aa3c814f426dcbb394fb36bb2d420f0f88080b10a3d6b2aa05e11ab2715945795e8229451abd81f1d69ed617f5e837d70820fe119a72d174c9df6acc011cdd94740319";
    const C2: &str = "daffb2297631a992f0ce583505c6af0758d5563dab2cd31ee315128862c33a4fb774eb5248db40af72158370d269a9a5ae658f33fe3b890b93f448b3a5aa3c814f426dcbb394fb36bb2d420f0f88080b10a3d6b2aa05e11ab2715945795e8229451abd81f1d69ed617f5e837d70820fe119a72d174c9df6acc011cdd94740313";

    fn hex(s: &str) -> Vec<u8> {
        let s = if s.len() % 2 == 1 { format!("0{s}") } else { s.to_string() };
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn pow_hex(b: &str, e: &[u8], n: &str) -> Vec<u8> {
        let n = Limbs::from_be(&hex(n));
        let k = n.0.len();
        let out = modpow(&Limbs::from_be_len(&hex(b), k), e, &n);
        out.to_be(k * 4).into_iter().skip_while(|b| *b == 0).collect()
    }

    #[test]
    fn modpow_matches_independent_known_answers() {
        assert_eq!(pow_hex(B1, &[1, 0, 1], N1), hex(C1), "2048-bit, e = 65537");
        assert_eq!(pow_hex(B2, &[3], N2), hex(C2), "1024-bit, e = 3, base = n - 2");
        // Textbook: 65^17 mod 3233 = 2790.
        let n = Limbs(vec![3233]);
        assert_eq!(modpow(&Limbs(vec![65]), &[17], &n), Limbs(vec![2790]));
    }

    #[test]
    fn base64_and_der_reject_garbage() {
        assert_eq!(base64_decode("TWFu").unwrap(), b"Man");
        assert_eq!(base64_decode("TWE=").unwrap(), b"Ma");
        assert!(base64_decode("T!==").is_err());
        assert!(PublicKey::from_pem(b"-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----").is_err());
        assert!(PublicKey::from_pem(b"no pem here").is_err());
    }

    #[test]
    fn oaep_layout_is_reversible_with_the_masks() {
        // Unmask the encoded message by hand (no private key needed when the
        // "encryption" is the identity: n so large that e = 1 is accepted).
        let key = PublicKey { n: vec![0xFF; 128], e: vec![1] };
        let seed = [7u8; 20];
        let em = key.encrypt_oaep_sha1(b"secret\0", seed).unwrap();
        assert_eq!(em.len(), 128);
        assert_eq!(em[0], 0);
        let (masked_seed, masked_db) = em[1..].split_at(20);
        let seed2: Vec<u8> = masked_seed.iter().zip(mgf1(masked_db, 20)).map(|(a, b)| a ^ b).collect();
        assert_eq!(seed2, seed);
        let db: Vec<u8> = masked_db.iter().zip(mgf1(&seed2, 107)).map(|(a, b)| a ^ b).collect();
        assert_eq!(&db[..20], &Sha1::digest([])[..]);
        assert!(db.ends_with(b"\x01secret\0"));
        assert!(key.encrypt_oaep_sha1(&[0; 100], seed).is_err(), "too long for the key");
    }
}
