//! Password hashing — PBKDF2-HMAC-SHA256 (RFC 8018), built from our own HMAC.
//!
//! Storing a password as a bare hash (`sha256(pw)`) is broken: it is fast to brute-
//! force and unsalted, so equal passwords collide and rainbow tables apply. A password
//! hash must be **salted** (a unique random salt per password) and **slow** (a large,
//! tunable work factor). This provides that with PBKDF2 — the KDF that is just HMAC
//! iterated `c` times, so it is safe to implement over the audited `hmac_sha256`
//! rather than pulling in crypto (KEP-0000 §1). Argon2/bcrypt are stronger against
//! GPU/ASIC attackers; if you need them, wrap an audited crate behind the same two
//! functions — this is the dependency-free default, not a ceiling.
//!
//! The stored string is self-describing (algorithm, iterations, salt, hash), so the
//! work factor can be raised later without breaking existing hashes:
//!
//! ```text
//! pbkdf2-sha256$600000$<salt-b64url>$<hash-b64url>
//! ```

use crate::hash::hmac_sha256;
use crate::token::{b64url_decode, b64url_encode};

/// OWASP's 2023 floor for PBKDF2-HMAC-SHA256. Deliberately slow — the point is to
/// cost an attacker, not to be fast. Tune via [`hash_with`].
pub const DEFAULT_ITERATIONS: u32 = 600_000;

/// 128 bits of salt — enough that no two hashes share one in practice.
const SALT_LEN: usize = 16;

/// Hash `password` with a fresh random salt and the [`DEFAULT_ITERATIONS`] work
/// factor, returning the self-describing storage string.
#[must_use]
pub fn hash_password(password: &str) -> String {
    hash_with(password, DEFAULT_ITERATIONS)
}

/// [`hash_password`] with an explicit iteration count (for a different work factor,
/// or a fast one in tests).
#[must_use]
pub fn hash_with(password: &str, iterations: u32) -> String {
    let mut salt = [0u8; SALT_LEN];
    getrandom::getrandom(&mut salt).expect("OS randomness unavailable");
    let dk = pbkdf2_sha256(password.as_bytes(), &salt, iterations);
    format!("pbkdf2-sha256${iterations}${}${}", b64url_encode(&salt), b64url_encode(&dk))
}

/// Whether `password` matches a `stored` hash from [`hash_password`]. Recomputes the
/// derivation with the stored salt + iterations and compares in constant time. A
/// malformed or unknown-algorithm string returns `false` (never panics).
#[must_use]
pub fn verify_password(password: &str, stored: &str) -> bool {
    let mut parts = stored.split('$');
    if parts.next() != Some("pbkdf2-sha256") {
        return false; // unknown algorithm
    }
    let Some(iterations) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
        return false;
    };
    let Some(salt) = parts.next().and_then(b64url_decode) else {
        return false;
    };
    let Some(expected) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false; // trailing garbage
    }
    let dk = pbkdf2_sha256(password.as_bytes(), &salt, iterations);
    // Constant-time compare of the two base64url hashes (reuses the CSRF verify).
    crate::csrf::verify(&b64url_encode(&dk), expected)
}

/// PBKDF2-HMAC-SHA256 for a single 32-byte output block (RFC 8018 §5.2). The derived
/// key length equals the HMAC output, so only block 1 is needed:
/// `T = U_1 ^ U_2 ^ … ^ U_c`, where `U_1 = HMAC(pw, salt || 0x00000001)` and
/// `U_j = HMAC(pw, U_{j-1})`.
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut block1 = Vec::with_capacity(salt.len() + 4);
    block1.extend_from_slice(salt);
    block1.extend_from_slice(&1u32.to_be_bytes());

    let mut u = hmac_sha256(password, &block1);
    let mut out = u;
    for _ in 1..iterations.max(1) {
        u = hmac_sha256(password, &u);
        for (o, byte) in out.iter_mut().zip(u.iter()) {
            *o ^= byte;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // A small work factor keeps the tests fast; production uses DEFAULT_ITERATIONS.
    const TEST_ITER: u32 = 1000;

    #[test]
    fn a_password_verifies_against_its_own_hash() {
        let stored = hash_with("correct horse battery staple", TEST_ITER);
        assert!(verify_password("correct horse battery staple", &stored));
        assert!(!verify_password("wrong password", &stored));
    }

    #[test]
    fn the_same_password_hashes_differently_each_time() {
        // A random salt per hash → two hashes of the same password differ, but both
        // verify.
        let a = hash_with("hunter2", TEST_ITER);
        let b = hash_with("hunter2", TEST_ITER);
        assert_ne!(a, b, "the salt makes each hash unique");
        assert!(verify_password("hunter2", &a));
        assert!(verify_password("hunter2", &b));
    }

    #[test]
    fn the_stored_form_is_self_describing() {
        let stored = hash_with("pw", 4096);
        let parts: Vec<&str> = stored.split('$').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[0], "pbkdf2-sha256");
        assert_eq!(parts[1], "4096", "iterations are recoverable, so the factor can be raised later");
    }

    #[test]
    fn a_hash_stays_verifiable_regardless_of_the_default_changing() {
        // A hash made at one work factor still verifies (the count is read from the
        // stored string, not the current DEFAULT_ITERATIONS).
        let stored = hash_with("pw", 2048);
        assert!(verify_password("pw", &stored));
    }

    #[test]
    fn malformed_stored_strings_are_rejected_not_panicked() {
        for bad in ["", "plain", "md5$1$x$y", "pbkdf2-sha256$notanumber$s$h", "pbkdf2-sha256$1000$s$h$extra"] {
            assert!(!verify_password("pw", bad), "must reject {bad:?}");
        }
    }

    #[test]
    fn matches_published_pbkdf2_hmac_sha256_vectors() {
        // Known-answer vectors for PBKDF2-HMAC-SHA256, dkLen=32 (widely published,
        // e.g. RFC 7914 §11 references and the common SHA-256 test set). Proves the
        // derivation is spec-correct, so a hash interoperates rather than being merely
        // self-consistent.
        let c1 = pbkdf2_sha256(b"password", b"salt", 1);
        assert_eq!(
            hex(&c1),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b",
            "P=password S=salt c=1"
        );
        let c2 = pbkdf2_sha256(b"password", b"salt", 2);
        assert_eq!(
            hex(&c2),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43",
            "P=password S=salt c=2"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}
