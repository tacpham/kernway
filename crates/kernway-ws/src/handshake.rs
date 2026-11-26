//! WebSocket opening handshake (RFC 6455 §4.2): validate the client's upgrade
//! request and compute `Sec-WebSocket-Accept`.
//!
//! `Sec-WebSocket-Accept = base64(SHA-1(key + GUID))`. SHA-1 and base64 are
//! implemented here (a few dozen lines each) so the crate needs no crypto
//! dependency — SHA-1 is used only for this fixed, non-security-sensitive
//! derivation, exactly as the RFC prescribes.

/// The GUID appended to the client key (RFC 6455 §1.3).
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// `Sec-WebSocket-Accept` for a client `Sec-WebSocket-Key`.
#[must_use]
pub fn accept_key(client_key: &str) -> String {
    let mut input = Vec::with_capacity(client_key.len() + GUID.len());
    input.extend_from_slice(client_key.trim().as_bytes());
    input.extend_from_slice(GUID.as_bytes());
    base64_encode(&sha1(&input))
}

/// Why an upgrade request was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeError {
    /// Not `GET`.
    Method,
    /// No `Upgrade: websocket`.
    NotWebSocket,
    /// `Connection` does not list `upgrade`.
    NoConnectionUpgrade,
    /// `Sec-WebSocket-Version` is not 13 — answered with `426` + the version we speak.
    Version,
    /// `Sec-WebSocket-Key` missing or not 16 base64-encoded bytes.
    Key,
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            HandshakeError::Method => "websocket upgrade must be a GET",
            HandshakeError::NotWebSocket => "missing `Upgrade: websocket`",
            HandshakeError::NoConnectionUpgrade => "missing `Connection: Upgrade`",
            HandshakeError::Version => "unsupported Sec-WebSocket-Version (need 13)",
            HandshakeError::Key => "missing or malformed Sec-WebSocket-Key",
        })
    }
}

/// Check an upgrade request's method and headers; on success return the
/// `Sec-WebSocket-Accept` value to send back.
///
/// `header` looks a request header up by (case-insensitive) name.
pub fn validate<'a>(
    method: &str,
    header: impl Fn(&str) -> Option<&'a str>,
) -> Result<String, HandshakeError> {
    if !method.eq_ignore_ascii_case("GET") {
        return Err(HandshakeError::Method);
    }
    let lists = |name: &str, token: &str| {
        header(name).is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
    };
    if !lists("upgrade", "websocket") {
        return Err(HandshakeError::NotWebSocket);
    }
    if !lists("connection", "upgrade") {
        return Err(HandshakeError::NoConnectionUpgrade);
    }
    if header("sec-websocket-version").map(str::trim) != Some("13") {
        return Err(HandshakeError::Version);
    }
    let key = header("sec-websocket-key")
        .map(str::trim)
        .ok_or(HandshakeError::Key)?;
    match base64_decode(key) {
        Some(raw) if raw.len() == 16 => Ok(accept_key(key)),
        _ => Err(HandshakeError::Key),
    }
}

// ------------------------------------------------------------------ SHA-1 (FIPS 180-4)

/// SHA-1 digest of `data`.
#[must_use]
pub fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (hv, v) in h.iter_mut().zip([a, b, c, d, e]) {
            *hv = hv.wrapping_add(v);
        }
    }
    let mut out = [0u8; 20];
    for (chunk, v) in out.chunks_exact_mut(4).zip(h) {
        chunk.copy_from_slice(&v.to_be_bytes());
    }
    out
}

// ------------------------------------------------------------------ base64 (RFC 4648)

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
#[must_use]
pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Decode standard base64 (padding required, no whitespace). `None` if malformed.
#[must_use]
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let val = |c: u8| B64.iter().position(|&b| b == c).map(|p| p as u32);
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for (i, quad) in bytes.chunks_exact(4).enumerate() {
        let last = i == bytes.len() / 4 - 1;
        let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &quad[..4 - pad] {
            n = (n << 6) | val(c)?;
        }
        n <<= 6 * pad as u32;
        out.push((n >> 16) as u8);
        if pad < 2 {
            out.push((n >> 8) as u8);
        }
        if pad < 1 {
            out.push(n as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc6455_example_accept_key() {
        // RFC 6455 §1.3.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn sha1_known_vectors() {
        let hex = |d: [u8; 20]| d.iter().map(|b| format!("{b:02x}")).collect::<String>();
        assert_eq!(hex(sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            hex(sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        // Two blocks.
        assert_eq!(
            hex(sha1(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
    }

    #[test]
    fn base64_round_trip() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"foob", b"fooba", b"foobar"] {
            assert_eq!(base64_decode(&base64_encode(data)).unwrap(), data);
        }
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert!(base64_decode("abc").is_none());
        assert!(base64_decode("ab=c").is_none());
    }

    fn headers<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<&'a str> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| *v)
        }
    }

    #[test]
    fn validates_a_browser_upgrade() {
        let ok = [
            ("Upgrade", "websocket"),
            ("Connection", "keep-alive, Upgrade"),
            ("Sec-WebSocket-Version", "13"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ];
        assert_eq!(
            validate("GET", headers(&ok)).unwrap(),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        assert_eq!(validate("POST", headers(&ok)), Err(HandshakeError::Method));

        let mut v8 = ok;
        v8[2].1 = "8";
        assert_eq!(validate("GET", headers(&v8)), Err(HandshakeError::Version));
        let mut bad_key = ok;
        bad_key[3].1 = "c2hvcnQ="; // "short": not 16 bytes
        assert_eq!(validate("GET", headers(&bad_key)), Err(HandshakeError::Key));
        let mut no_conn = ok;
        no_conn[1].1 = "keep-alive";
        assert_eq!(
            validate("GET", headers(&no_conn)),
            Err(HandshakeError::NoConnectionUpgrade)
        );
    }
}
