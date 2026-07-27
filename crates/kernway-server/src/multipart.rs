//! `Multipart` — a `multipart/form-data` request body (RFC 7578).
//!
//! A browser file-upload form (`<form enctype="multipart/form-data">`) sends a
//! boundary-delimited stream of parts, each with its own `Content-Disposition`
//! (a `name`, and for a file a `filename`). This extractor iterates those parts:
//! a short field is read as text, a file part is spooled to a temp file and
//! handed back as an [`UploadFile`] — the same handle a raw body upload gets, so
//! `persist` moves it into place off the request path.
//!
//! ```rust,ignore
//! async fn create(&self, mut form: Multipart) -> impl IntoResponse {
//!     while let Some(part) = form.next().await? {
//!         match part.name() {
//!             "title" => { let title = part.text()?; /* small: in memory */ }
//!             "cover" => { part.file().await?.persist("/data/covers/x.png").await?; }
//!             _ => {}
//!         }
//!     }
//!     StatusCode::CREATED
//! }
//! ```
//!
//! This is the first cut of [KEP-0008]: the parser reads the already-received body
//! (`req.body`, or the spooled temp file materialised via `body_bytes`), then
//! splits it into parts and spools the file ones. Parsing straight off the socket —
//! so a multi-GB form never buffers the outer body whole — is the deferred
//! optimisation noted there.
//!
//! [KEP-0008]: https://github.com/tacpham/kernway/blob/main/docs/kep/0008-request-body.md

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use kernway_core::request::Request;

use crate::upload::UploadFile;

/// A part is a file (spooled) once its `Content-Disposition` carries a `filename`;
/// otherwise it is a form field kept in memory. This mirrors what browsers send:
/// `<input type="file">` sets a filename, a text input does not.
const MAX_PARTS: usize = 1000;

/// Cap a single part's header block, so a part with a pathological
/// `Content-Disposition` cannot force unbounded header buffering (RFC-parsing DoS).
const MAX_PART_HEADER_BYTES: usize = 16 * 1024;

/// What went wrong parsing a `multipart/form-data` body. Renders as a `400` through
/// the [`Extract`](crate::extract::Extract) impl.
#[derive(Debug)]
pub enum MultipartError {
    /// The request is not `multipart/form-data`, or its `Content-Type` has no `boundary`.
    NotMultipart,
    /// The body did not follow the boundary structure (truncated, missing delimiter).
    Malformed(&'static str),
    /// A part had no `name` in its `Content-Disposition` (required by RFC 7578 §4.2).
    MissingName,
    /// More than `MAX_PARTS` parts, or a part header over `MAX_PART_HEADER_BYTES`.
    TooLarge(&'static str),
    /// `text()` was called on a part whose bytes are not valid UTF-8.
    NotUtf8,
    /// The temp file for a spooled file part could not be written.
    Io(std::io::Error),
}

impl std::fmt::Display for MultipartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MultipartError::NotMultipart => f.write_str("not a multipart/form-data body (no boundary)"),
            MultipartError::Malformed(w) => write!(f, "malformed multipart body: {w}"),
            MultipartError::MissingName => f.write_str("a multipart part is missing its `name`"),
            MultipartError::TooLarge(w) => write!(f, "multipart body rejected: {w}"),
            MultipartError::NotUtf8 => f.write_str("multipart text field is not valid UTF-8"),
            MultipartError::Io(e) => write!(f, "spooling a multipart file part failed: {e}"),
        }
    }
}

impl std::error::Error for MultipartError {}

/// A parsed part before it is handed out — headers plus the body bytes in memory.
#[derive(Debug)]
struct RawPart {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

/// A `multipart/form-data` body, iterated one [`Part`] at a time.
///
/// Built by the `#[controller]` extractor, or directly with [`from_request`](Self::from_request).
/// The whole body is parsed up front (first cut); [`next`](Self::next) then walks the parts.
#[derive(Debug)]
pub struct Multipart {
    parts: std::vec::IntoIter<RawPart>,
    /// Where a file part's temp file is written — the spooled body's directory when
    /// the request streamed to disk (so parts land beside it, on the configured
    /// `upload_temp_dir`), otherwise the system temp dir.
    spool_dir: PathBuf,
}

impl Multipart {
    /// Parse the request's `multipart/form-data` body.
    ///
    /// # Errors
    /// [`MultipartError::NotMultipart`] if the `Content-Type` is not multipart or has no
    /// boundary; [`MultipartError::Malformed`] / [`MissingName`](MultipartError::MissingName)
    /// / [`TooLarge`](MultipartError::TooLarge) on a bad body.
    pub fn from_request(req: &Request) -> Result<Self, MultipartError> {
        let content_type = req.header("content-type").ok_or(MultipartError::NotMultipart)?;
        let boundary = boundary_of(content_type).ok_or(MultipartError::NotMultipart)?;

        // First cut: parse the body we already have. `body_bytes()` borrows `req.body`
        // for the common small form, and reads the spooled temp file for a large one.
        let body = req.body_bytes();
        let parts = parse_parts(&body, boundary.as_bytes())?;

        // File parts spool beside the request's own spool file when there is one, so
        // they inherit the configured upload_temp_dir; otherwise the system temp dir.
        let spool_dir = req
            .body_spool
            .as_ref()
            .and_then(|s| s.path.parent().map(std::path::Path::to_path_buf))
            .unwrap_or_else(std::env::temp_dir);

        Ok(Self { parts: parts.into_iter(), spool_dir })
    }

    /// The next part, or `None` at the end.
    ///
    /// `async` for forward-compatibility with socket-direct streaming; today it pops
    /// an already-parsed part and never awaits.
    ///
    /// # Errors
    /// Never, in this cut — the whole body was validated in [`from_request`](Self::from_request). The
    /// `Result` is part of the streaming-ready signature.
    #[allow(clippy::unused_async, clippy::missing_errors_doc)]
    pub async fn next(&mut self) -> Result<Option<Part>, MultipartError> {
        Ok(self.parts.next().map(|raw| Part { raw, spool_dir: self.spool_dir.clone() }))
    }
}

/// One part of a multipart body: a named form field, possibly a file.
#[derive(Debug)]
pub struct Part {
    raw: RawPart,
    spool_dir: PathBuf,
}

impl Part {
    /// The field name (`Content-Disposition`'s `name`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.raw.name
    }

    /// The client's filename for a file part, if any. Its presence is what marks a
    /// part as a file rather than a plain field.
    #[must_use]
    pub fn filename(&self) -> Option<&str> {
        self.raw.filename.as_deref()
    }

    /// Whether this part is a file (it carried a `filename`).
    #[must_use]
    pub fn is_file(&self) -> bool {
        self.raw.filename.is_some()
    }

    /// The part's own `Content-Type`, if it declared one.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.raw.content_type.as_deref()
    }

    /// The raw part body bytes (in memory).
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.raw.body
    }

    /// The part's body decoded as UTF-8 text — for a short form field.
    ///
    /// # Errors
    /// [`MultipartError::NotUtf8`] if the bytes are not valid UTF-8.
    pub fn text(&self) -> Result<String, MultipartError> {
        std::str::from_utf8(&self.raw.body).map(str::to_owned).map_err(|_| MultipartError::NotUtf8)
    }

    /// Spool this part's bytes to a temp file and return it as an [`UploadFile`],
    /// so `persist` can move it into place. Runs the write on the blocking pool.
    ///
    /// # Errors
    /// [`MultipartError::Io`] if the temp file cannot be written.
    pub async fn file(self) -> Result<UploadFile, MultipartError> {
        let path = part_temp_path(&self.spool_dir);
        let len = self.raw.body.len() as u64;
        let bytes = self.raw.body;
        let write_path = path.clone();
        let written = rt_core::spawn_blocking(move || std::fs::write(&write_path, &bytes)).await;
        match written {
            Some(Ok(())) => Ok(UploadFile::from_spooled(path, len)),
            Some(Err(e)) => Err(MultipartError::Io(e)),
            None => Err(MultipartError::Io(std::io::Error::other("blocking pool unavailable"))),
        }
    }
}

/// The `boundary=` value from a `multipart/form-data` Content-Type, unquoted.
fn boundary_of(content_type: &str) -> Option<String> {
    let ct = content_type.trim();
    if !ct.to_ascii_lowercase().starts_with("multipart/form-data") {
        return None;
    }
    for param in ct.split(';').skip(1) {
        let param = param.trim();
        if let Some(rest) = param.strip_prefix("boundary=").or_else(|| param.strip_prefix("boundary =")) {
            let b = rest.trim().trim_matches('"');
            if !b.is_empty() {
                return Some(b.to_string());
            }
        }
    }
    None
}

/// Split a multipart body into its parts. Pure over the byte slice.
fn parse_parts(body: &[u8], boundary: &[u8]) -> Result<Vec<RawPart>, MultipartError> {
    // The delimiter on the wire is `--boundary`; parts are separated by
    // `\r\n--boundary`, and the body ends at `--boundary--`.
    let mut dash = Vec::with_capacity(boundary.len() + 2);
    dash.extend_from_slice(b"--");
    dash.extend_from_slice(boundary);

    // Skip any preamble up to the first delimiter.
    let mut pos = find(body, &dash, 0).ok_or(MultipartError::Malformed("no opening boundary"))?;
    pos += dash.len();

    let mut parts = Vec::new();
    loop {
        // Right after a `--boundary`: `--` closes the body, `\r\n` starts a part.
        if body[pos..].starts_with(b"--") {
            return Ok(parts); // closing delimiter
        }
        // Tolerate optional transport padding (whitespace) before the CRLF.
        let after = skip_ws(body, pos);
        if !body[after..].starts_with(b"\r\n") {
            return Err(MultipartError::Malformed("boundary not followed by CRLF"));
        }
        pos = after + 2;

        // Part headers end at a blank line.
        let header_end = find(body, b"\r\n\r\n", pos)
            .ok_or(MultipartError::Malformed("part headers not terminated"))?;
        if header_end - pos > MAX_PART_HEADER_BYTES {
            return Err(MultipartError::TooLarge("a part header block is too large"));
        }
        let headers = &body[pos..header_end];
        pos = header_end + 4;

        // Body runs up to the next `\r\n--boundary`.
        let mut next_delim = Vec::with_capacity(dash.len() + 2);
        next_delim.extend_from_slice(b"\r\n");
        next_delim.extend_from_slice(&dash);
        let body_end = find(body, &next_delim, pos)
            .ok_or(MultipartError::Malformed("part body not terminated by a boundary"))?;
        let part_body = body[pos..body_end].to_vec();
        pos = body_end + next_delim.len();

        let (name, filename, content_type) = parse_headers(headers)?;
        if parts.len() >= MAX_PARTS {
            return Err(MultipartError::TooLarge("too many parts"));
        }
        parts.push(RawPart { name, filename, content_type, body: part_body });
    }
}

/// (name, filename, content_type) parsed from a part's `Content-Disposition` /
/// `Content-Type` headers.
type PartMeta = (String, Option<String>, Option<String>);

/// Parse a part's header block into its [`PartMeta`].
fn parse_headers(headers: &[u8]) -> Result<PartMeta, MultipartError> {
    let text = std::str::from_utf8(headers).map_err(|_| MultipartError::Malformed("non-UTF-8 part header"))?;
    let mut name = None;
    let mut filename = None;
    let mut content_type = None;

    for line in text.split("\r\n") {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "content-disposition" => {
                for param in value.split(';').skip(1) {
                    let param = param.trim();
                    if let Some(v) = param.strip_prefix("name=") {
                        name = Some(unquote(v));
                    } else if let Some(v) = param.strip_prefix("filename=") {
                        filename = Some(unquote(v));
                    }
                }
            }
            "content-type" => content_type = Some(value.to_string()),
            _ => {}
        }
    }

    Ok((name.ok_or(MultipartError::MissingName)?, filename, content_type))
}

/// Strip surrounding double quotes from a header parameter value.
fn unquote(v: &str) -> String {
    v.trim().trim_matches('"').to_string()
}

/// Advance past ASCII spaces/tabs.
fn skip_ws(body: &[u8], mut pos: usize) -> usize {
    while pos < body.len() && (body[pos] == b' ' || body[pos] == b'\t') {
        pos += 1;
    }
    pos
}

/// First index of `needle` in `haystack` at or after `from`.
fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| i + from)
}

/// A unique temp path for a spooled file part.
fn part_temp_path(dir: &std::path::Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!("kernway-part-{}-{}-{}.tmp", std::process::id(), nanos, n))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (name, filename, content_type, body) for one part to build.
    type PartSpec<'a> = (&'a str, Option<&'a str>, Option<&'a str>, &'a [u8]);

    /// Build a `multipart/form-data` body from part specs.
    fn build(boundary: &str, parts: &[PartSpec]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, filename, ct, body) in parts {
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            let mut cd = format!("Content-Disposition: form-data; name=\"{name}\"");
            if let Some(f) = filename {
                cd.push_str(&format!("; filename=\"{f}\""));
            }
            cd.push_str("\r\n");
            out.extend_from_slice(cd.as_bytes());
            if let Some(ct) = ct {
                out.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
            }
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(body);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        out
    }

    fn req_with(boundary: &str, body: Vec<u8>) -> Request {
        let mut req = Request::new("POST", "/upload");
        req.headers.insert("content-type", &format!("multipart/form-data; boundary={boundary}"));
        req.body = body;
        req
    }

    #[test]
    fn parses_a_text_field_and_a_file_part() {
        let body = build(
            "X1Y2",
            &[
                ("title", None, None, b"My Song"),
                ("cover", Some("c.png"), Some("image/png"), b"\x89PNGdata"),
            ],
        );
        let req = req_with("X1Y2", body);
        let mut mp = Multipart::from_request(&req).unwrap();

        let field = rt_core::Executor::new().unwrap().block_on(async { mp.next().await.unwrap() }).unwrap().unwrap();
        assert_eq!(field.name(), "title");
        assert!(!field.is_file());
        assert_eq!(field.text().unwrap(), "My Song");

        let file = rt_core::Executor::new().unwrap().block_on(async { mp.next().await.unwrap() }).unwrap().unwrap();
        assert_eq!(file.name(), "cover");
        assert_eq!(file.filename(), Some("c.png"));
        assert_eq!(file.content_type(), Some("image/png"));
        assert!(file.is_file());
        assert_eq!(file.bytes(), b"\x89PNGdata");
    }

    #[test]
    fn ends_after_the_last_part() {
        let body = build("B", &[("only", None, None, b"one")]);
        let req = req_with("B", body);
        let mut mp = Multipart::from_request(&req).unwrap();
        rt_core::Executor::new()
            .unwrap()
            .block_on(async {
                assert!(mp.next().await.unwrap().is_some());
                assert!(mp.next().await.unwrap().is_none(), "no part past the closing boundary");
            })
            .unwrap();
    }

    #[test]
    fn a_file_part_spools_and_persists() {
        let body = build("BND", &[("f", Some("s.bin"), None, b"song bytes here")]);
        let req = req_with("BND", body);
        let mut mp = Multipart::from_request(&req).unwrap();

        let dst = std::env::temp_dir().join(format!("kernway-mp-dst-{}.bin", std::process::id()));
        let dst2 = dst.clone();
        rt_core::Executor::new()
            .unwrap()
            .block_on(async move {
                let part = mp.next().await.unwrap().unwrap();
                let upload = part.file().await.unwrap();
                assert_eq!(upload.len(), 15);
                upload.persist(dst2).await.unwrap();
            })
            .unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"song bytes here");
        std::fs::remove_file(&dst).ok();
    }

    #[test]
    fn an_unpersisted_file_part_is_cleaned_up() {
        let body = build("Z", &[("f", Some("t.bin"), None, b"temp")]);
        let req = req_with("Z", body);
        let mut mp = Multipart::from_request(&req).unwrap();
        let path = rt_core::Executor::new()
            .unwrap()
            .block_on(async move {
                let part = mp.next().await.unwrap().unwrap();
                let upload = part.file().await.unwrap();
                upload.path().to_path_buf()
                // `upload` drops here without persist → its temp file must be removed
            })
            .unwrap();
        assert!(!path.exists(), "an un-persisted file part leaks its temp file");
    }

    #[test]
    fn a_non_multipart_request_is_rejected() {
        let mut req = Request::new("POST", "/x");
        req.headers.insert("content-type", "application/json");
        assert!(matches!(Multipart::from_request(&req), Err(MultipartError::NotMultipart)));
    }

    #[test]
    fn a_part_without_a_name_is_rejected() {
        // Hand-built: a part whose Content-Disposition has no name.
        let body = b"--B\r\nContent-Disposition: form-data\r\n\r\nx\r\n--B--\r\n".to_vec();
        let req = req_with("B", body);
        assert!(matches!(Multipart::from_request(&req), Err(MultipartError::MissingName)));
    }

    #[test]
    fn boundary_is_read_from_the_content_type() {
        assert_eq!(boundary_of("multipart/form-data; boundary=abc").as_deref(), Some("abc"));
        assert_eq!(boundary_of("multipart/form-data; boundary=\"a b\"").as_deref(), Some("a b"));
        assert_eq!(boundary_of("application/json"), None);
        assert_eq!(boundary_of("multipart/form-data"), None); // no boundary
    }
}
