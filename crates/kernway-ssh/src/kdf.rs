//! What opening a passphrase-protected OpenSSH key takes: `bcrypt_pbkdf` (OpenBSD's
//! KDF, over the shared EksBlowfish) and AES in counter mode (FIPS-197, SP 800-38A).
//!
//! `ring` has AES only inside AEAD constructions, so the block cipher is here. It
//! runs on a key file read from local disk, a few hundred bytes once per load — not
//! on network data — so the plain table-free byte implementation is enough.

use kernway_wire::blowfish::Blowfish;
use ring::digest;

// ── AES ──────────────────────────────────────────────────────────────────────

/// The AES S-box, built from its definition (the GF(2^8) inverse, then the affine
/// map) rather than typed in.
fn sbox() -> &'static [u8; 256] {
    static S: std::sync::OnceLock<[u8; 256]> = std::sync::OnceLock::new();
    S.get_or_init(|| {
        let mut s = [0u8; 256];
        for (i, out) in s.iter_mut().enumerate() {
            let inv = if i == 0 { 0 } else { (1..=255u8).find(|&b| gmul(i as u8, b) == 1).expect("an inverse") };
            let mut x = inv;
            let mut r = inv;
            for _ in 0..4 {
                x = x.rotate_left(1);
                r ^= x;
            }
            *out = r ^ 0x63;
        }
        s
    })
}

fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    p
}

/// An expanded AES key (128, 192 or 256 bits).
pub(crate) struct Aes {
    round_keys: Vec<[u8; 16]>,
}

impl Aes {
    pub(crate) fn new(key: &[u8]) -> Self {
        assert!(matches!(key.len(), 16 | 24 | 32), "an AES key is 16, 24 or 32 bytes");
        let s = sbox();
        let nk = key.len() / 4;
        let nr = nk + 6;
        let mut w: Vec<[u8; 4]> = key.chunks(4).map(|c| [c[0], c[1], c[2], c[3]]).collect();
        let mut rcon = 1u8;
        for i in nk..4 * (nr + 1) {
            let mut t = w[i - 1];
            if i % nk == 0 {
                t = [s[t[1] as usize] ^ rcon, s[t[2] as usize], s[t[3] as usize], s[t[0] as usize]];
                rcon = gmul(rcon, 2);
            } else if nk > 6 && i % nk == 4 {
                t = t.map(|b| s[b as usize]);
            }
            let prev = w[i - nk];
            w.push([prev[0] ^ t[0], prev[1] ^ t[1], prev[2] ^ t[2], prev[3] ^ t[3]]);
        }
        let round_keys = w
            .chunks(4)
            .map(|c| {
                let mut k = [0u8; 16];
                for (j, word) in c.iter().enumerate() {
                    k[4 * j..4 * j + 4].copy_from_slice(word);
                }
                k
            })
            .collect();
        Aes { round_keys }
    }

    /// Encrypt one 16-byte block in place.
    pub(crate) fn encrypt_block(&self, b: &mut [u8; 16]) {
        let s = sbox();
        let add = |b: &mut [u8; 16], k: &[u8; 16]| b.iter_mut().zip(k).for_each(|(x, y)| *x ^= y);
        let nr = self.round_keys.len() - 1;
        add(b, &self.round_keys[0]);
        for (round, key) in self.round_keys.iter().enumerate().skip(1) {
            b.iter_mut().for_each(|x| *x = s[*x as usize]);
            // ShiftRows: byte (row r, column c) sits at 4c + r.
            let t = *b;
            for c in 0..4 {
                for r in 0..4 {
                    b[4 * c + r] = t[4 * ((c + r) % 4) + r];
                }
            }
            if round != nr {
                for c in 0..4 {
                    let a = [b[4 * c], b[4 * c + 1], b[4 * c + 2], b[4 * c + 3]];
                    b[4 * c] = gmul(a[0], 2) ^ gmul(a[1], 3) ^ a[2] ^ a[3];
                    b[4 * c + 1] = a[0] ^ gmul(a[1], 2) ^ gmul(a[2], 3) ^ a[3];
                    b[4 * c + 2] = a[0] ^ a[1] ^ gmul(a[2], 2) ^ gmul(a[3], 3);
                    b[4 * c + 3] = gmul(a[0], 3) ^ a[1] ^ a[2] ^ gmul(a[3], 2);
                }
            }
            add(b, key);
        }
    }

    /// CTR mode (SP 800-38A §6.5) with a 128-bit big-endian counter starting at `iv`:
    /// encryption and decryption are the same operation.
    pub(crate) fn ctr(&self, iv: &[u8; 16], data: &mut [u8]) {
        let mut counter = u128::from_be_bytes(*iv);
        for chunk in data.chunks_mut(16) {
            let mut ks = counter.to_be_bytes();
            self.encrypt_block(&mut ks);
            chunk.iter_mut().zip(ks).for_each(|(x, k)| *x ^= k);
            counter = counter.wrapping_add(1);
        }
    }
}

// ── bcrypt_pbkdf ─────────────────────────────────────────────────────────────

/// OpenBSD's `bcrypt_hash`: EksBlowfish over the SHA-512 of the passphrase and salt,
/// then 64 encryptions of "OxychromaticBlowfishSwatDynamite".
fn bcrypt_hash(sha2pass: &[u8], sha2salt: &[u8]) -> [u8; 32] {
    let mut state = Blowfish::from_pi();
    state.expand(sha2salt, sha2pass);
    for _ in 0..64 {
        state.expand0(sha2salt);
        state.expand0(sha2pass);
    }
    let magic = b"OxychromaticBlowfishSwatDynamite";
    let mut cdata = [0u32; 8];
    let mut off = 0;
    for w in &mut cdata {
        *w = kernway_wire::blowfish::stream_word(magic, &mut off);
    }
    for _ in 0..64 {
        for pair in cdata.chunks_mut(2) {
            let (l, r) = pair.split_at_mut(1);
            state.encipher(&mut l[0], &mut r[0]);
        }
    }
    let mut out = [0u8; 32];
    for (i, w) in cdata.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&w.to_le_bytes());
    }
    out
}

/// `bcrypt_pbkdf(passphrase, salt, rounds)` → `out.len()` bytes (OpenBSD libutil),
/// the KDF of `openssh-key-v1` files.
pub(crate) fn bcrypt_pbkdf(pass: &[u8], salt: &[u8], rounds: u32, out: &mut [u8]) {
    let sha2pass = digest::digest(&digest::SHA512, pass);
    let stride = out.len().div_ceil(32);
    let amount = out.len().div_ceil(stride);
    let mut count = 1u32;
    let mut left = out.len();
    while left > 0 {
        let mut countsalt = salt.to_vec();
        countsalt.extend_from_slice(&count.to_be_bytes());
        let sha2salt = digest::digest(&digest::SHA512, &countsalt);
        let mut tmp = bcrypt_hash(sha2pass.as_ref(), sha2salt.as_ref());
        let mut acc = tmp;
        for _ in 1..rounds {
            let s = digest::digest(&digest::SHA512, &tmp);
            tmp = bcrypt_hash(sha2pass.as_ref(), s.as_ref());
            acc.iter_mut().zip(tmp).for_each(|(a, t)| *a ^= t);
        }
        let take = amount.min(left);
        for (i, &byte) in acc.iter().enumerate().take(take) {
            let dest = i * stride + (count as usize - 1);
            if dest >= out.len() {
                break;
            }
            out[dest] = byte;
        }
        left -= take;
        count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn aes_matches_fips_197() {
        let pt: [u8; 16] = hex("00112233445566778899aabbccddeeff").try_into().unwrap();
        for (key, ct) in [
            ("000102030405060708090a0b0c0d0e0f", "69c4e0d86a7b0430d8cdb78070b4c55a"),
            ("000102030405060708090a0b0c0d0e0f1011121314151617", "dda97ca4864cdfe06eaf70a0ec0d7191"),
            ("000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f", "8ea2b7ca516745bfeafc49904b496089"),
        ] {
            let mut b = pt;
            Aes::new(&hex(key)).encrypt_block(&mut b);
            assert_eq!(b.to_vec(), hex(ct), "FIPS-197 appendix C, {}-bit", key.len() * 4);
        }
        assert_eq!(sbox()[0x00], 0x63);
        assert_eq!(sbox()[0x53], 0xed);
    }

    #[test]
    fn ctr_matches_sp_800_38a() {
        // F.5.5 CTR-AES256.Encrypt, all four blocks.
        let key = hex("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4");
        let iv: [u8; 16] = hex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff").try_into().unwrap();
        let mut data = hex("6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e5130c81c46a35ce411e5fbc1191a0a52eff69f2445df4f9b17ad2b417be66c3710");
        Aes::new(&key).ctr(&iv, &mut data);
        assert_eq!(data, hex("601ec313775789a5b7a7f504bbf3d228f443e3ca4d62b59aca84e990cacaf5c52b0930daa23de94ce87017ba2d84988ddfc9c58db67aada613c2dd08457941a6"));
        // The counter carries across all 128 bits.
        let mut z = [0u8; 32];
        Aes::new(&key).ctr(&[0xff; 16], &mut z);
        let mut first = [0xffu8; 16];
        Aes::new(&key).encrypt_block(&mut first);
        let mut second = [0u8; 16];
        Aes::new(&key).encrypt_block(&mut second);
        assert_eq!((&z[..16], &z[16..]), (&first[..], &second[..]));
    }

    #[test]
    fn bcrypt_pbkdf_is_deterministic_and_spreads_bytes() {
        // Interop is proven against keys ssh-keygen encrypted (the live tests); here,
        // the shape: deterministic, salt- and round-sensitive, every byte filled.
        let mut a = [0u8; 48];
        let mut b = [0u8; 48];
        bcrypt_pbkdf(b"password", b"salt", 4, &mut a);
        bcrypt_pbkdf(b"password", b"salt", 4, &mut b);
        assert_eq!(a, b);
        let mut c = [0u8; 48];
        bcrypt_pbkdf(b"password", b"salu", 4, &mut c);
        assert_ne!(a, c);
        bcrypt_pbkdf(b"password", b"salt", 5, &mut c);
        assert_ne!(a, c);
        assert!(a.iter().filter(|&&x| x == 0).count() < 4);
    }
}
