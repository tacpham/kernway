//! HTTP/1.1 request parser — RFC 9112

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::net::TcpStream;

use kernway_core::request::Request;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("malformed request line: {0}")]
    BadRequestLine(String),
    #[error("request too large")]
    TooLarge,
}

/// Parse an HTTP/1.1 request from TcpStream.
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
    let method    = parts[0].to_uppercase();
    let full_path = parts[1];

    // Split the path and query string
    let (path, query_str) = match full_path.find('?') {
        Some(q) => (&full_path[..q], &full_path[q + 1..]),
        None    => (full_path, ""),
    };

    let query = parse_query_string(query_str);

    // --- Headers ---
    let mut headers: HashMap<String, String> = HashMap::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim();
        if trimmed.is_empty() { break; }
        if let Some(colon) = trimmed.find(':') {
            let name  = trimmed[..colon].trim().to_lowercase();
            let value = trimmed[colon + 1..].trim().to_string();
            headers.insert(name, value);
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
        headers,
        query,
        path_params: HashMap::new(), // filled by router
        body,
    })
}

fn parse_query_string(query: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() { continue; }
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("").to_string();
        let val = parts.next().unwrap_or("").to_string();
        if !key.is_empty() {
            map.insert(key, val);
        }
    }
    map
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
        assert_eq!(req.query.get("q").unwrap(), "rust");
        assert_eq!(req.query.get("page").unwrap(), "2");
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
        let req = parse("GET / HTTP/1.1\r\nAuthorization: Bearer token123\r\nX-Custom: value\r\n\r\n").unwrap();
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
        assert_eq!(req.query.get("flag").unwrap(), "");
    }
}
