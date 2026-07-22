//! kernway-http — HTTP/1.1 parser + writer (RFC 9112)
//! Pure std — no external dependencies.

#![forbid(unsafe_code)]

pub mod parser;
pub mod writer;
pub use parser::{parse_bytes, parse_request, Parsed};
pub use writer::{encode_response, encode_response_with, write_response, Connection};
