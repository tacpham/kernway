//! Ed25519 signing (RFC 8032) for MariaDB's `ed25519` and `parsec`
//! authentication plugins, plus the PBKDF2-HMAC-SHA512 that `parsec` derives
//! its key with. SHA-512 comes from `sha2`; the curve arithmetic is ours.
//!
//! [`sign`] takes the secret as bytes of any length and expands it with
//! SHA-512, exactly as RFC 8032 does with its 32-byte seed. MariaDB's
//! `ed25519` plugin feeds it the password itself (any length); `parsec` feeds
//! it a 32-byte PBKDF2 output, so both plugins share it.
//!
//! Only the client signs: the secret is the user's password, so the scalar
//! multiplication runs the same steps whatever its bits (conditional
//! selects, no secret-dependent branches).

use sha2::{Digest, Sha512};

// --- GF(2^255 - 19), five 51-bit limbs ---------------------------------------

#[derive(Clone, Copy)]
struct Fe([u64; 5]);

const MASK: u64 = (1 << 51) - 1;

impl Fe {
    const ZERO: Fe = Fe([0; 5]);
    const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    fn small(n: u64) -> Fe {
        Fe([n, 0, 0, 0, 0])
    }

    /// Carry each limb into the next (the top one wraps around times 19).
    fn carry(mut self) -> Fe {
        let h = &mut self.0;
        for i in 0..4 {
            h[i + 1] += h[i] >> 51;
            h[i] &= MASK;
        }
        h[0] += 19 * (h[4] >> 51);
        h[4] &= MASK;
        self
    }

    /// The canonical 32-byte little-endian encoding (fully reduced mod p).
    fn to_bytes(self) -> [u8; 32] {
        let mut h = self.carry().carry().0;
        // h < 2^255 + small: subtract p once if h >= p.
        let mut q = (h[0] + 19) >> 51;
        for x in &h[1..] {
            q = (x + q) >> 51;
        }
        h[0] += 19 * q;
        for i in 0..4 {
            h[i + 1] += h[i] >> 51;
            h[i] &= MASK;
        }
        h[4] &= MASK;
        let words = [h[0] | h[1] << 51, h[1] >> 13 | h[2] << 38, h[2] >> 26 | h[3] << 25, h[3] >> 39 | h[4] << 12];
        let mut out = [0u8; 32];
        for (i, w) in words.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    fn add(self, o: Fe) -> Fe {
        let (a, b) = (self.0, o.0);
        Fe([a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3], a[4] + b[4]]).carry()
    }

    fn sub(self, o: Fe) -> Fe {
        // + 4p keeps every limb positive (inputs are carried: limbs < 2^52).
        let (a, b) = (self.0, o.0);
        const P4_0: u64 = (MASK - 18) * 4;
        const P4_N: u64 = MASK * 4;
        Fe([a[0] + P4_0 - b[0], a[1] + P4_N - b[1], a[2] + P4_N - b[2], a[3] + P4_N - b[3], a[4] + P4_N - b[4]]).carry()
    }

    fn neg(self) -> Fe {
        Fe::ZERO.sub(self)
    }

    fn mul(self, o: Fe) -> Fe {
        let a = self.0.map(u128::from);
        let b = o.0.map(u128::from);
        let r0 = a[0] * b[0] + 19 * (a[1] * b[4] + a[2] * b[3] + a[3] * b[2] + a[4] * b[1]);
        let mut r1 = a[0] * b[1] + a[1] * b[0] + 19 * (a[2] * b[4] + a[3] * b[3] + a[4] * b[2]);
        let mut r2 = a[0] * b[2] + a[1] * b[1] + a[2] * b[0] + 19 * (a[3] * b[4] + a[4] * b[3]);
        let mut r3 = a[0] * b[3] + a[1] * b[2] + a[2] * b[1] + a[3] * b[0] + 19 * (a[4] * b[4]);
        let mut r4 = a[0] * b[4] + a[1] * b[3] + a[2] * b[2] + a[3] * b[1] + a[4] * b[0];
        let m = u128::from(MASK);
        r1 += r0 >> 51;
        r2 += r1 >> 51;
        r3 += r2 >> 51;
        r4 += r3 >> 51;
        let h0 = (r0 & m) + 19 * (r4 >> 51);
        let h1 = (r1 & m) + (h0 >> 51);
        Fe([(h0 & m) as u64, h1 as u64, (r2 & m) as u64, (r3 & m) as u64, (r4 & m) as u64])
    }

    fn square(self) -> Fe {
        self.mul(self)
    }

    /// `self ^ e` for a little-endian exponent (public exponents only).
    fn pow(self, e: &[u8; 32]) -> Fe {
        let mut r = Fe::ONE;
        for i in (0..256).rev() {
            r = r.square();
            if e[i / 8] >> (i % 8) & 1 == 1 {
                r = r.mul(self);
            }
        }
        r
    }

    fn invert(self) -> Fe {
        // p - 2 = 2^255 - 21
        let mut e = [0xff; 32];
        e[0] = 0xeb;
        e[31] = 0x7f;
        self.pow(&e)
    }

    fn eq(self, o: Fe) -> bool {
        self.to_bytes() == o.to_bytes()
    }

    fn is_odd(self) -> bool {
        self.to_bytes()[0] & 1 == 1
    }

    /// `a` if `pick` is 0, `b` if it is 1 — without branching on it.
    fn select(a: Fe, b: Fe, pick: u64) -> Fe {
        let m = pick.wrapping_neg();
        Fe(std::array::from_fn(|i| a.0[i] ^ (m & (a.0[i] ^ b.0[i]))))
    }
}

// --- The curve: -x^2 + y^2 = 1 + d x^2 y^2, extended coordinates ---------------

#[derive(Clone, Copy)]
struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

struct Curve {
    d2: Fe,
    base: Point,
}

impl Curve {
    fn new() -> Curve {
        let d = Fe::small(121_665).neg().mul(Fe::small(121_666).invert());
        // The base point: y = 4/5, x even.
        let y = Fe::small(4).mul(Fe::small(5).invert());
        let x = recover_x(d, y, false).expect("the base point is on the curve");
        Curve { d2: d.add(d), base: Point { x, y, z: Fe::ONE, t: x.mul(y) } }
    }

    fn identity() -> Point {
        Point { x: Fe::ZERO, y: Fe::ONE, z: Fe::ONE, t: Fe::ZERO }
    }

    /// The complete addition law (RFC 8032 §5.1.4); also doubles.
    fn add(&self, p: Point, q: Point) -> Point {
        let a = p.y.sub(p.x).mul(q.y.sub(q.x));
        let b = p.y.add(p.x).mul(q.y.add(q.x));
        let c = p.t.mul(self.d2).mul(q.t);
        let d = p.z.add(p.z).mul(q.z);
        let (e, f, g, h) = (b.sub(a), d.sub(c), d.add(c), b.add(a));
        Point { x: e.mul(f), y: g.mul(h), z: f.mul(g), t: e.mul(h) }
    }

    /// `k · B` for a 256-bit little-endian scalar, the same steps for every k.
    fn mul_base(&self, k: &[u8; 32]) -> Point {
        let mut q = Curve::identity();
        for i in (0..256).rev() {
            q = self.add(q, q);
            let r = self.add(q, self.base);
            let bit = u64::from(k[i / 8] >> (i % 8) & 1);
            q = Point {
                x: Fe::select(q.x, r.x, bit),
                y: Fe::select(q.y, r.y, bit),
                z: Fe::select(q.z, r.z, bit),
                t: Fe::select(q.t, r.t, bit),
            };
        }
        q
    }
}

fn encode(p: Point) -> [u8; 32] {
    let zi = p.z.invert();
    let (x, y) = (p.x.mul(zi), p.y.mul(zi));
    let mut out = y.to_bytes();
    out[31] |= u8::from(x.is_odd()) << 7;
    out
}

/// x from y and its sign (RFC 8032 §5.1.3).
fn recover_x(d: Fe, y: Fe, odd: bool) -> Option<Fe> {
    let y2 = y.square();
    let u = y2.sub(Fe::ONE);
    let v = d.mul(y2).add(Fe::ONE);
    let v3 = v.square().mul(v);
    let v7 = v3.square().mul(v);
    // (p - 5) / 8 = 2^252 - 3
    let mut e = [0xff; 32];
    e[0] = 0xfd;
    e[31] = 0x0f;
    let mut x = u.mul(v3).mul(u.mul(v7).pow(&e));
    let vx2 = v.mul(x.square());
    if vx2.eq(u.neg()) {
        // sqrt(-1) = 2^((p - 1) / 4), (p - 1) / 4 = 2^253 - 5
        let mut e = [0xff; 32];
        e[0] = 0xfb;
        e[31] = 0x1f;
        x = x.mul(Fe::small(2).pow(&e));
    } else if !vx2.eq(u) {
        return None;
    }
    if x.is_odd() != odd {
        x = x.neg();
    }
    Some(x)
}

// --- Scalars mod l = 2^252 + 27742317777372353535851937790883648493 ------------

const L: [u64; 4] = [0x5812_631a_5cf5_d3ed, 0x14de_f9de_a2f7_9cd6, 0, 0x1000_0000_0000_0000];

/// A little-endian number of up to 512 bits, reduced mod l.
fn reduce(n: &[u64; 8]) -> [u8; 32] {
    let mut r = [0u64; 4];
    for i in (0..512).rev() {
        // r = 2r + bit (r < l < 2^253, so it fits)
        let mut carry = n[i / 64] >> (i % 64) & 1;
        for limb in &mut r {
            let next = *limb >> 63;
            *limb = *limb << 1 | carry;
            carry = next;
        }
        // r -= l when r >= l, chosen without branching.
        let mut diff = [0u64; 4];
        let mut borrow = 0u64;
        for j in 0..4 {
            let (d1, b1) = r[j].overflowing_sub(L[j]);
            let (d2, b2) = d1.overflowing_sub(borrow);
            diff[j] = d2;
            borrow = u64::from(b1 | b2);
        }
        let keep = borrow.wrapping_neg(); // all ones when r < l
        for j in 0..4 {
            r[j] = (r[j] & keep) | (diff[j] & !keep);
        }
    }
    let mut out = [0u8; 32];
    for (j, w) in r.iter().enumerate() {
        out[j * 8..j * 8 + 8].copy_from_slice(&w.to_le_bytes());
    }
    out
}

fn words<const N: usize>(b: &[u8]) -> [u64; N] {
    std::array::from_fn(|i| b.get(i * 8..i * 8 + 8).map_or(0, |s| u64::from_le_bytes(s.try_into().unwrap())))
}

/// `(a · b + c) mod l` for 256-bit little-endian a, b, c.
fn mul_add(a: &[u8; 32], b: &[u8; 32], c: &[u8; 32]) -> [u8; 32] {
    let (a, b, c): ([u64; 4], [u64; 4], [u64; 4]) = (words(a), words(b), words(c));
    let mut n = [0u128; 9];
    for i in 0..4 {
        for j in 0..4 {
            let p = u128::from(a[i]) * u128::from(b[j]);
            n[i + j] += p & u128::from(u64::MAX);
            n[i + j + 1] += p >> 64;
        }
        n[i] += u128::from(c[i]);
    }
    let mut out = [0u64; 8];
    let mut carry = 0u128;
    for i in 0..8 {
        let v = n[i] + carry;
        out[i] = v as u64;
        carry = v >> 64;
    }
    reduce(&out)
}

fn hash(parts: &[&[u8]]) -> [u64; 8] {
    let mut h = Sha512::new();
    for p in parts {
        h.update(p);
    }
    words(&h.finalize())
}

/// The secret scalar `a` (clamped) and the nonce prefix from `secret`.
fn expand(secret: &[u8]) -> ([u8; 32], [u8; 32]) {
    let h = Sha512::digest(secret);
    let mut a: [u8; 32] = h[..32].try_into().unwrap();
    a[0] &= 248;
    a[31] &= 63;
    a[31] |= 64;
    (a, h[32..].try_into().unwrap())
}

/// The public key of `secret` (RFC 8032: a 32-byte seed; MariaDB's `ed25519`
/// plugin: the password). The server keeps it; the client only signs.
#[cfg(test)]
fn public_key(secret: &[u8]) -> [u8; 32] {
    let (a, _) = expand(secret);
    encode(Curve::new().mul_base(&a))
}

/// Sign `msg` with `secret` (see [`public_key`]): `R || S`.
pub(crate) fn sign(secret: &[u8], msg: &[u8]) -> [u8; 64] {
    let curve = Curve::new();
    let (a, prefix) = expand(secret);
    let pub_key = encode(curve.mul_base(&a));
    let r = reduce(&hash(&[&prefix, msg]));
    let big_r = encode(curve.mul_base(&r));
    let k = reduce(&hash(&[&big_r, &pub_key, msg]));
    let s = mul_add(&k, &a, &r);
    let mut sig = [0u8; 64];
    sig[..32].copy_from_slice(&big_r);
    sig[32..].copy_from_slice(&s);
    sig
}

// --- PBKDF2-HMAC-SHA512 ---------------------------------------------------------

fn hmac_sha512(key: &[u8], parts: &[&[u8]]) -> [u8; 64] {
    let mut k = [0u8; 128];
    if key.len() > 128 {
        k[..64].copy_from_slice(&Sha512::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha512::new();
    inner.update(k.map(|b| b ^ 0x36));
    for p in parts {
        inner.update(p);
    }
    let mut outer = Sha512::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// PBKDF2 with HMAC-SHA512, `len` ≤ 64 bytes (one block).
pub(crate) fn pbkdf2_sha512(password: &[u8], salt: &[u8], iterations: u32, len: usize) -> Vec<u8> {
    let mut u = hmac_sha512(password, &[salt, &1u32.to_be_bytes()]);
    let mut t = u;
    for _ in 1..iterations {
        u = hmac_sha512(password, &[&u]);
        for (x, y) in t.iter_mut().zip(u) {
            *x ^= y;
        }
    }
    t[..len.min(64)].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn rfc8032_test_vectors() {
        // RFC 8032 §7.1, TEST 1 / 2 / 3: (secret, public, message, signature).
        let cases = [
            (
                "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
                "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
                "",
                "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
            ),
            (
                "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
                "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
                "72",
                "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
            ),
            (
                "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
                "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
                "af82",
                "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
            ),
        ];
        for (secret, public, msg, sig) in cases {
            let secret = unhex(secret);
            assert_eq!(public_key(&secret).to_vec(), unhex(public));
            assert_eq!(sign(&secret, &unhex(msg)).to_vec(), unhex(sig));
        }
    }

    #[test]
    fn scalars_reduce_mod_l() {
        // l itself reduces to 0; l - 1 stays; 2^512 - 1 matches the known residue.
        let l: Vec<u8> = L.iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut n = [0u64; 8];
        n[..4].copy_from_slice(&L);
        assert_eq!(reduce(&n), [0; 32]);
        n[0] -= 1;
        let mut lm1 = l.clone();
        lm1[0] -= 1;
        assert_eq!(reduce(&n).to_vec(), lm1);
        // 1 · 1 + 2 = 3
        let (mut one, mut two) = ([0u8; 32], [0u8; 32]);
        one[0] = 1;
        two[0] = 2;
        assert_eq!(mul_add(&one, &one, &two)[0], 3);
    }

    #[test]
    fn pbkdf2_hmac_sha512_vectors() {
        // RFC 6070's inputs with SHA-512 (as published for PBKDF2-HMAC-SHA512).
        assert_eq!(
            pbkdf2_sha512(b"password", b"salt", 1, 64),
            unhex("867f70cf1ade02cff3752599a3a53dc4af34c7a669815ae5d513554e1c8cf252c02d470a285a0501bad999bfe943c08f050235d7d68b1da55e63f73b60a57fce")
        );
        assert_eq!(
            pbkdf2_sha512(b"password", b"salt", 2, 64),
            unhex("e1d9c16aa681708a45f5c7c4e215ceb66e011a2e9f0040713f18aefdb866d53cf76cab2868a39b9f7840edce4fef5a82be67335c77a6068e04112754f27ccf4e")
        );
    }
}
