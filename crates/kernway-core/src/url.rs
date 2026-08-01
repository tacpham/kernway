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
            b'%' if i + 2 < b.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(n) => {
                    out.push(n);
                    i += 3;
                }
                Err(_) => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_basics() {
        assert_eq!(percent_decode("a%40b.com"), "a@b.com");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%"), "100%"); // malformed escape kept
        assert_eq!(percent_decode("plain.value-1~"), "plain.value-1~");
    }

    #[test]
    fn encode_then_decode_is_identity() {
        let s = "user+tag@x.com / a b?c#d é";
        assert_eq!(percent_decode(&percent_encode(s)), s);
    }
}
