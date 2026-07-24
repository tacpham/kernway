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

/// What to tell the client about the connection's fate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    /// The server closes after this response.
    Close,
    /// The connection stays open for another request.
    KeepAlive,
}

impl Connection {
    fn header_value(self) -> &'static str {
        match self {
            Connection::Close => "close",
            Connection::KeepAlive => "keep-alive",
        }
    }
}

/// Serialize a response to bytes, announcing `connection: close`.
///
/// The entry point for async transports: the caller owns the socket and does
/// the writing, so `kernway-http` needs no runtime dependency.
pub fn encode_response(response: &Response) -> Vec<u8> {
    encode_response_with(response, Connection::Close)
}

/// Serialize the head — status line and headers — with `Content-Length` passed
/// in rather than read from the body.
///
/// Passing the length in is what lets the head be written for a body that is not
/// in memory: a `HEAD` response (length of the file, empty body) or a streamed
/// `Body::File` (the head goes out, then the connection task streams the bytes).
pub fn encode_head(response: &Response, connection: Connection, content_length: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(head_estimate(response));
    encode_head_into(&mut out, response, connection, content_length);
    out
}

/// Append the head to an existing buffer.
///
/// Split out so [`encode_response_with`] can size one buffer for head *and* body
/// up front and write into it — head and body in a single allocation, no realloc
/// between them, which is the property the one-write coalescing depends on.
fn encode_head_into(out: &mut Vec<u8>, response: &Response, connection: Connection, content_length: u64) {
    out.extend_from_slice(b"HTTP/1.1 ");
    out.extend_from_slice(itoa(u64::from(response.status.0)).as_bytes());
    out.push(b' ');
    out.extend_from_slice(status_text(response.status.0).as_bytes());
    out.extend_from_slice(b"\r\n");

    for (name, value) in &response.headers {
        // The connection and content-length headers are the transport's call.
        if name.eq_ignore_ascii_case("connection") || name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    // Content-Length is required (RFC 9112 §6.2)
    out.extend_from_slice(b"content-length: ");
    out.extend_from_slice(itoa(content_length).as_bytes());
    out.extend_from_slice(b"\r\nconnection: ");
    out.extend_from_slice(connection.header_value().as_bytes());
    out.extend_from_slice(b"\r\n\r\n");
}

/// Serialize a response with an in-memory body, head and body in one buffer.
///
/// Head and body coalesce into a single allocation so a small response leaves in
/// one `write` — with keep-alive that matters, since a split head and body can
/// otherwise be delayed by Nagle waiting on the peer's ACK. For a `Body::File`,
/// use [`encode_head`] and stream the file separately instead.
pub fn encode_response_with(response: &Response, connection: Connection) -> Vec<u8> {
    let body_len = response.body.len() as usize;
    let mut out = Vec::with_capacity(head_estimate(response) + body_len);
    encode_head_into(&mut out, response, connection, response.body.len());
    out.extend_from_slice(response.body_bytes());
    out
}

/// Big enough for the head in one allocation, without walking the headers twice
/// to get it exactly right. Over-estimating a few bytes is cheaper than a
/// realloc; under-estimating only costs the growth `Vec` would have done anyway.
fn head_estimate(response: &Response) -> usize {
    const STATUS_AND_FIXED_HEADERS: usize = 64; // status line + content-length + connection
    let headers: usize = response
        .headers
        .iter()
        .map(|(name, value)| name.len() + value.len() + 4)
        .sum();
    STATUS_AND_FIXED_HEADERS + headers
}

/// Decimal digits of `n`, without the allocation `format!`/`to_string` makes.
///
/// Both call sites are small integers — a status code and a body length — so a
/// 20-byte stack buffer covers every `u64`.
struct Digits {
    buf: [u8; 20],
    start: usize,
}

impl Digits {
    fn as_bytes(&self) -> &[u8] {
        &self.buf[self.start..]
    }
}

fn itoa(mut n: u64) -> Digits {
    let mut d = Digits { buf: [0; 20], start: 20 };
    loop {
        d.start -= 1;
        d.buf[d.start] = b'0' + u8::try_from(n % 10).expect("a decimal digit fits in u8");
        n /= 10;
        if n == 0 {
            return d;
        }
    }
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        206 => "Partial Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
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

#[cfg(test)]
mod connection_tests {
    use super::*;
    use kernway_core::{error::StatusCode, response::Response};

    fn encoded(response: &Response, connection: Connection) -> String {
        String::from_utf8(encode_response_with(response, connection)).unwrap()
    }

    #[test]
    fn keep_alive_is_announced_when_asked() {
        let out = encoded(&Response::new(StatusCode::OK), Connection::KeepAlive);
        assert!(out.contains("connection: keep-alive\r\n"), "got {out:?}");
        assert!(!out.contains("connection: close"));
    }

    #[test]
    fn the_default_encoder_still_closes() {
        let out = String::from_utf8(encode_response(&Response::new(StatusCode::OK))).unwrap();
        assert!(out.contains("connection: close\r\n"));
    }

    #[test]
    fn a_handler_cannot_override_the_connection_header() {
        // Whether the socket survives is the transport's decision — a handler
        // claiming keep-alive on a connection the server is about to close
        // would leave the client waiting for a response that never comes.
        let mut response = Response::new(StatusCode::OK);
        response.headers.insert("connection".into(), "keep-alive".into());
        let out = encoded(&response, Connection::Close);
        assert!(out.contains("connection: close\r\n"), "got {out:?}");
        assert_eq!(out.matches("connection:").count(), 1);
    }

    #[test]
    fn a_handler_cannot_forge_content_length() {
        // A wrong length desynchronises a persistent connection: the next
        // response would be read as part of this body.
        let mut response = Response::new(StatusCode::OK).body(b"1234".to_vec());
        response.headers.insert("Content-Length".into(), "999".into());
        let out = encoded(&response, Connection::KeepAlive);
        assert!(out.contains("content-length: 4\r\n"), "got {out:?}");
        assert!(!out.contains("999"));
    }

    #[test]
    fn head_and_body_are_written_as_one_buffer() {
        let response = Response::new(StatusCode::OK).body(b"hello".to_vec());
        let bytes = encode_response_with(&response, Connection::KeepAlive);
        assert!(bytes.ends_with(b"\r\n\r\nhello"));
    }
}
