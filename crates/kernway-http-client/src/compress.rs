//! Response body decompression — `Content-Encoding: gzip | deflate | br`.
//!
//! The client advertises `Accept-Encoding: gzip, deflate, br`; a server that honours
//! it sends a compressed body, which this transparently inflates so callers always see
//! plain bytes. The framing headers (`Content-Encoding`, the now-wrong
//! `Content-Length`) are dropped after decoding so they cannot mislead.

use std::io::Read;

use crate::{HttpError, Response};

/// Inflate `resp.body` in place per its `Content-Encoding`. `identity`, an absent
/// header, or an unknown encoding leave the body untouched.
pub(crate) fn decompress(resp: &mut Response) -> Result<(), HttpError> {
    let Some(encoding) = resp.header("content-encoding").map(|e| e.trim().to_ascii_lowercase()) else {
        return Ok(());
    };
    let decoded = match encoding.as_str() {
        "gzip" | "x-gzip" => gunzip(&resp.body)?,
        "deflate" => inflate(&resp.body)?,
        "br" => unbrotli(&resp.body)?,
        _ => return Ok(()), // identity or something we don't decode — leave as-is
    };
    resp.body = decoded;

    // The body is plain now; drop the framing headers so they don't lie about it.
    let Response { head, spans, .. } = resp;
    spans.retain(|(name, _)| {
        let name = &head[name.clone()];
        !name.eq_ignore_ascii_case(b"content-encoding") && !name.eq_ignore_ascii_case(b"content-length")
    });
    Ok(())
}

fn gunzip(data: &[u8]) -> Result<Vec<u8>, HttpError> {
    read_all(flate2::read::GzDecoder::new(data), "gzip")
}

fn inflate(data: &[u8]) -> Result<Vec<u8>, HttpError> {
    // HTTP "deflate" is ambiguously zlib-wrapped or raw; try zlib, then raw.
    read_all(flate2::read::ZlibDecoder::new(data), "deflate")
        .or_else(|_| read_all(flate2::read::DeflateDecoder::new(data), "deflate"))
}

fn unbrotli(data: &[u8]) -> Result<Vec<u8>, HttpError> {
    read_all(brotli::Decompressor::new(data, 4096), "br")
}

fn read_all(mut reader: impl Read, what: &str) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::new();
    reader.read_to_end(&mut out).map_err(|e| HttpError::Protocol(format!("{what} decode: {e}")))?;
    Ok(out)
}
