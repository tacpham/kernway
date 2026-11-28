//! Response body decompression — the client half of the shared [`kernway_compress`]
//! layer (the server half compresses; this reverses it). A response marked
//! `Content-Encoding: gzip | deflate | br` (or a list of them, applied in order) is
//! inflated transparently so callers always see plain bytes, and the now-stale framing
//! headers are dropped. The inflated size is capped, so a small "compression bomb"
//! fails instead of exhausting memory.

use kernway_compress::Encoding;

use crate::{HttpError, Response};

/// Inflate `resp.body` in place per its `Content-Encoding`, producing at most `max`
/// bytes. `identity`, an absent header, or a coding we do not know leave the body
/// (and its headers) untouched.
pub(crate) fn decompress(resp: &mut Response, max: usize) -> Result<(), HttpError> {
    let Some(header) = resp.header("content-encoding") else {
        return Ok(());
    };
    // `Content-Encoding: gzip, br` means gzip was applied first: undo br, then gzip.
    let mut codings = Vec::new();
    for token in header.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        match Encoding::from_token(token) {
            Some(Encoding::Identity) => {}
            Some(e) => codings.push(e),
            None => return Ok(()), // a coding we cannot undo — hand over the bytes as sent
        }
    }
    if codings.is_empty() {
        return Ok(());
    }
    let mut body = std::mem::take(&mut resp.body);
    for &coding in codings.iter().rev() {
        body = kernway_compress::decode_limited(&body, coding, max).map_err(|e| {
            if e.is_too_large() {
                HttpError::TooLarge(format!("decompressed body over the {max}-byte limit"))
            } else {
                HttpError::Protocol(e.0)
            }
        })?;
    }
    resp.body = body;

    // The body is plain now; drop the framing headers so they don't lie about it.
    let Response { head, spans, .. } = resp;
    spans.retain(|(name, _)| {
        let name = &head[name.clone()];
        !name.eq_ignore_ascii_case(b"content-encoding")
            && !name.eq_ignore_ascii_case(b"content-length")
    });
    Ok(())
}
