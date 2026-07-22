//! HTTP/1.1 response writer — RFC 9112

use std::io::Write;
use std::net::TcpStream;

use kernway_core::response::Response;

/// Write HTTP/1.1 response ra TcpStream.
pub fn write_response(stream: &mut TcpStream, response: &Response) {
    write_to(stream, response);
}

/// Write an HTTP/1.1 response to any `Write` — useful for testing.
pub fn write_to<W: Write>(writer: &mut W, response: &Response) {
    let bytes = encode_response(response);
    let _ = writer.write_all(&bytes);
    let _ = writer.flush();
}

/// Serialize a response to bytes.
///
/// The entry point for async transports: the caller owns the socket and does
/// the writing, so `kernway-http` needs no runtime dependency. Head and body go
/// into one buffer so a small response leaves as a single `write`.
pub fn encode_response(response: &Response) -> Vec<u8> {
    let status_text = status_text(response.status.0);
    let mut head = format!("HTTP/1.1 {} {}\r\n", response.status.0, status_text);

    for (name, value) in &response.headers {
        head.push_str(&format!("{}: {}\r\n", name, value));
    }
    // Content-Length is required (RFC 9112 §6.2)
    head.push_str(&format!("content-length: {}\r\n", response.body.len()));
    // Connection: close for v0.3 (keep-alive in v0.4)
    head.push_str("connection: close\r\n");
    head.push_str("\r\n");

    let mut out = Vec::with_capacity(head.len() + response.body.len());
    out.extend_from_slice(head.as_bytes());
    out.extend_from_slice(&response.body);
    out
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        422 => "Unprocessable Entity",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _   => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernway_core::{error::StatusCode, response::Response};

    fn capture(response: &Response) -> String {
        let mut buf: Vec<u8> = Vec::new();
        write_to(&mut buf, response);
        String::from_utf8(buf).unwrap()
    }

    #[test]
    fn write_200_status_line() {
        let resp = Response::new(StatusCode::OK);
        let out = capture(&resp);
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"));
    }

    #[test]
    fn write_404_status_line() {
        let resp = Response::new(StatusCode::NOT_FOUND);
        let out = capture(&resp);
        assert!(out.starts_with("HTTP/1.1 404 Not Found\r\n"));
    }

    #[test]
    fn write_500_status_line() {
        let resp = Response::new(StatusCode::INTERNAL_SERVER_ERROR);
        let out = capture(&resp);
        assert!(out.starts_with("HTTP/1.1 500 Internal Server Error\r\n"));
    }

    #[test]
    fn write_includes_content_length() {
        let resp = Response::new(StatusCode::OK).body(b"hello".to_vec());
        let out = capture(&resp);
        assert!(out.contains("content-length: 5\r\n"));
    }

    #[test]
    fn write_includes_body() {
        let body = b"hello world";
        let resp = Response::new(StatusCode::OK).body(body.to_vec());
        let out = capture(&resp);
        assert!(out.ends_with("hello world"));
    }

    #[test]
    fn write_includes_connection_close() {
        let resp = Response::new(StatusCode::OK);
        let out = capture(&resp);
        assert!(out.contains("connection: close\r\n"));
    }

    #[test]
    fn write_custom_content_type() {
        let resp = Response::new(StatusCode::OK)
            .content_type("application/json; charset=utf-8");
        let out = capture(&resp);
        assert!(out.contains("content-type: application/json; charset=utf-8\r\n"));
    }

    #[test]
    fn status_text_unknown_code() {
        assert_eq!(status_text(999), "Unknown");
    }
}
