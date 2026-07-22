//! kernway-http — HTTP/1.1 parser + writer (RFC 9112)
//! Pure std — no external dependencies.

#![forbid(unsafe_code)]

pub mod parser;
pub mod writer;
pub use parser::parse_request;
pub use writer::write_response;
