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
}

impl StaticFiles {
    /// Serve files from `root`, using `index.html` for directory requests.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), index: "index.html".to_string() }
    }

    /// Use a different index file name than `index.html`.
    pub fn index(mut self, name: impl Into<String>) -> Self {
        self.index = name.into();
        self
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

        if decoded.bytes().any(|b| b == 0 || b == b'\\' || b.is_ascii_control()) {
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
        _ => "application/octet-stream",
    }
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
        assert_eq!(sf().resolve("/style.css"), Ok(PathBuf::from("public/style.css")));
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
        assert_eq!(sf().resolve("/docs/"), Ok(PathBuf::from("public/docs/index.html")));
    }

    #[test]
    fn the_index_name_is_configurable() {
        let sf = StaticFiles::new("public").index("home.html");
        assert_eq!(sf.resolve("/"), Ok(PathBuf::from("public/home.html")));
    }

    #[test]
    fn a_dot_segment_is_skipped_not_rejected() {
        assert_eq!(sf().resolve("/./style.css"), Ok(PathBuf::from("public/style.css")));
    }

    #[test]
    fn double_slashes_collapse() {
        assert_eq!(sf().resolve("//a///b.css"), Ok(PathBuf::from("public/a/b.css")));
    }

    // --- traversal, in every spelling -------------------------------------

    #[test]
    fn a_parent_segment_is_rejected() {
        assert_eq!(sf().resolve("/../etc/passwd"), Err(Rejected::Traversal));
    }

    #[test]
    fn a_parent_segment_in_the_middle_is_rejected() {
        assert_eq!(sf().resolve("/a/../../etc/passwd"), Err(Rejected::Traversal));
    }

    #[test]
    fn percent_encoded_traversal_is_rejected() {
        // %2e is '.', so %2e%2e%2f is "../" — this must not slip past.
        assert_eq!(sf().resolve("/%2e%2e/etc/passwd"), Err(Rejected::Traversal));
        assert_eq!(sf().resolve("/%2e%2e%2fetc/passwd"), Err(Rejected::Traversal));
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
        assert_eq!(mime_for(Path::new("public/index.html")), "text/html; charset=utf-8");
    }

    #[test]
    fn css_and_js_are_typed() {
        assert_eq!(mime_for(Path::new("a.css")), "text/css; charset=utf-8");
        assert_eq!(mime_for(Path::new("a.js")), "text/javascript; charset=utf-8");
    }

    #[test]
    fn an_unknown_extension_is_octet_stream() {
        assert_eq!(mime_for(Path::new("a.xyz")), "application/octet-stream");
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert_eq!(mime_for(Path::new("PHOTO.JPG")), "image/jpeg");
    }
}
