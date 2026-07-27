//! HTTP/1.1 request parser — RFC 9112

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::net::TcpStream;

use kernway_core::fields::{Headers, QueryParams};
use kernway_core::request::{HttpVersion, Request};
use thiserror::Error;

/// Why a request could not be decoded.
///
/// Every variant is a client-visible failure: the connection is answered with a
/// 4xx and closed rather than left in an ambiguous state, since a half-parsed
/// request stream cannot be resynchronised.
#[derive(Debug, Error)]
pub enum ParseError {
    /// The underlying transport failed while reading.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The request line did not match `method SP request-target SP version`
    /// (RFC 9112 §3). Carries the offending line for the log.
    #[error("malformed request line: {0}")]
    BadRequestLine(String),
    /// The head exceeded [`MAX_HEAD_BYTES`] or the body [`MAX_BODY_BYTES`].
    ///
    /// Bounded on purpose: an unbounded head is a memory-exhaustion vector.
    #[error("request too large")]
    TooLarge,
}

/// Largest request head (request line + headers) accepted, in bytes.
pub const MAX_HEAD_BYTES: usize = 8 * 1024;

/// Largest body accepted, in bytes.
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Result of parsing a byte buffer that may not hold a whole request yet.
///
/// `Complete` is much larger than `Incomplete`; boxing it to even them out
/// would add a heap allocation to every request, which is the wrong trade for a
/// ~224-byte move on the hot path.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Parsed {
    /// A full request was decoded; `consumed` bytes may be dropped from the
    /// front of the buffer (anything beyond belongs to the next request).
    Complete {
        /// The decoded request.
        request: Request,
        /// Bytes consumed from the front of the buffer. Anything past this
        /// point belongs to the next pipelined request and must be kept.
        consumed: usize,
    },
    /// Not enough bytes yet — read more and call again.
    Incomplete,
}

/// Result of parsing only the head of a request (see [`parse_head`]).
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum ParsedHead {
    /// The head was decoded; the body (of `content_length` bytes) begins at `head_end`.
    Complete {
        /// The decoded request, with an empty body (the caller reads the body).
        request: Request,
        /// Byte offset where the body begins (just past the head's blank line).
        head_end: usize,
        /// The declared `Content-Length` (0 if absent).
        content_length: usize,
    },
    /// The head has not fully arrived yet — read more and call again.
    Incomplete,
}

/// Parse an HTTP/1.1 request out of a byte buffer.
///
/// This is the entry point for async transports: the caller owns the socket and
/// the read loop, and `kernway-http` stays free of any runtime dependency —
/// it never touches a socket, it decodes bytes.
///
/// Accepts LF as well as CRLF line endings, so a request typed by hand through
/// `nc` parses like one sent by `curl`.
pub fn parse_bytes(buf: &[u8]) -> Result<Parsed, ParseError> {
    let Some((mut request, head_end, content_length)) = parse_head_inner(buf)? else {
        return Ok(Parsed::Incomplete);
    };
    // The buffered path caps the body: this whole request lives in memory.
    if content_length > MAX_BODY_BYTES {
        return Err(ParseError::TooLarge);
    }
    let total = head_end + content_length;
    if buf.len() < total {
        return Ok(Parsed::Incomplete);
    }
    request.body = buf[head_end..total].to_vec();
    Ok(Parsed::Complete {
        request,
        consumed: total,
    })
}

/// Parse only the head (request line + headers), WITHOUT requiring the body. Returns the
/// decoded request (empty body), the offset where the body begins, and the declared
/// `Content-Length`, so the caller can decide how to read the body — buffer it, or stream
/// it to disk for a large upload. Unlike [`parse_bytes`] it does **not** cap the body:
/// the server enforces its own `max_upload_size`.
pub fn parse_head(buf: &[u8]) -> Result<ParsedHead, ParseError> {
    match parse_head_inner(buf)? {
        Some((request, head_end, content_length)) => Ok(ParsedHead::Complete {
            request,
            head_end,
            content_length,
        }),
        None => Ok(ParsedHead::Incomplete),
    }
}

/// Head-only decode shared by [`parse_bytes`] and [`parse_head`]. `Ok(None)` means the
/// head has not fully arrived yet. The returned request carries an empty body.
fn parse_head_inner(buf: &[u8]) -> Result<Option<(Request, usize, usize)>, ParseError> {
    let Some((head_end, line_count)) = find_head_end(buf) else {
        // No blank line yet. Bound the wait, or a client that never sends one
        // could grow this buffer without limit.
        if buf.len() > MAX_HEAD_BYTES {
            return Err(ParseError::TooLarge);
        }
        return Ok(None);
    };
    if head_end > MAX_HEAD_BYTES {
        return Err(ParseError::TooLarge);
    }

    let head = std::str::from_utf8(&buf[..head_end])
        .map_err(|_| ParseError::BadRequestLine("head is not valid UTF-8".into()))?;
    let mut lines = head.lines();

    let request_line = lines.next().unwrap_or("").trim();
    let (method, path, query, version) = parse_request_line(request_line)?;

    // One allocation for the whole set, sized from the head we already have in
    // front of us — see the note on `Headers` for why this is not a `HashMap`.
    // `append` rather than `insert`: the head is walked in order, so a repeated
    // header resolves to the last one without a duplicate check per line.
    let mut headers = Headers::with_capacity(head.len(), line_count.saturating_sub(1));
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = split_header(trimmed) {
            headers.append(name, value);
        }
    }

    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let request = Request {
        method,
        path,
        version,
        headers,
        query,
        path_params: HashMap::new(), // filled by the router
        body: Vec::new(),
        body_spool: None,
        remote_addr: None, // set by the server from the socket peer
    };
    Ok(Some((request, head_end, content_length)))
}

/// Byte offset just past the blank line that ends the head, plus how many lines
/// preceded it — if the head has arrived at all.
///
/// The line count comes out of this scan for free, and it is exactly the header
/// capacity the caller needs. Counting `\n` again afterwards would walk the
/// whole head a second time to learn something already known here.
fn find_head_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut pos = 0;
    let mut lines = 0;
    while let Some(offset) = buf[pos..].iter().position(|&b| b == b'\n') {
        let end = pos + offset;
        let line = buf[pos..end].strip_suffix(b"\r").unwrap_or(&buf[pos..end]);
        if line.is_empty() {
            return Some((end + 1, lines));
        }
        lines += 1;
        pos = end + 1;
    }
    None
}

/// `GET /path?query HTTP/1.1` → method, path, query pairs, version.
type RequestLine = (String, String, QueryParams, HttpVersion);

fn parse_request_line(line: &str) -> Result<RequestLine, ParseError> {
    // Walked with the iterator rather than collected: `collect::<Vec<_>>` puts a
    // heap allocation on the request-line path for three string slices.
    let mut parts = line.splitn(3, ' ');
    let (Some(method), Some(full_path)) = (parts.next(), parts.next()) else {
        return Err(ParseError::BadRequestLine(line.to_string()));
    };
    if method.is_empty() {
        return Err(ParseError::BadRequestLine(line.to_string()));
    }
    let (path, query_str) = match full_path.find('?') {
        Some(q) => (&full_path[..q], &full_path[q + 1..]),
        None => (full_path, ""),
    };
    // A missing version means HTTP/0.9, which nobody speaks any more; treating
    // it as 1.1 would wrongly hold the connection open, so it falls to 1.0.
    let version = match parts.next().map(str::trim) {
        Some("HTTP/1.0") | None => HttpVersion::Http10,
        _ => HttpVersion::Http11,
    };
    Ok((
        // ASCII-only: RFC 9110 §9 defines methods as tokens, so the Unicode
        // case tables `to_uppercase` consults can only cost time here.
        method.to_ascii_uppercase(),
        path.to_string(),
        parse_query_string(query_str),
        version,
    ))
}

/// `Name: value` → `("Name", "value")`, borrowed from `line`.
///
/// Nothing is copied or lowercased here: `Headers` owns the one buffer these
/// end up in and lowercases the name as it copies. Returning `String`s would
/// put two allocations per header back on the path this exists to keep clear.
fn split_header(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    Some((line[..colon].trim(), line[colon + 1..].trim()))
}

/// Parse an HTTP/1.1 request from a blocking TcpStream.
///
/// Retained for tooling and tests that already hold a std socket; the server
/// itself is async and goes through [`parse_bytes`].
pub fn parse_request(stream: &TcpStream) -> Result<Request, ParseError> {
    parse_from_reader(BufReader::new(stream))
}

/// Parse an HTTP/1.1 request from any `BufRead` — useful for testing.
pub fn parse_from_reader<R: BufRead>(mut reader: R) -> Result<Request, ParseError> {
    // --- Request line: "GET /path?query HTTP/1.1\r\n" ---
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let request_line = request_line.trim();

    let parts: Vec<&str> = request_line.splitn(3, ' ').collect();
    if parts.len() < 2 {
        return Err(ParseError::BadRequestLine(request_line.to_string()));
    }
    let method = parts[0].to_uppercase();
    let full_path = parts[1];
    let version = match parts.get(2).map(|v| v.trim()) {
        Some("HTTP/1.0") | None => HttpVersion::Http10,
        _ => HttpVersion::Http11,
    };

    // Split the path and query string
    let (path, query_str) = match full_path.find('?') {
        Some(q) => (&full_path[..q], &full_path[q + 1..]),
        None => (full_path, ""),
    };

    let query = parse_query_string(query_str);

    // --- Headers ---
    let mut headers = Headers::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = split_header(trimmed) {
            headers.append(name, value);
        }
    }

    // --- Body (based on Content-Length) ---
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    // 10 MB limit
    if content_length > 10 * 1024 * 1024 {
        return Err(ParseError::TooLarge);
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body)?;
    }

    Ok(Request {
        method,
        path: path.to_string(),
        version,
        headers,
        query,
        path_params: HashMap::new(), // filled by router
        body,
        body_spool: None,
        remote_addr: None, // set by the server from the socket peer
    })
}

/// `a=1&b=2` → the pairs, sharing one buffer.
///
/// `append` rather than `insert` for the same reason the header loop uses it:
/// the string is walked in order, so a repeated parameter resolves to the last
/// one through `QueryParams::get` without a duplicate check per pair.
fn parse_query_string(query: &str) -> QueryParams {
    // An empty query allocates nothing — `Vec::with_capacity(0)` does not.
    let mut params = QueryParams::with_capacity(query.len(), estimated_params(query));
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, val) = match pair.find('=') {
            Some(eq) => (&pair[..eq], &pair[eq + 1..]),
            None => (pair, ""),
        };
        if !key.is_empty() {
            params.append(key, val);
        }
    }
    params
}

/// One more than the number of separators — exact for a well-formed query and
/// harmless when it isn't.
fn estimated_params(query: &str) -> usize {
    if query.is_empty() {
        return 0;
    }
    query.bytes().filter(|&b| b == b'&').count() + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(raw: &str) -> Result<Request, ParseError> {
        parse_from_reader(Cursor::new(raw.as_bytes()))
    }

    #[test]
    fn parse_simple_get() {
        let req = parse("GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/health");
        assert!(req.body.is_empty());
    }

    #[test]
    fn parse_path_with_query_string() {
        let req = parse("GET /search?q=rust&page=2 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(req.path, "/search");
        assert_eq!(req.query.get("q"), Some("rust"));
        assert_eq!(req.query.get("page"), Some("2"));
    }

    #[test]
    fn parse_post_with_body() {
        let body = r#"{"name":"Alice"}"#;
        let raw = format!(
            "POST /users HTTP/1.1\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
            body.len(),
            body
        );
        let req = parse(&raw).unwrap();
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/users");
        assert_eq!(req.body, body.as_bytes());
    }

    #[test]
    fn parse_headers_lowercased() {
        let req =
            parse("GET / HTTP/1.1\r\nAuthorization: Bearer token123\r\nX-Custom: value\r\n\r\n")
                .unwrap();
        assert_eq!(req.headers.get("authorization").unwrap(), "Bearer token123");
        assert_eq!(req.headers.get("x-custom").unwrap(), "value");
    }

    #[test]
    fn parse_bad_request_line_returns_error() {
        let result = parse("BADLINE\r\n\r\n");
        assert!(matches!(result, Err(ParseError::BadRequestLine(_))));
    }

    #[test]
    fn parse_query_string_empty() {
        let req = parse("GET /ping HTTP/1.1\r\n\r\n").unwrap();
        assert!(req.query.is_empty());
    }

    #[test]
    fn parse_query_string_no_value() {
        let req = parse("GET /items?flag HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!(req.query.get("flag"), Some(""));
    }
}

#[cfg(test)]
mod byte_parser_tests {
    use super::*;

    fn complete(raw: &str) -> (Request, usize) {
        match parse_bytes(raw.as_bytes()).unwrap() {
            Parsed::Complete { request, consumed } => (request, consumed),
            Parsed::Incomplete => panic!("expected a complete request from {raw:?}"),
        }
    }

    #[test]
    fn parses_a_whole_request_in_one_buffer() {
        let raw = "GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let (req, consumed) = complete(raw);
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/health");
        assert_eq!(req.headers.get("host"), Some("localhost"));
        assert_eq!(
            consumed,
            raw.len(),
            "a bodyless request consumes exactly its head"
        );
    }

    #[test]
    fn parse_head_completes_before_the_body_arrives() {
        // Head present, body (content-length 100) not yet — parse_head is Complete while
        // parse_bytes is still Incomplete (it waits for the whole body).
        let buf = b"POST /up HTTP/1.1\r\ncontent-length: 100\r\n\r\n";
        match parse_head(buf).unwrap() {
            ParsedHead::Complete {
                request,
                head_end,
                content_length,
            } => {
                assert_eq!(content_length, 100);
                assert_eq!(head_end, buf.len());
                assert!(
                    request.body.is_empty(),
                    "the head-only request carries no body"
                );
            }
            ParsedHead::Incomplete => panic!("the head is complete"),
        }
        assert!(matches!(parse_bytes(buf).unwrap(), Parsed::Incomplete));
    }

    #[test]
    fn head_split_across_reads_is_incomplete_until_the_blank_line() {
        assert!(matches!(
            parse_bytes(b"GET /health HTTP/1.1\r\nHost: loc").unwrap(),
            Parsed::Incomplete
        ));
        assert!(matches!(
            parse_bytes(b"GET /health HTTP/1.1\r\nHost: localhost\r\n").unwrap(),
            Parsed::Incomplete
        ));
    }

    #[test]
    fn body_split_across_reads_is_incomplete_until_content_length_arrives() {
        let head = "POST /users HTTP/1.1\r\ncontent-length: 10\r\n\r\n";
        assert!(matches!(
            parse_bytes(format!("{head}12345").as_bytes()).unwrap(),
            Parsed::Incomplete
        ));
        let (req, _) = complete(&format!("{head}1234567890"));
        assert_eq!(req.body, b"1234567890");
    }

    #[test]
    fn consumed_stops_at_the_end_of_this_request() {
        // Pipelined requests: the second must be left in the buffer untouched.
        let raw = "GET /a HTTP/1.1\r\n\r\nGET /b HTTP/1.1\r\n\r\n";
        let (first, consumed) = complete(raw);
        assert_eq!(first.path, "/a");
        let (second, _) = complete(&raw[consumed..]);
        assert_eq!(second.path, "/b");
    }

    #[test]
    fn lf_only_line_endings_are_accepted() {
        // What a hand-typed `nc` session sends.
        let (req, _) = complete("GET /ping HTTP/1.1\nHost: x\n\n");
        assert_eq!(req.path, "/ping");
        assert_eq!(req.headers.get("host"), Some("x"));
    }

    #[test]
    fn query_string_is_decoded_into_pairs() {
        let (req, _) = complete("GET /search?q=rust&page=2 HTTP/1.1\r\n\r\n");
        assert_eq!(req.path, "/search");
        assert_eq!(req.query.get("q"), Some("rust"));
        assert_eq!(req.query.get("page"), Some("2"));
    }

    #[test]
    fn a_head_that_never_ends_is_rejected_rather_than_buffered_forever() {
        let flood = vec![b'x'; MAX_HEAD_BYTES + 1];
        assert!(matches!(parse_bytes(&flood), Err(ParseError::TooLarge)));
    }

    #[test]
    fn an_oversized_content_length_is_rejected_before_allocating() {
        let raw = format!(
            "POST /upload HTTP/1.1\r\ncontent-length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert!(matches!(
            parse_bytes(raw.as_bytes()),
            Err(ParseError::TooLarge)
        ));
    }

    #[test]
    fn a_malformed_request_line_is_an_error_not_a_hang() {
        assert!(matches!(
            parse_bytes(b"BADLINE\r\n\r\n"),
            Err(ParseError::BadRequestLine(_))
        ));
    }

    #[test]
    fn method_is_normalised_to_uppercase() {
        let (req, _) = complete("post /x HTTP/1.1\r\n\r\n");
        assert_eq!(req.method, "POST");
    }
}
