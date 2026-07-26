//! The signed session token (KEP-0004): `payload.signature`, HMAC-SHA256.
//!
//! A compact, cookie-safe encoding of the session claims. Not JWT's JSON — a tight
//! kernway encoding — but the same shape: a base64url payload and a base64url MAC
//! over it, verified in constant time.

use crate::csrf;
use crate::hash::hmac_sha256;

/// The claims carried in a token. `version` is the account version at login
/// (KEP-0004); it lets a role/active/expire change force re-login by mismatching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    /// Session id — the key into the registry.
    pub sid: String,
    /// The authenticated principal (username).
    pub user: String,
    /// The roles snapshot at login.
    pub roles: Vec<String>,
    /// The account version at login (KEP-0004), for forcing re-login on a change.
    pub version: u64,
    /// The token's own expiry, unix seconds — a fail-safe upper bound.
    pub exp: u64,
}

/// Signs and verifies session tokens with a secret key.
pub struct TokenCodec {
    key: Vec<u8>,
}

impl TokenCodec {
    /// Build a codec over a signing key. Keep the key secret and stable; rotating
    /// it invalidates every outstanding token.
    pub fn new(key: impl Into<Vec<u8>>) -> Self {
        Self { key: key.into() }
    }

    /// Sign claims into a `payload.signature` token.
    pub fn sign(&self, claims: &Claims) -> String {
        let payload = b64url_encode(&encode_claims(claims));
        let sig = b64url_encode(&hmac_sha256(&self.key, payload.as_bytes()));
        format!("{payload}.{sig}")
    }

    /// Verify a token's signature and parse its claims. `None` if the signature is
    /// wrong or the token is malformed. Does **not** check `exp` — that is the
    /// session manager's job, against the current clock and config.
    pub fn verify(&self, token: &str) -> Option<Claims> {
        let (payload, sig) = token.split_once('.')?;
        let expected = b64url_encode(&hmac_sha256(&self.key, payload.as_bytes()));
        // Constant-time compare — reuse the CSRF verify.
        if !csrf::verify(sig, &expected) {
            return None;
        }
        decode_claims(&b64url_decode(payload)?)
    }
}

// Field separator that never appears in a username, role, or sid.
const SEP: char = '\u{1f}';

fn encode_claims(c: &Claims) -> Vec<u8> {
    let roles = c.roles.join(",");
    format!("{}{SEP}{}{SEP}{}{SEP}{}{SEP}{}", c.sid, c.user, roles, c.version, c.exp).into_bytes()
}

fn decode_claims(bytes: &[u8]) -> Option<Claims> {
    let s = std::str::from_utf8(bytes).ok()?;
    let mut parts = s.split(SEP);
    let sid = parts.next()?.to_string();
    let user = parts.next()?.to_string();
    let roles_s = parts.next()?;
    let version = parts.next()?.parse().ok()?;
    let exp = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None; // extra fields — malformed
    }
    let roles = if roles_s.is_empty() {
        Vec::new()
    } else {
        roles_s.split(',').map(String::from).collect()
    };
    Some(Claims { sid, user, roles, version, exp })
}

// --- base64url (no padding) — shared with the password hasher and JWT ---

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url without padding (RFC 4648 §5) — the encoding JWT, PKCE, and the session
/// token share. Public so sibling crates (e.g. OAuth2/PKCE) reuse it.
pub fn b64url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6 & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(n & 63) as usize] as char);
        }
    }
    out
}

pub(crate) fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        let v = match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'-' => 62,
            b'_' => 63,
            b'=' => continue, // tolerate padding if present
            _ => return None,
        };
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> Claims {
        Claims {
            sid: "abc123".into(),
            user: "alice".into(),
            roles: vec!["ADMIN".into(), "USER".into()],
            version: 7,
            exp: 1_800_000_000,
        }
    }

    #[test]
    fn base64url_round_trips_arbitrary_bytes() {
        for len in 0..20 {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(b64url_decode(&b64url_encode(&data)).unwrap(), data, "len {len}");
        }
    }

    #[test]
    fn sign_then_verify_round_trips_claims() {
        let codec = TokenCodec::new("secret-key");
        let token = codec.sign(&claims());
        assert_eq!(codec.verify(&token), Some(claims()));
    }

    #[test]
    fn a_tampered_payload_fails_verification() {
        let codec = TokenCodec::new("secret-key");
        let token = codec.sign(&claims());
        // Flip a character in the payload.
        let mut bad: Vec<char> = token.chars().collect();
        bad[0] = if bad[0] == 'A' { 'B' } else { 'A' };
        let bad: String = bad.into_iter().collect();
        assert_eq!(codec.verify(&bad), None);
    }

    #[test]
    fn a_wrong_key_fails_verification() {
        let a = TokenCodec::new("key-a");
        let b = TokenCodec::new("key-b");
        let token = a.sign(&claims());
        assert_eq!(b.verify(&token), None, "a token signed with a different key must not verify");
    }

    #[test]
    fn malformed_tokens_are_rejected() {
        let codec = TokenCodec::new("k");
        assert_eq!(codec.verify("no-dot"), None);
        assert_eq!(codec.verify("only.parts.here"), None);
        assert_eq!(codec.verify(""), None);
    }

    #[test]
    fn empty_roles_round_trip() {
        let codec = TokenCodec::new("k");
        let mut c = claims();
        c.roles = Vec::new();
        let token = codec.sign(&c);
        assert_eq!(codec.verify(&token).unwrap().roles, Vec::<String>::new());
    }
}
