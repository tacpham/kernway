//! # kernway-static
//!
//! Turns a request path into a safe file path, and names its MIME type. That is
//! all — there is no I/O here.
//!
//! The split is deliberate. Path safety is pure logic and the place a directory
//! traversal is stopped, so it is testable without a filesystem and every attack
//! string is a unit test. The file read is async I/O and belongs to
//! `kernway-server`, where it runs on the blocking pool so it never stalls a core
//! ([KEP-0000 §4](../../kep/0000-principles.md)).
//!
//! ```
//! use kernway_static::StaticFiles;
//! use std::path::PathBuf;
//!
//! let sf = StaticFiles::new("public");
//!
//! // A normal path resolves under the root.
//! assert_eq!(sf.resolve("/style.css"), Ok(PathBuf::from("public/style.css")));
//!
//! // "/" becomes the index file.
//! assert_eq!(sf.resolve("/"), Ok(PathBuf::from("public/index.html")));
//!
//! // Traversal is rejected, in every spelling.
//! assert!(sf.resolve("/../etc/passwd").is_err());
//! assert!(sf.resolve("/%2e%2e/etc/passwd").is_err());
//! assert!(sf.resolve("/.env").is_err());
//! ```

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

/// Why a request path was refused.
///
/// Every variant is a `404` to the client — a rejected path must not reveal
/// whether the target exists, so all refusals look identical from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// A `..` segment — an attempt to climb out of the root.
    Traversal,
    /// A segment beginning with `.` — `.env`, `.git`, and friends. Denied as a
    /// class rather than by a blocklist.
    Dotfile,
    /// A NUL byte, a backslash, or a control character — never valid in a path
    /// we will open, and usually an attempt to confuse the check.
    IllegalByte,
    /// Percent-encoding that is not two hex digits.
    BadEncoding,
}

/// A static file root: a directory on disk and the file to serve for `/`.
#[derive(Debug, Clone)]
pub struct StaticFiles {
    root: PathBuf,
    index: String,
    precompressed: bool,
}

impl StaticFiles {
    /// Serve files from `root`, using `index.html` for directory requests.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            index: "index.html".to_string(),
            precompressed: false,
        }
    }

    /// Use a different index file name than `index.html`.
    pub fn index(mut self, name: impl Into<String>) -> Self {
        self.index = name.into();
        self
    }

    /// Serve a precompressed `.br`/`.gz` sitting next to a file when the client
    /// accepts it and the file is a [compressible][is_compressible] type.
    ///
    /// Off by default, and deliberately so: with it on, every request for a
    /// compressible asset does one or two extra `stat`s probing for a variant,
    /// which is wasted work unless a build step actually produced them. Turn it
    /// on once your deploy ships `app.js.br` beside `app.js`. When on, responses
    /// carry `Vary: Accept-Encoding` so a shared cache keys on the encoding.
    pub fn precompressed(mut self) -> Self {
        self.precompressed = true;
        self
    }

    /// Whether precompressed variants are served — see [`precompressed`](Self::precompressed).
    pub fn serves_precompressed(&self) -> bool {
        self.precompressed
    }

    /// The configured root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Map a request path to a file path under the root, or reject it.
    ///
    /// This is the security boundary. It runs before any I/O, so a path that
    /// would escape the root never reaches the filesystem. The steps, in order:
    ///
    /// 1. Percent-decode, so `%2e%2e` cannot smuggle a `..` past the check.
    /// 2. Reject NUL, control bytes, and `\` — none belong in a path we open.
    /// 3. Split on `/` and inspect every segment: `..` is traversal, a leading
    ///    `.` is a dotfile, `.` and empty segments are skipped.
    /// 4. Join the survivors onto the root. Because no `..` survives step 3, the
    ///    result cannot climb above the root — this is lexical containment, and
    ///    it holds without touching disk.
    /// 5. A trailing `/` (or the bare root) appends the index file.
    ///
    /// # Not covered here
    /// Symlinks. A file *inside* the root that links *outside* it is not caught
    /// by lexical checks — that needs a canonicalize-and-re-check at open time,
    /// and lives with the I/O in `kernway-server` (a `kernway-static` TODO for
    /// M2). Until then, do not serve a root that contains untrusted symlinks.
    pub fn resolve(&self, url_path: &str) -> Result<PathBuf, Rejected> {
        let decoded = percent_decode(url_path)?;

        if decoded
            .bytes()
            .any(|b| b == 0 || b == b'\\' || b.is_ascii_control())
        {
            return Err(Rejected::IllegalByte);
        }

        let mut path = self.root.clone();
        let mut had_named_segment = false;

        for segment in decoded.split('/') {
            match segment {
                "" | "." => continue,
                ".." => return Err(Rejected::Traversal),
                s if s.starts_with('.') => return Err(Rejected::Dotfile),
                s => {
                    path.push(s);
                    had_named_segment = true;
                }
            }
        }

        // A directory request ("/", "/docs/") names no file — serve the index.
        // `had_named_segment` distinguishes "/" from "/style.css": both may end
        // without a trailing slash after splitting, but only the directory case
        // has nothing to push.
        let is_directory_request = decoded.ends_with('/') || !had_named_segment;
        if is_directory_request {
            path.push(&self.index);
        }

        Ok(path)
    }
}

/// The MIME type for a path's extension, or `application/octet-stream`.
///
/// A small hand-written table rather than a dependency (KEP-0000 §1). It covers
/// what a web UI actually serves; extend it as needed rather than reaching for a
/// crate. `charset=utf-8` is stated on text types so a browser does not guess.
///
/// The returned type is meant to be sent verbatim as `Content-Type`, always
/// alongside `X-Content-Type-Options: nosniff` so the browser trusts it rather
/// than sniffing the bytes.
pub fn mime_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);

    match ext.as_deref() {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("ttf") => "font/ttf",
        Some("txt") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("pdf") => "application/pdf",
        // Audio — the media a `<audio>` player streams (Range-served).
        Some("mp3") => "audio/mpeg",
        Some("m4a" | "m4b" | "aac") => "audio/mp4",
        Some("oga" | "ogg") => "audio/ogg",
        Some("opus") => "audio/opus",
        Some("wav") => "audio/wav",
        Some("flac") => "audio/flac",
        // Video.
        Some("mp4" | "m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        _ => "application/octet-stream",
    }
}

/// A content encoding Kernway can serve from a **precompressed file on disk**.
///
/// The server never compresses on the request path (that would spend CPU per
/// request, [KEP-0000 §4](../../kep/0000-principles.md)); it serves a `.br`/`.gz`
/// that a build step produced. So this enum names only the two encodings worth
/// shipping that way, in the order the server prefers them: Brotli first (it
/// compresses text ~15–20% smaller than gzip), gzip as the fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Brotli — `Content-Encoding: br`, variant file `*.br`.
    Brotli,
    /// gzip — `Content-Encoding: gzip`, variant file `*.gz`.
    Gzip,
}

impl Encoding {
    /// Server preference order, best first. Brotli beats gzip on text.
    pub const PREFERENCE: [Encoding; 2] = [Encoding::Brotli, Encoding::Gzip];

    /// The `Content-Encoding` token to send.
    pub fn token(self) -> &'static str {
        match self {
            Encoding::Brotli => "br",
            Encoding::Gzip => "gzip",
        }
    }

    /// The filename suffix of the precompressed variant (`.br` / `.gz`).
    pub fn extension(self) -> &'static str {
        match self {
            Encoding::Brotli => ".br",
            Encoding::Gzip => ".gz",
        }
    }
}

/// The encodings a client accepts, in **server preference order** (Brotli, then
/// gzip) — the order to try variant files in.
///
/// Parses `Accept-Encoding` by RFC 9110 §12.5.3: a `;q=0` explicitly refuses an
/// encoding, a `*` sets the default for anything unlisted, and an exact token
/// beats the wildcard. An empty result means "no acceptable precompressed
/// encoding — send the file as-is."
///
/// ```
/// use kernway_static::{accepted_encodings, Encoding};
///
/// assert_eq!(accepted_encodings("br, gzip"), vec![Encoding::Brotli, Encoding::Gzip]);
/// assert_eq!(accepted_encodings("gzip"), vec![Encoding::Gzip]);
/// assert_eq!(accepted_encodings("gzip;q=0"), vec![]);       // refused
/// assert_eq!(accepted_encodings("identity"), vec![]);       // no compressed form wanted
/// ```
pub fn accepted_encodings(accept_encoding: &str) -> Vec<Encoding> {
    Encoding::PREFERENCE
        .into_iter()
        .filter(|e| is_accepted(accept_encoding, e.token()))
        .collect()
}

/// Whether `token` (e.g. `"br"`) has a positive q-value in the header.
fn is_accepted(header: &str, token: &str) -> bool {
    let mut exact_q: Option<f32> = None;
    let mut wildcard_q: Option<f32> = None;
    for part in header.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (name, q) = split_qvalue(part);
        if name.eq_ignore_ascii_case(token) {
            exact_q = Some(q);
        } else if name == "*" {
            wildcard_q = Some(q);
        }
    }
    // An exact match wins over the wildcard; unlisted with no `*` is not accepted.
    exact_q.or(wildcard_q).unwrap_or(0.0) > 0.0
}

/// Split `"gzip;q=0.8"` into (`"gzip"`, `0.8`). A missing or malformed `q`
/// defaults to `1.0`, per the spec's "quality 1 unless stated".
fn split_qvalue(part: &str) -> (&str, f32) {
    match part.split_once(';') {
        None => (part, 1.0),
        Some((name, params)) => {
            let q = params
                .split(';')
                .find_map(|p| {
                    let p = p.trim();
                    let v = p.strip_prefix("q=").or_else(|| p.strip_prefix("Q="))?;
                    v.trim().parse::<f32>().ok()
                })
                .unwrap_or(1.0);
            (name.trim(), q)
        }
    }
}

/// Whether a MIME type is worth serving precompressed.
///
/// The answer to "what can be precompressed": the **text tier**. Text markup and
/// code shrink 60–90% under gzip/brotli, so a `.br`/`.gz` is a large win. Binary
/// media (`image/png`, `image/jpeg`, `image/webp`, `font/woff2`, …) is *already*
/// compressed — a second pass gains nothing and often grows the file — so the
/// server does not even look for a variant of it, saving a `stat` per request.
///
/// Kept as a tiny local predicate rather than importing `kernway_compress`: this
/// crate is zero-dependency by design (KEP-0000 §1), and pulling in the compression
/// *codecs* just to share a 15-line pure function is the wrong trade. The server's
/// compression middleware has the canonical copy; the two agree by construction.
///
/// ```
/// use kernway_static::is_compressible;
///
/// assert!(is_compressible("text/html; charset=utf-8"));
/// assert!(is_compressible("application/json"));
/// assert!(is_compressible("image/svg+xml"));
/// assert!(!is_compressible("image/png"));
/// assert!(!is_compressible("font/woff2"));
/// ```
pub fn is_compressible(mime: &str) -> bool {
    // Only the type/subtype matters; drop a `; charset=…` parameter.
    let base = mime.split(';').next().unwrap_or(mime).trim();
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
            | "image/svg+xml"
            | "font/ttf"
            | "font/otf"
    )
}

/// Build an `ETag` for a file from its length and modification time.
///
/// A validator, not a hash: `"{len:x}-{mtime:x}"`, the same shape nginx uses.
/// Two different contents at the same size and mtime would collide, which in
/// practice does not happen for files a server hands out — and the cost of
/// hashing every file on every request would. The value includes the quotes, so
/// it is ready to place in an `ETag` header verbatim.
///
/// `mtime_nanos` is nanoseconds since the Unix epoch; a backend that cannot read
/// an mtime passes 0, and the ETag then varies by size alone.
pub fn etag(len: u64, mtime_nanos: u128) -> String {
    format!("\"{len:x}-{mtime_nanos:x}\"")
}

/// Whether an `If-None-Match` header value matches `etag`.
///
/// Handles the three shapes a client sends: `*` (matches anything), a
/// comma-separated list, and a weak validator `W/"..."`. Comparison is weak per
/// RFC 9110 §13.1.2 — the `W/` prefix is ignored on both sides — which is the
/// correct rule for `If-None-Match`. `etag` is expected to be the value
/// [`etag`] produced (strong, quoted).
pub fn etag_matches(if_none_match: &str, etag: &str) -> bool {
    let inm = if_none_match.trim();
    if inm == "*" {
        return true;
    }
    let want = etag.strip_prefix("W/").unwrap_or(etag);
    inm.split(',').any(|candidate| {
        let c = candidate.trim();
        c.strip_prefix("W/").unwrap_or(c) == want
    })
}

/// Percent-decode a URL path segment string.
///
/// `%2e` → `.`, `%2F` → `/`, and so on. A decoded `/` is left in the string so
/// that step 3 of [`resolve`](StaticFiles::resolve) still sees it as a separator
/// — the point of decoding first is precisely that `%2e%2e%2f` must be caught as
/// `../`, not waved through as an opaque segment.
fn percent_decode(s: &str) -> Result<String, Rejected> {
    if !s.contains('%') {
        return Ok(s.to_string());
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = bytes.get(i + 1).and_then(|b| hex_val(*b));
            let lo = bytes.get(i + 2).and_then(|b| hex_val(*b));
            match (hi, lo) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 3;
                }
                _ => return Err(Rejected::BadEncoding),
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // A percent sequence could decode to invalid UTF-8; reject rather than
    // lossily replace, since a mangled path should not resolve to anything.
    String::from_utf8(out).map_err(|_| Rejected::IllegalByte)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sf() -> StaticFiles {
        StaticFiles::new("public")
    }

    // --- normal resolution -------------------------------------------------

    #[test]
    fn a_plain_file_resolves_under_the_root() {
        assert_eq!(
            sf().resolve("/style.css"),
            Ok(PathBuf::from("public/style.css"))
        );
    }

    #[test]
    fn a_nested_file_resolves() {
        assert_eq!(
            sf().resolve("/assets/app.js"),
            Ok(PathBuf::from("public/assets/app.js"))
        );
    }

    #[test]
    fn the_bare_root_serves_the_index() {
        assert_eq!(sf().resolve("/"), Ok(PathBuf::from("public/index.html")));
    }

    #[test]
    fn a_trailing_slash_serves_the_index_of_that_directory() {
        assert_eq!(
            sf().resolve("/docs/"),
            Ok(PathBuf::from("public/docs/index.html"))
        );
    }

    #[test]
    fn the_index_name_is_configurable() {
        let sf = StaticFiles::new("public").index("home.html");
        assert_eq!(sf.resolve("/"), Ok(PathBuf::from("public/home.html")));
    }

    #[test]
    fn a_dot_segment_is_skipped_not_rejected() {
        assert_eq!(
            sf().resolve("/./style.css"),
            Ok(PathBuf::from("public/style.css"))
        );
    }

    #[test]
    fn double_slashes_collapse() {
        assert_eq!(
            sf().resolve("//a///b.css"),
            Ok(PathBuf::from("public/a/b.css"))
        );
    }

    // --- traversal, in every spelling -------------------------------------

    #[test]
    fn a_parent_segment_is_rejected() {
        assert_eq!(sf().resolve("/../etc/passwd"), Err(Rejected::Traversal));
    }

    #[test]
    fn a_parent_segment_in_the_middle_is_rejected() {
        assert_eq!(
            sf().resolve("/a/../../etc/passwd"),
            Err(Rejected::Traversal)
        );
    }

    #[test]
    fn percent_encoded_traversal_is_rejected() {
        // %2e is '.', so %2e%2e%2f is "../" — this must not slip past.
        assert_eq!(sf().resolve("/%2e%2e/etc/passwd"), Err(Rejected::Traversal));
        assert_eq!(
            sf().resolve("/%2e%2e%2fetc/passwd"),
            Err(Rejected::Traversal)
        );
    }

    #[test]
    fn mixed_case_percent_encoding_decodes() {
        assert_eq!(sf().resolve("/%2E%2e/x"), Err(Rejected::Traversal));
    }

    // --- dotfiles ----------------------------------------------------------

    #[test]
    fn a_dotfile_is_rejected() {
        assert_eq!(sf().resolve("/.env"), Err(Rejected::Dotfile));
    }

    #[test]
    fn a_dot_directory_is_rejected() {
        assert_eq!(sf().resolve("/.git/config"), Err(Rejected::Dotfile));
    }

    #[test]
    fn a_dotfile_deeper_in_the_tree_is_rejected() {
        assert_eq!(sf().resolve("/assets/.secret"), Err(Rejected::Dotfile));
    }

    // --- illegal bytes -----------------------------------------------------

    #[test]
    fn a_nul_byte_is_rejected() {
        assert_eq!(sf().resolve("/a%00b"), Err(Rejected::IllegalByte));
    }

    #[test]
    fn a_backslash_is_rejected() {
        // On Windows a backslash is a separator; reject it everywhere so the
        // check behaves the same on every platform.
        assert_eq!(sf().resolve("/..\\..\\x"), Err(Rejected::IllegalByte));
    }

    #[test]
    fn a_bare_percent_is_rejected() {
        assert_eq!(sf().resolve("/a%"), Err(Rejected::BadEncoding));
        assert_eq!(sf().resolve("/a%zz"), Err(Rejected::BadEncoding));
    }

    // --- mime --------------------------------------------------------------

    #[test]
    fn html_is_utf8_typed() {
        assert_eq!(
            mime_for(Path::new("public/index.html")),
            "text/html; charset=utf-8"
        );
    }

    #[test]
    fn css_and_js_are_typed() {
        assert_eq!(mime_for(Path::new("a.css")), "text/css; charset=utf-8");
        assert_eq!(
            mime_for(Path::new("a.js")),
            "text/javascript; charset=utf-8"
        );
    }

    #[test]
    fn an_unknown_extension_is_octet_stream() {
        assert_eq!(mime_for(Path::new("a.xyz")), "application/octet-stream");
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert_eq!(mime_for(Path::new("PHOTO.JPG")), "image/jpeg");
    }

    // --- etag --------------------------------------------------------------

    #[test]
    fn etag_is_quoted_and_hex() {
        assert_eq!(etag(255, 16), "\"ff-10\"");
    }

    #[test]
    fn etag_changes_with_length_or_mtime() {
        assert_ne!(etag(100, 1), etag(101, 1));
        assert_ne!(etag(100, 1), etag(100, 2));
    }

    #[test]
    fn if_none_match_exact() {
        let e = etag(100, 5);
        assert!(etag_matches(&e, &e));
        assert!(!etag_matches("\"deadbeef-0\"", &e));
    }

    #[test]
    fn if_none_match_star_matches_anything() {
        assert!(etag_matches("*", &etag(1, 1)));
    }

    #[test]
    fn if_none_match_list() {
        let e = etag(100, 5);
        let header = format!("\"other-1\", {e}, \"another-2\"");
        assert!(etag_matches(&header, &e));
    }

    #[test]
    fn if_none_match_ignores_weak_prefix() {
        let e = etag(100, 5);
        assert!(etag_matches(&format!("W/{e}"), &e));
    }

    // --- content negotiation ----------------------------------------------

    #[test]
    fn brotli_is_preferred_over_gzip_regardless_of_client_order() {
        // The server's preference wins, not the client's listing order.
        assert_eq!(
            accepted_encodings("gzip, br"),
            vec![Encoding::Brotli, Encoding::Gzip]
        );
        assert_eq!(
            accepted_encodings("br, gzip"),
            vec![Encoding::Brotli, Encoding::Gzip]
        );
    }

    #[test]
    fn only_the_accepted_encodings_are_returned() {
        assert_eq!(accepted_encodings("gzip"), vec![Encoding::Gzip]);
        assert_eq!(accepted_encodings("br"), vec![Encoding::Brotli]);
        assert_eq!(accepted_encodings("deflate"), vec![]);
        assert_eq!(accepted_encodings(""), vec![]);
    }

    #[test]
    fn a_q_zero_refuses_an_encoding() {
        // `gzip;q=0` means "do not send gzip", even though the token is present.
        assert_eq!(accepted_encodings("br, gzip;q=0"), vec![Encoding::Brotli]);
        assert_eq!(accepted_encodings("gzip;q=0"), vec![]);
    }

    #[test]
    fn a_wildcard_enables_unlisted_encodings() {
        assert_eq!(
            accepted_encodings("*"),
            vec![Encoding::Brotli, Encoding::Gzip]
        );
        // …but an explicit token overrides the wildcard.
        assert_eq!(accepted_encodings("*, br;q=0"), vec![Encoding::Gzip]);
        assert_eq!(accepted_encodings("*;q=0, gzip"), vec![Encoding::Gzip]);
    }

    #[test]
    fn negotiation_tolerates_whitespace_and_case() {
        assert_eq!(
            accepted_encodings("  BR ; q=0.9 ,  GZIP "),
            vec![Encoding::Brotli, Encoding::Gzip]
        );
    }

    #[test]
    fn encoding_tokens_and_extensions() {
        assert_eq!(Encoding::Brotli.token(), "br");
        assert_eq!(Encoding::Brotli.extension(), ".br");
        assert_eq!(Encoding::Gzip.token(), "gzip");
        assert_eq!(Encoding::Gzip.extension(), ".gz");
    }

    // --- compressibility ---------------------------------------------------

    #[test]
    fn text_tier_is_compressible() {
        for m in [
            "text/html; charset=utf-8",
            "text/css; charset=utf-8",
            "text/javascript; charset=utf-8",
            "application/json; charset=utf-8",
            "application/xml; charset=utf-8",
            "application/wasm",
            "image/svg+xml",
            "font/ttf",
        ] {
            assert!(is_compressible(m), "{m} should be compressible");
        }
    }

    #[test]
    fn already_compressed_media_is_not() {
        for m in [
            "image/png",
            "image/jpeg",
            "image/webp",
            "image/gif",
            "font/woff2",
            "font/woff",
            "application/pdf",
            "application/octet-stream",
        ] {
            assert!(!is_compressible(m), "{m} should not be compressible");
        }
    }
}
