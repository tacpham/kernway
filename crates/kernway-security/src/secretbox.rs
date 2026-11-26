//! Field-level encryption for secrets at rest (feature = `aead`). AES-256-GCM via the audited
//! `ring` crate — never hand-rolled. `seal` returns a hex string `nonce(12) || ciphertext || tag`;
//! `open` reverses it. A random 96-bit nonce per call (OS CSPRNG via `getrandom`) means the same
//! plaintext encrypts to different ciphertext each time, so stored values are opaque to a casual
//! DB reader, and nonce-reuse (catastrophic for GCM) is avoided without per-key counters.
//!
//! The 32-byte key is a master/data-encryption key the caller holds (from a secret manager or
//! config), NOT derived from the data. Authenticated encryption (GCM, 128-bit tag) means a
//! tampered or wrong-key ciphertext fails to `open` rather than returning garbage.
//!
//! `aad` (additional authenticated data) binds the ciphertext to its context — pass a stable
//! label such as `b"location:42:stripe_secret"`. It is authenticated but NOT stored in the
//! output, so `open` must supply the same `aad`. This stops a ciphertext from being lifted out of
//! one row/field and replayed into another (a wrong `aad` fails authentication).

use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};

/// Encrypt `plaintext` under the 32-byte `key`, authenticating `aad` (use `b""` for none).
/// Returns a hex string, or `None` if the system RNG or the cipher fails (both effectively never).
pub fn seal(key: &[u8; 32], aad: &[u8], plaintext: &[u8]) -> Option<String> {
    let unbound = UnboundKey::new(&AES_256_GCM, key).ok()?;
    let sealing = LessSafeKey::new(unbound);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce_bytes).ok()?;
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = plaintext.to_vec();
    sealing.seal_in_place_append_tag(nonce, Aad::from(aad), &mut in_out).ok()?;
    let mut out = Vec::with_capacity(NONCE_LEN + in_out.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&in_out);
    Some(to_hex(&out))
}

/// Decrypt a hex string produced by [`seal`] under the same `key` and `aad`. Returns `None` on
/// malformed input, a wrong key, a wrong `aad`, or a tampered ciphertext (authentication failure).
pub fn open(key: &[u8; 32], aad: &[u8], hex: &str) -> Option<Vec<u8>> {
    let bytes = from_hex(hex)?;
    if bytes.len() < NONCE_LEN + AES_256_GCM.tag_len() {
        return None;
    }
    let unbound = UnboundKey::new(&AES_256_GCM, key).ok()?;
    let opening = LessSafeKey::new(unbound);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    nonce_bytes.copy_from_slice(&bytes[..NONCE_LEN]);
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = bytes[NONCE_LEN..].to_vec();
    let plain = opening.open_in_place(nonce, Aad::from(aad), &mut in_out).ok()?;
    Some(plain.to_vec())
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let val = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in s.chunks(2) {
        out.push((val(pair[0])? << 4) | val(pair[1])?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AAD: &[u8] = b"location:42:stripe_secret";

    #[test]
    fn round_trips() {
        let key = [7u8; 32];
        let ct = seal(&key, AAD, b"sk_live_secret").unwrap();
        assert_ne!(ct, "sk_live_secret"); // opaque — not the plaintext
        assert!(!ct.contains("sk_live")); // plaintext does not leak into the hex
        assert_eq!(open(&key, AAD, &ct).unwrap(), b"sk_live_secret");
    }

    #[test]
    fn nonce_makes_ciphertext_unique() {
        let key = [1u8; 32];
        assert_ne!(seal(&key, AAD, b"same").unwrap(), seal(&key, AAD, b"same").unwrap());
    }

    #[test]
    fn wrong_key_fails() {
        let ct = seal(&[1u8; 32], AAD, b"data").unwrap();
        assert!(open(&[2u8; 32], AAD, &ct).is_none());
    }

    #[test]
    fn wrong_aad_fails() {
        // A ciphertext bound to one context cannot be opened under another (no row/field swap).
        let key = [9u8; 32];
        let ct = seal(&key, b"location:1:secret", b"data").unwrap();
        assert!(open(&key, b"location:2:secret", &ct).is_none());
        assert_eq!(open(&key, b"location:1:secret", &ct).unwrap(), b"data");
    }

    #[test]
    fn tamper_fails() {
        let key = [3u8; 32];
        let ct = seal(&key, AAD, b"data").unwrap();
        // Flip every byte position in turn — none must authenticate.
        for i in 0..ct.len() {
            let mut bad: Vec<char> = ct.chars().collect();
            bad[i] = if bad[i] == 'a' { 'b' } else { 'a' };
            let s: String = bad.into_iter().collect();
            assert!(open(&key, AAD, &s).is_none(), "tamper at {i} opened");
        }
    }

    #[test]
    fn empty_and_large_round_trip() {
        let key = [5u8; 32];
        assert_eq!(open(&key, AAD, &seal(&key, AAD, b"").unwrap()).unwrap(), b"");
        let big = vec![0xABu8; 100_000];
        assert_eq!(open(&key, AAD, &seal(&key, AAD, &big).unwrap()).unwrap(), big);
    }

    #[test]
    fn malformed_input_fails() {
        let key = [4u8; 32];
        assert!(open(&key, AAD, "").is_none()); // too short
        assert!(open(&key, AAD, "zz").is_none()); // not hex
        assert!(open(&key, AAD, "abc").is_none()); // odd length
        assert!(open(&key, AAD, &"ab".repeat(13)).is_none()); // 13 bytes < nonce+tag
    }

    #[test]
    fn truncated_tag_fails() {
        let key = [6u8; 32];
        let ct = seal(&key, AAD, b"secret").unwrap();
        assert!(open(&key, AAD, &ct[..ct.len() - 2]).is_none()); // drop a tag byte
    }
}
