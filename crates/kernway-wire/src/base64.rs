//! Standard base64 (RFC 4648 §4), shared by the protocols that carry binary in
//! text: SCRAM messages, SSH public keys and fingerprints, PEM bodies.

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
#[must_use]
pub fn encode(data: &[u8]) -> String {
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                s.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// Standard base64 without the `=` padding (OpenSSH's `SHA256:` fingerprints).
#[must_use]
pub fn encode_unpadded(data: &[u8]) -> String {
    let mut s = encode(data);
    while s.ends_with('=') {
        s.pop();
    }
    s
}

/// Decode standard base64 (padding optional, ASCII whitespace ignored); `None`
/// if malformed.
#[must_use]
pub fn decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches(|c: char| c == '=' || c.is_ascii_whitespace());
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        if c.is_ascii_whitespace() {
            continue;
        }
        let v = B64.iter().position(|&b| b == c)? as u32;
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

    #[test]
    fn rfc_4648_vectors() {
        for (plain, coded) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")] {
            assert_eq!(encode(plain.as_bytes()), coded);
            assert_eq!(decode(coded).unwrap(), plain.as_bytes());
            assert_eq!(decode(coded.trim_end_matches('=')).unwrap(), plain.as_bytes());
        }
        assert_eq!(encode_unpadded(b"f"), "Zg");
        assert_eq!(decode("Zm9v\nYmFy").unwrap(), b"foobar", "line breaks inside, as in PEM");
        assert!(decode("Zm9v!").is_none());
    }
}
