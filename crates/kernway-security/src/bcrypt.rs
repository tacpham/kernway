//! BCrypt — verify and generate OpenBSD `$2a$/$2b$/$2y$` password hashes, byte-for-byte
//! compatible with jBCrypt / Spring `BCryptPasswordEncoder` (feature = `bcrypt`).
//!
//! Kernway's own password KDF is PBKDF2 ([`crate::password`], KEP-0000 §1). BCrypt is
//! here for one reason: **interop** — an existing store (e.g. a Spring app's user index)
//! holds `$2a$…` hashes, and logging those users in requires verifying BCrypt exactly.
//! So this is not a new "roll your own crypto" primitive; it is a faithful, tested
//! reimplementation of a fixed, published algorithm, gated behind a feature and proven
//! against the canonical jBCrypt test vectors (see the tests).
//!
//! The algorithm (Provos & Mazières, 1999): the Blowfish cipher whose P-array and
//! S-boxes are seeded from the fractional hex digits of π, an *expensive* key schedule
//! (`EksBlowfish`) run `2^cost` times over the salt and password, then 64 encryptions of
//! the constant `"OrpheanBeholderScryDoubt"` — the 23-byte result is the hash. The π
//! seed is computed here with exact integer arithmetic (Machin's formula), so there is
//! no giant embedded constant table to get wrong; the test vectors prove it end to end.


use kernway_wire::blowfish::Blowfish;

// ---------------------------------------------------------------------------
// bcrypt core
// ---------------------------------------------------------------------------

/// The 24-byte magic `"OrpheanBeholderScryDoubt"`, as six big-endian words.
const MAGIC: [u32; 6] = [
    0x4f72_7068, 0x6561_6e42, 0x6568_6f6c, 0x6465_7253, 0x6372_7944, 0x6f75_6274,
];

/// Run bcrypt: `key` (password bytes, already NUL-terminated by the caller), 16-byte
/// `salt`, log₂ cost `cost` → the raw 23-byte hash.
fn bcrypt_raw(cost: u32, salt: &[u8; 16], key: &[u8]) -> [u8; 23] {
    let mut state = Blowfish::from_pi();
    state.expand(salt, key);
    let zero = [0u8; 16];
    let rounds = 1u64 << cost;
    for _ in 0..rounds {
        state.expand(&zero, key);
        state.expand(&zero, salt);
    }

    let mut cdata = MAGIC;
    for _ in 0..64 {
        let mut i = 0;
        while i < 6 {
            let (mut l, mut r) = (cdata[i], cdata[i + 1]);
            state.encipher(&mut l, &mut r);
            cdata[i] = l;
            cdata[i + 1] = r;
            i += 2;
        }
    }

    // 24 bytes big-endian, but bcrypt keeps only the first 23.
    let mut out = [0u8; 23];
    for (idx, byte) in cdata.iter().flat_map(|w| w.to_be_bytes()).take(23).enumerate() {
        out[idx] = byte;
    }
    out
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Verify `password` against a `$2a$/$2b$/$2y$` bcrypt `hash`. Returns `false` for a
/// non-matching password or a malformed hash — never panics, never errors, so it is
/// safe to call straight on untrusted input on the login path. The comparison of the
/// recomputed hash is constant-time.
#[must_use]
pub fn verify_bcrypt(password: &str, hash: &str) -> bool {
    let Some((cost, salt, expected)) = parse_hash(hash) else {
        return false;
    };
    let key = key_bytes(password);
    let raw = bcrypt_raw(cost, &salt, &key);
    let got = encode_b64(&raw, 23);
    constant_time_eq(got.as_bytes(), expected.as_bytes())
}

/// Hash `password` with a fresh random 16-byte salt at the given `cost` (log₂ rounds;
/// 10–12 is typical). Emits a `$2b$` string compatible with jBCrypt/Spring. `cost` is
/// clamped to the valid 4..=31 range.
#[must_use]
pub fn hash_bcrypt(password: &str, cost: u32) -> String {
    let cost = cost.clamp(4, 31);
    let mut salt = [0u8; 16];
    getrandom::getrandom(&mut salt).expect("OS randomness unavailable");
    let key = key_bytes(password);
    let raw = bcrypt_raw(cost, &salt, &key);
    format!(
        "$2b${:02}${}{}",
        cost,
        encode_b64(&salt, 16),
        encode_b64(&raw, 23)
    )
}

/// Password → key bytes: UTF-8 plus a trailing NUL, exactly as jBCrypt/Spring build the
/// key for the `$2a/$2b/$2y` variants.
fn key_bytes(password: &str) -> Vec<u8> {
    let mut k = password.as_bytes().to_vec();
    k.push(0);
    k
}

/// Parse `$2[abxy]$<cost>$<22-char salt><31-char hash>` → (cost, 16-byte salt, the
/// 31-char hash text). `None` for anything malformed.
fn parse_hash(hash: &str) -> Option<(u32, [u8; 16], &str)> {
    let b = hash.as_bytes();
    if b.len() < 4 || b[0] != b'$' || b[1] != b'2' {
        return None;
    }
    // Optional minor version letter, then '$'.
    let mut i = 2;
    if b[i] != b'$' {
        // a / b / x / y
        i += 1;
    }
    if i >= b.len() || b[i] != b'$' {
        return None;
    }
    i += 1;
    // Two-digit cost.
    if i + 2 >= b.len() || b[i + 2] != b'$' {
        return None;
    }
    let cost = std::str::from_utf8(&b[i..i + 2]).ok()?.parse::<u32>().ok()?;
    if !(4..=31).contains(&cost) {
        return None;
    }
    let rest = &hash[i + 3..];
    if rest.len() < 53 {
        return None;
    }
    let salt_b64 = &rest[..22];
    let hash_b64 = &rest[22..53];
    let salt = decode_b64(salt_b64, 16)?;
    let mut salt16 = [0u8; 16];
    if salt.len() != 16 {
        return None;
    }
    salt16.copy_from_slice(&salt);
    Some((cost, salt16, hash_b64))
}

// ---------------------------------------------------------------------------
// bcrypt's custom base64 (alphabet "./A-Za-z0-9", MSB-first, no padding)
// ---------------------------------------------------------------------------

const B64: &[u8; 64] = b"./ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

fn char64(c: u8) -> Option<u8> {
    B64.iter().position(|&x| x == c).map(|p| p as u8)
}

/// Encode `len` bytes of `data` in bcrypt's base64. Matches jBCrypt `encode_base64`.
fn encode_b64(data: &[u8], len: usize) -> String {
    let mut out = String::new();
    let mut off = 0usize;
    while off < len {
        let c1 = data[off] as u32;
        off += 1;
        out.push(B64[((c1 >> 2) & 0x3f) as usize] as char);
        let mut c1 = (c1 & 0x03) << 4;
        if off >= len {
            out.push(B64[(c1 & 0x3f) as usize] as char);
            break;
        }
        let c2 = data[off] as u32;
        off += 1;
        c1 |= (c2 >> 4) & 0x0f;
        out.push(B64[(c1 & 0x3f) as usize] as char);
        let mut c1 = (c2 & 0x0f) << 2;
        if off >= len {
            out.push(B64[(c1 & 0x3f) as usize] as char);
            break;
        }
        let c3 = data[off] as u32;
        off += 1;
        c1 |= (c3 >> 6) & 0x03;
        out.push(B64[(c1 & 0x3f) as usize] as char);
        out.push(B64[(c3 & 0x3f) as usize] as char);
    }
    out
}

/// Decode up to `max` bytes from bcrypt base64. Matches jBCrypt `decode_base64`.
fn decode_b64(s: &str, max: usize) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(max);
    let mut off = 0usize;
    while off < b.len() && out.len() < max {
        let c1 = char64(b[off])?;
        off += 1;
        if off >= b.len() {
            break;
        }
        let c2 = char64(b[off])?;
        off += 1;
        out.push((c1 << 2) | ((c2 & 0x30) >> 4));
        if out.len() >= max || off >= b.len() {
            break;
        }
        let c3 = char64(b[off])?;
        off += 1;
        out.push(((c2 & 0x0f) << 4) | ((c3 & 0x3c) >> 2));
        if out.len() >= max || off >= b.len() {
            break;
        }
        let c4 = char64(b[off])?;
        off += 1;
        out.push(((c3 & 0x03) << 6) | c4);
    }
    Some(out)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    // Canonical jBCrypt vectors (the same encoder Spring's BCryptPasswordEncoder uses).
    // If any π word were wrong, these would fail — they are the end-to-end proof.
    pub(super) const VECTORS: &[(&str, &str)] = &[
        ("a", "$2a$06$m0CrhHm10qJ3lXRY.5zDGO3rS2KdeeWLuGmsfGlMfOxih58VYVfxe"),
        ("abc", "$2a$06$If6bvum7DFjUnE9p2uDeDu0YHzrHM6tf.iqN8.yx.jNN1ILEf7h0i"),
        (
            "abcdefghijklmnopqrstuvwxyz",
            "$2a$06$.rCVZVOThsIa97pEDOxvGuRRgzG64bvtJ0938xuqzv18d3ZpQhstC",
        ),
    ];

    #[test]
    fn verifies_canonical_jbcrypt_vectors() {
        for (pw, hash) in VECTORS {
            assert_eq!(hash.len(), 60, "vector for {pw:?} not 60 chars (transcription)");
            assert!(verify_bcrypt(pw, hash), "should verify {pw:?}");
        }
    }

    #[test]
    fn rejects_wrong_password() {
        assert!(!verify_bcrypt("wrong", VECTORS[2].1));
        assert!(!verify_bcrypt("abc", VECTORS[0].1));
    }

    #[test]
    fn rejects_malformed_hash() {
        assert!(!verify_bcrypt("x", ""));
        assert!(!verify_bcrypt("x", "$1$abc"));
        assert!(!verify_bcrypt("x", "not-a-hash"));
    }

    #[test]
    fn round_trips_own_hashes() {
        let h = hash_bcrypt("correct horse", 6);
        assert!(h.starts_with("$2b$06$"));
        assert!(verify_bcrypt("correct horse", &h));
        assert!(!verify_bcrypt("battery staple", &h));
        // Higher cost round-trips too (exercises the expensive key schedule).
        let h12 = hash_bcrypt("correct horse", 10);
        assert!(verify_bcrypt("correct horse", &h12));
    }
}



