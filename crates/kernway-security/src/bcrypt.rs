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

use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Blowfish state
// ---------------------------------------------------------------------------

const N_ROUNDS: usize = 16;
const P_LEN: usize = 18;

#[derive(Clone)]
struct Blowfish {
    p: [u32; P_LEN],
    s: [[u32; 256]; 4],
}

impl Blowfish {
    fn from_pi() -> Self {
        let (p, s) = pi_state();
        Blowfish { p: *p, s: *s }
    }

    #[inline]
    fn f(&self, x: u32) -> u32 {
        let a = (x >> 24) & 0xff;
        let b = (x >> 16) & 0xff;
        let c = (x >> 8) & 0xff;
        let d = x & 0xff;
        let mut y = self.s[0][a as usize].wrapping_add(self.s[1][b as usize]);
        y ^= self.s[2][c as usize];
        y = y.wrapping_add(self.s[3][d as usize]);
        y
    }

    /// Encrypt the 64-bit block `(l, r)` in place.
    fn encipher(&self, l: &mut u32, r: &mut u32) {
        let mut xl = *l;
        let mut xr = *r;
        for i in 0..N_ROUNDS {
            xl ^= self.p[i];
            xr ^= self.f(xl);
            std::mem::swap(&mut xl, &mut xr);
        }
        std::mem::swap(&mut xl, &mut xr);
        xr ^= self.p[N_ROUNDS];
        xl ^= self.p[N_ROUNDS + 1];
        *l = xl;
        *r = xr;
    }

    /// The `EksBlowfish` key schedule. `salt` is XORed into the chained block while the
    /// P-array and S-boxes are refilled; passing an all-zero salt gives the "expand0"
    /// variant used inside the cost loop. `key` bytes are cycled into the P-array.
    fn expand(&mut self, salt: &[u8; 16], key: &[u8]) {
        let mut kj = 0usize;
        for i in 0..P_LEN {
            self.p[i] ^= stream_word(key, &mut kj);
        }
        let mut sj = 0usize;
        let mut l = 0u32;
        let mut r = 0u32;
        for i in (0..P_LEN).step_by(2) {
            l ^= stream_word(salt, &mut sj);
            r ^= stream_word(salt, &mut sj);
            self.encipher(&mut l, &mut r);
            self.p[i] = l;
            self.p[i + 1] = r;
        }
        for si in 0..4 {
            for k in (0..256).step_by(2) {
                l ^= stream_word(salt, &mut sj);
                r ^= stream_word(salt, &mut sj);
                self.encipher(&mut l, &mut r);
                self.s[si][k] = l;
                self.s[si][k + 1] = r;
            }
        }
    }
}

/// Read four bytes, big-endian, from `data` starting at `*off`, wrapping modulo the
/// length; advance `*off` by four. Matches OpenBSD/jBCrypt `streamtoword` (each byte is
/// masked to 8 bits, so there is no sign-extension quirk).
#[inline]
fn stream_word(data: &[u8], off: &mut usize) -> u32 {
    let mut word = 0u32;
    let len = data.len();
    for _ in 0..4 {
        word = (word << 8) | u32::from(data[*off % len]);
        *off += 1;
    }
    word
}

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

// ---------------------------------------------------------------------------
// π seed for the Blowfish state, via exact integer arithmetic (Machin's formula)
// ---------------------------------------------------------------------------
//
// Blowfish's P-array (18 words) and S-boxes (4×256 words) are the first 1042 32-bit
// words of the fractional hex expansion of π. Rather than embed 1042 constants (easy to
// mistype, impossible to eyeball), compute them once: π = 16·atan(1/5) − 4·atan(1/239)
// scaled by 16^N as a big integer, then read the hex digits. The jBCrypt test vectors
// exercise every one of these words, so a wrong digit anywhere fails the tests.

fn pi_state() -> &'static ([u32; P_LEN], [[u32; 256]; 4]) {
    static STATE: OnceLock<([u32; P_LEN], [[u32; 256]; 4])> = OnceLock::new();
    STATE.get_or_init(compute_pi_state)
}

fn compute_pi_state() -> ([u32; P_LEN], [[u32; 256]; 4]) {
    // Words needed: 18 + 4*256 = 1042 → 8336 hex digits; add guard digits for exactness.
    const WORDS: usize = P_LEN + 4 * 256;
    const HEX_DIGITS: usize = WORDS * 8; // 8336
    const GUARD: usize = 48;
    let n = HEX_DIGITS + GUARD;

    // SCALE = 16^n = 2^(4n).
    let scale = pow2(4 * n);
    let a5 = atan_inv(5, &scale);
    let a239 = atan_inv(239, &scale);
    let pi = big_sub(&big_mul_small(&a5, 16), &big_mul_small(&a239, 4));

    // pi ≈ 3.243F6A88… × 16^n. Its hex string starts with '3'; the rest are the
    // fractional digits we want.
    let hex = big_to_hex(&pi);
    let frac: Vec<u8> = hex.bytes().skip(1).take(HEX_DIGITS).collect();
    debug_assert_eq!(frac.len(), HEX_DIGITS, "not enough pi digits");

    let mut words = [0u32; WORDS];
    for (w, chunk) in words.iter_mut().zip(frac.chunks(8)) {
        let mut v = 0u32;
        for &c in chunk {
            v = (v << 4) | u32::from(hex_val(c));
        }
        *w = v;
    }

    let mut p = [0u32; P_LEN];
    p.copy_from_slice(&words[..P_LEN]);
    let mut s = [[0u32; 256]; 4];
    for (bi, sbox) in s.iter_mut().enumerate() {
        let base = P_LEN + bi * 256;
        sbox.copy_from_slice(&words[base..base + 256]);
    }
    (p, s)
}

fn hex_val(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => 0,
    }
}

// --- minimal big integer: little-endian Vec<u32> limbs ---------------------

type Big = Vec<u32>;

fn big_norm(a: &mut Big) {
    while a.len() > 1 && *a.last().unwrap() == 0 {
        a.pop();
    }
}

fn big_is_zero(a: &Big) -> bool {
    a.iter().all(|&x| x == 0)
}

/// 2^bits as a big integer.
fn pow2(bits: usize) -> Big {
    let limbs = bits / 32;
    let rem = bits % 32;
    let mut v = vec![0u32; limbs + 1];
    v[limbs] = 1u32 << rem;
    v
}

fn big_mul_small(a: &Big, m: u32) -> Big {
    let mut out = Vec::with_capacity(a.len() + 1);
    let mut carry = 0u64;
    for &limb in a {
        let cur = u64::from(limb) * u64::from(m) + carry;
        out.push((cur & 0xffff_ffff) as u32);
        carry = cur >> 32;
    }
    while carry != 0 {
        out.push((carry & 0xffff_ffff) as u32);
        carry >>= 32;
    }
    if out.is_empty() {
        out.push(0);
    }
    out
}

/// Floor division by a small divisor.
fn big_div_small(a: &Big, d: u32) -> Big {
    let mut out = vec![0u32; a.len()];
    let mut rem = 0u64;
    for i in (0..a.len()).rev() {
        let cur = (rem << 32) | u64::from(a[i]);
        out[i] = (cur / u64::from(d)) as u32;
        rem = cur % u64::from(d);
    }
    big_norm(&mut out);
    out
}

fn big_add(a: &Big, b: &Big) -> Big {
    let n = a.len().max(b.len());
    let mut out = Vec::with_capacity(n + 1);
    let mut carry = 0u64;
    for i in 0..n {
        let x = u64::from(*a.get(i).unwrap_or(&0));
        let y = u64::from(*b.get(i).unwrap_or(&0));
        let cur = x + y + carry;
        out.push((cur & 0xffff_ffff) as u32);
        carry = cur >> 32;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    out
}

/// a − b, assuming a ≥ b.
fn big_sub(a: &Big, b: &Big) -> Big {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (i, &ai) in a.iter().enumerate() {
        let x = i64::from(ai);
        let y = i64::from(*b.get(i).unwrap_or(&0));
        let mut cur = x - y - borrow;
        if cur < 0 {
            cur += 1 << 32;
            borrow = 1;
        } else {
            borrow = 0;
        }
        out.push(cur as u32);
    }
    big_norm(&mut out);
    out
}

/// atan(1/x) · scale, as a big integer. Series: Σ (−1)^k / ((2k+1)·x^(2k+1)).
fn atan_inv(x: u32, scale: &Big) -> Big {
    let x2 = x * x; // ≤ 239² = 57121, fits u32
    let mut power = big_div_small(scale, x); // scale / x
    let mut total = vec![0u32];
    let mut k: u32 = 0;
    let mut add = true;
    while !big_is_zero(&power) {
        let term = big_div_small(&power, 2 * k + 1);
        total = if add {
            big_add(&total, &term)
        } else {
            big_sub(&total, &term)
        };
        power = big_div_small(&power, x2);
        k += 1;
        add = !add;
    }
    total
}

fn big_to_hex(a: &Big) -> String {
    let mut s = String::with_capacity(a.len() * 8);
    for &limb in a.iter().rev() {
        s.push_str(&format!("{limb:08x}"));
    }
    let trimmed = s.trim_start_matches('0');
    if trimmed.is_empty() {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
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



