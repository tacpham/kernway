//! # kernway-compress
//!
//! One HTTP compression layer for the whole framework: encode + decode for
//! `gzip`/`deflate`/`br`, plus the two policy helpers — [`is_compressible`] (which
//! content types are worth compressing) and [`negotiate`] (which encoding a client
//! will accept). The **server** uses it to compress dynamic responses; the **client**
//! uses it to decompress responses it receives. Same algorithm, both directions — so
//! it lives in one place rather than being reimplemented per side.
//!
//! Codecs are pure Rust (flate2 on miniz_oxide, brotli) — no C toolchain (KEP-0000 §1).

#![forbid(unsafe_code)]

use std::io::{Read, Write};

/// A content encoding this crate can apply or reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// No compression.
    Identity,
    /// gzip (RFC 1952).
    Gzip,
    /// zlib/deflate (RFC 1950/1951).
    Deflate,
    /// Brotli (RFC 7932).
    Br,
}

impl Encoding {
    /// The `Content-Encoding` token (`"gzip"`, `"deflate"`, `"br"`, `"identity"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Identity => "identity",
            Encoding::Gzip => "gzip",
            Encoding::Deflate => "deflate",
            Encoding::Br => "br",
        }
    }

    /// Parse a `Content-Encoding` token (case-insensitive). Unknown tokens → `None`.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token.trim().to_ascii_lowercase().as_str() {
            "identity" | "" => Some(Encoding::Identity),
            "gzip" | "x-gzip" => Some(Encoding::Gzip),
            "deflate" => Some(Encoding::Deflate),
            "br" => Some(Encoding::Br),
            _ => None,
        }
    }
}

/// A compression/decompression failure (a corrupt or truncated stream).
#[derive(Debug, Clone)]
pub struct CompressError(pub String);

impl std::fmt::Display for CompressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "compression error: {}", self.0)
    }
}

impl std::error::Error for CompressError {}

/// Compress `data` with `encoding` (`Identity` copies it through).
#[must_use]
pub fn encode(data: &[u8], encoding: Encoding) -> Vec<u8> {
    match encoding {
        Encoding::Identity => data.to_vec(),
        Encoding::Gzip => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(data).expect("in-memory gzip write");
            e.finish().expect("in-memory gzip finish")
        }
        Encoding::Deflate => {
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(data).expect("in-memory deflate write");
            e.finish().expect("in-memory deflate finish")
        }
        Encoding::Br => {
            // Quality 5 / window 22 — a good size/speed balance for responses.
            let mut w = brotli::CompressorWriter::new(Vec::new(), 4096, 5, 22);
            w.write_all(data).expect("in-memory brotli write");
            w.into_inner()
        }
    }
}

/// Decompress `data` that was encoded with `encoding` (`Identity` copies it through).
pub fn decode(data: &[u8], encoding: Encoding) -> Result<Vec<u8>, CompressError> {
    match encoding {
        Encoding::Identity => Ok(data.to_vec()),
        Encoding::Gzip => read_all(flate2::read::GzDecoder::new(data), "gzip"),
        // HTTP "deflate" is ambiguously zlib-wrapped or raw — try zlib, then raw.
        Encoding::Deflate => read_all(flate2::read::ZlibDecoder::new(data), "deflate")
            .or_else(|_| read_all(flate2::read::DeflateDecoder::new(data), "deflate")),
        Encoding::Br => read_all(brotli::Decompressor::new(data, 4096), "br"),
    }
}

fn read_all(mut reader: impl Read, what: &str) -> Result<Vec<u8>, CompressError> {
    let mut out = Vec::new();
    reader
        .read_to_end(&mut out)
        .map_err(|e| CompressError(format!("{what}: {e}")))?;
    Ok(out)
}

/// Whether a `Content-Type` is worth compressing — text and text-like formats compress
/// well; already-compressed binaries (images, video, archives) do not, so blindly
/// compressing them just burns CPU. A `; charset=…` parameter is ignored.
#[must_use]
pub fn is_compressible(content_type: &str) -> bool {
    let base = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim();
    if base.starts_with("text/") {
        return true;
    }
    matches!(
        base,
        "application/json"
            | "application/xml"
            | "application/javascript"
            | "application/manifest+json"
            | "application/wasm"
            | "application/rss+xml"
            | "application/atom+xml"
            | "image/svg+xml"
            | "font/ttf"
            | "font/otf"
    )
}

/// The best encoding to use for a client's `Accept-Encoding` header, preferring
/// `br` > `gzip` > `deflate`. `Identity` if the client accepts none of them (or the
/// header is empty). A token with `q=0` is treated as "not acceptable".
#[must_use]
pub fn negotiate(accept_encoding: &str) -> Encoding {
    let (mut br, mut gzip, mut deflate) = (false, false, false);
    for part in accept_encoding.split(',') {
        let part = part.trim();
        let (token, q) = match part.split_once(';') {
            Some((token, params)) => (token.trim(), parse_q(params)),
            None => (part, 1.0),
        };
        if q <= 0.0 {
            continue; // explicitly refused
        }
        match token.to_ascii_lowercase().as_str() {
            "br" => br = true,
            "gzip" | "x-gzip" => gzip = true,
            "deflate" => deflate = true,
            "*" => {
                br = true;
                gzip = true;
                deflate = true;
            }
            _ => {}
        }
    }
    if br {
        Encoding::Br
    } else if gzip {
        Encoding::Gzip
    } else if deflate {
        Encoding::Deflate
    } else {
        Encoding::Identity
    }
}

/// The `q=` value from an Accept-Encoding parameter list (default `1.0`).
fn parse_q(params: &str) -> f32 {
    for p in params.split(';') {
        let p = p.trim();
        if let Some(v) = p.strip_prefix("q=").or_else(|| p.strip_prefix("Q=")) {
            return v.trim().parse().unwrap_or(1.0);
        }
    }
    1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_codec_round_trips() {
        let data = b"the quick brown fox jumps over the lazy dog, repeatedly. ".repeat(20);
        for enc in [
            Encoding::Gzip,
            Encoding::Deflate,
            Encoding::Br,
            Encoding::Identity,
        ] {
            let encoded = encode(&data, enc);
            if enc != Encoding::Identity {
                assert!(
                    encoded.len() < data.len(),
                    "{enc:?} compressed a repetitive payload"
                );
            }
            assert_eq!(decode(&encoded, enc).unwrap(), data, "{enc:?} round-trips");
        }
    }

    #[test]
    fn a_corrupt_stream_errors() {
        assert!(decode(b"not gzip at all", Encoding::Gzip).is_err());
    }

    #[test]
    fn tokens_parse() {
        assert_eq!(Encoding::from_token("GZIP"), Some(Encoding::Gzip));
        assert_eq!(Encoding::from_token("br"), Some(Encoding::Br));
        assert_eq!(Encoding::from_token(""), Some(Encoding::Identity));
        assert_eq!(Encoding::from_token("bzip2"), None);
    }

    #[test]
    fn compressible_types() {
        assert!(is_compressible("text/html; charset=utf-8"));
        assert!(is_compressible("application/json"));
        assert!(is_compressible("image/svg+xml"));
        assert!(!is_compressible("image/png"));
        assert!(!is_compressible("application/octet-stream"));
        assert!(!is_compressible("video/mp4"));
    }

    #[test]
    fn negotiation_prefers_brotli_then_gzip() {
        assert_eq!(negotiate("gzip, deflate, br"), Encoding::Br);
        assert_eq!(negotiate("gzip, deflate"), Encoding::Gzip);
        assert_eq!(negotiate("deflate"), Encoding::Deflate);
        assert_eq!(negotiate(""), Encoding::Identity);
        assert_eq!(negotiate("identity"), Encoding::Identity);
        assert_eq!(negotiate("*"), Encoding::Br);
        // q=0 refuses an encoding.
        assert_eq!(negotiate("br;q=0, gzip"), Encoding::Gzip);
    }
}
