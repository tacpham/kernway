# http-proto — HTTP/1.1 Parser

## Purpose

Parse HTTP/1.1 requests and serialize responses. Engine: `httparse` crate.

## Standards

- **RFC 9110** — HTTP Semantics (methods, status codes, headers)
- **RFC 9112** — HTTP/1.1 Message Syntax (request line, CRLF, chunked)
- **RFC 6265** — Cookies
- **RFC 9111** — HTTP Caching (ETag, Cache-Control)

## HTTP/1.1 Parsing Pipeline

```
Raw bytes (TCP)
│
├── httparse::parse_request()      RFC 9112 §3-4
│   ├── Request line: METHOD SP URI SP HTTP/1.1 CRLF
│   ├── Headers: Name ":" OWS Value OWS CRLF
│   └── CRLFCRLF → end of headers
│
├── Body handling                  RFC 9112 §6
│   ├── Content-Length: read exact N bytes
│   ├── Transfer-Encoding: chunked → decode chunks
│   └── No body: zero-length
│
└── kernway_core::Request { method, uri, headers, body }
```

## Response Serialization

```rust
pub fn write_response(res: &Response, buf: &mut Vec<u8>) {
    // RFC 9112 §4: Status line
    write!(buf, "HTTP/1.1 {} {}\r\n", res.status.as_u16(), res.status.canonical_reason());

    // RFC 9110 §6.6.1: Date header (required for origin servers)
    write!(buf, "Date: {}\r\n", http_date_now());

    for (name, value) in &res.headers {
        write!(buf, "{}: {}\r\n", name, value);
    }
    buf.extend_from_slice(b"\r\n");

    // Body
    match &res.body {
        Body::Bytes(b) => buf.extend_from_slice(b),
        Body::Empty => {}
        Body::Stream(_) => { /* chunked transfer */ }
    }
}
```

## Security

- **Request smuggling** (RFC 9112 §6.3): reject requests that contain both Content-Length and Transfer-Encoding
- **Header injection**: reject all CR/LF characters in header values
- **Request size limit**: default 10MB body, 8KB header section — configurable
- **Slowloris mitigation**: read timeout per connection (default 30s)

## Pipelining

RFC 9112 §9: HTTP/1.1 pipelining is supported, but responses must be sent in the exact order of the requests.
