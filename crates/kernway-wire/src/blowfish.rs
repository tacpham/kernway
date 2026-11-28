//! Blowfish (Schneier, 1993) with the `EksBlowfish` key schedule — shared by bcrypt
//! password hashing (kernway-security) and `bcrypt_pbkdf`, the key derivation of
//! passphrase-protected OpenSSH keys (kernway-ssh). The π-derived initial state is
//! computed once with exact integer arithmetic; both users' published test vectors
//! cover every word of it.

use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Blowfish state
// ---------------------------------------------------------------------------

const N_ROUNDS: usize = 16;
const P_LEN: usize = 18;

/// A Blowfish cipher state (P-array and S-boxes).
#[derive(Clone)]
pub struct Blowfish {
    p: [u32; P_LEN],
    s: [[u32; 256]; 4],
}

impl Blowfish {
    /// The initial state, seeded from the digits of π.
    #[must_use]
    pub fn from_pi() -> Self {
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

    /// The plain Blowfish key schedule (no salt) — `expand0state` in OpenBSD.
    pub fn expand0(&mut self, key: &[u8]) {
        self.expand(&[0u8; 8], key);
    }

    /// Encrypt the 64-bit block `(l, r)` in place.
    pub fn encipher(&self, l: &mut u32, r: &mut u32) {
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
    pub fn expand(&mut self, salt: &[u8], key: &[u8]) {
        let mut kj = 0usize;
        for p in &mut self.p {
            *p ^= stream_word(key, &mut kj);
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
pub fn stream_word(data: &[u8], off: &mut usize) -> u32 {
    let mut word = 0u32;
    let len = data.len();
    for _ in 0..4 {
        word = (word << 8) | u32::from(data[*off % len]);
        *off += 1;
    }
    word
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
