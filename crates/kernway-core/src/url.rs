//! URL / `application/x-www-form-urlencoded` text codecs — the framework-wide helpers for
//! query values, form fields, and path segments. Shared here in the base crate so every tier
//! (server, security, an app's controllers) uses one implementation instead of hand-rolling
//! its own decoder.

/// Percent-encode text, keeping the unreserved set (`A-Z a-z 0-9 - _ . ~`) and escaping
/// everything else as `%XX` (space → `%20`, not `+`).
#[must_use]
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-decode text: `%XX` → its byte and `+` → space. A malformed escape (`%` not
/// followed by two hex digits) is kept literally; invalid UTF-8 is replaced. Inverse of
/// [`percent_encode`] (which emits `%20` for space, so a round-trip is exact).
#[must_use]
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            // Hex digits are read as bytes: slicing the `&str` here would panic
            // when a multi-byte character follows the `%` (`"%€"`).
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(hi << 4 | lo);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// Read one field from an `application/x-www-form-urlencoded` body by name, percent-decoded
/// (so `+` → space and `%XX` → its byte, via [`percent_decode`]). Returns an empty string if
/// the field is absent — a missing form field and a present-but-empty one are not
/// distinguished, which is what a plain form POST wants. The first match wins.
#[must_use]
pub fn form_field(body: &[u8], name: &str) -> String {
    String::from_utf8_lossy(body)
        .split('&')
        .find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == name).then(|| percent_decode(v))
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_field_basics() {
        let body = b"email=a%40b.com&name=Jo+Reader&empty=";
        assert_eq!(form_field(body, "email"), "a@b.com");
        assert_eq!(form_field(body, "name"), "Jo Reader");
        assert_eq!(form_field(body, "empty"), "");
        assert_eq!(form_field(body, "missing"), ""); // absent → empty, same as present-empty
    }

    #[test]
    fn decode_basics() {
        assert_eq!(percent_decode("a%40b.com"), "a@b.com");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%"), "100%"); // malformed escape kept
        assert_eq!(percent_decode("plain.value-1~"), "plain.value-1~");
    }

    #[test]
    fn a_percent_before_a_multibyte_character_does_not_panic() {
        assert_eq!(percent_decode("%€"), "%€");
        assert_eq!(percent_decode("a%é"), "a%é");
        assert_eq!(percent_decode("%1€"), "%1€");
        assert_eq!(percent_decode("%C3%A9%"), "é%");
    }

    #[test]
    fn encode_then_decode_is_identity() {
        let s = "user+tag@x.com / a b?c#d é";
        assert_eq!(percent_decode(&percent_encode(s)), s);
    }
}
