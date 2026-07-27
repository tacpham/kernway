//! Response body decompression — the client half of the shared [`kernway_compress`]
//! layer (the server half compresses; this reverses it). A response marked
//! `Content-Encoding: gzip | deflate | br` is inflated transparently so callers always
//! see plain bytes, and the now-stale framing headers are dropped.

use kernway_compress::Encoding;

use crate::{HttpError, Response};

/// Inflate `resp.body` in place per its `Content-Encoding`. `identity`, an absent
/// header, or an unrecognised encoding leave the body untouched.
pub(crate) fn decompress(resp: &mut Response) -> Result<(), HttpError> {
    let Some(encoding) = resp
        .header("content-encoding")
        .and_then(Encoding::from_token)
    else {
        return Ok(()); // no encoding, or one we don't handle — leave as-is
    };
    if encoding == Encoding::Identity {
        return Ok(());
    }

    resp.body =
        kernway_compress::decode(&resp.body, encoding).map_err(|e| HttpError::Protocol(e.0))?;

    // The body is plain now; drop the framing headers so they don't lie about it.
    let Response { head, spans, .. } = resp;
    spans.retain(|(name, _)| {
        let name = &head[name.clone()];
        !name.eq_ignore_ascii_case(b"content-encoding")
            && !name.eq_ignore_ascii_case(b"content-length")
    });
    Ok(())
}
