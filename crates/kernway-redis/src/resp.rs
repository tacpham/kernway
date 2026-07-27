//! RESP2 — the Redis wire protocol, as pure encode/parse over byte slices.
//!
//! Kept free of I/O so it can be unit-tested against fixed buffers: [`encode`]
//! writes a command as an array of bulk strings, and [`parse`] turns bytes back
//! into a [`Value`], reporting `Ok(None)` when the buffer holds only part of a
//! reply so the caller reads more and retries.
//!
//! Only RESP2 (the `+ - : $ *` prefixes) — enough for every command the session
//! store issues. RESP3's extra types are a later concern.

use crate::error::RedisError;

/// A parsed reply. A server error (`-ERR …`) is *not* a value — it surfaces as
/// [`RedisError::Server`] from [`parse`], the way a caller wants to handle it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// A null bulk string or null array (`$-1` / `*-1`) — Redis's "no value".
    Nil,
    /// A simple string (`+OK`).
    Simple(String),
    /// An integer (`:42`).
    Int(i64),
    /// A bulk string (`$3\r\nfoo`) — arbitrary bytes.
    Bulk(Vec<u8>),
    /// An array (`*2\r\n…`) — elements in order.
    Array(Vec<Value>),
}

impl Value {
    /// The bytes of a bulk string, or `None` for any other shape (incl. `Nil`).
    #[must_use]
    pub fn as_bulk(&self) -> Option<&[u8]> {
        match self {
            Value::Bulk(b) => Some(b),
            _ => None,
        }
    }

    /// The integer of an `:` reply, or `None`.
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
            _ => None,
        }
    }
}

/// Encode a command as a RESP array of bulk strings, appended to `out`.
///
/// `["SET", "k", "v"]` → `*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n`.
pub fn encode(args: &[&[u8]], out: &mut Vec<u8>) {
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
}

/// Parse one reply from the front of `b`.
///
/// - `Ok(Some((value, consumed)))` — a whole reply; `consumed` bytes were used.
/// - `Ok(None)` — the buffer holds only part of a reply; read more and retry.
/// - `Err(RedisError::Server)` — the reply was a `-` error.
/// - `Err(RedisError::Protocol)` — the bytes are not valid RESP.
pub fn parse(b: &[u8]) -> Result<Option<(Value, usize)>, RedisError> {
    let Some(crlf) = find_crlf(b) else {
        return Ok(None);
    };
    let line = &b[1..crlf]; // content between the type byte and the CRLF
    let after = crlf + 2; // index just past the CRLF

    match b[0] {
        b'+' => Ok(Some((Value::Simple(as_str(line)?), after))),
        b'-' => Err(RedisError::Server(as_str(line)?)),
        b':' => Ok(Some((Value::Int(as_int(line)?), after))),
        b'$' => {
            let len = as_int(line)?;
            if len < 0 {
                return Ok(Some((Value::Nil, after)));
            }
            let len = len as usize;
            let end = after + len;
            // Need the payload plus its trailing CRLF.
            if b.len() < end + 2 {
                return Ok(None);
            }
            Ok(Some((Value::Bulk(b[after..end].to_vec()), end + 2)))
        }
        b'*' => {
            let count = as_int(line)?;
            if count < 0 {
                return Ok(Some((Value::Nil, after)));
            }
            let mut items = Vec::with_capacity(count as usize);
            let mut off = after;
            for _ in 0..count {
                match parse(&b[off..])? {
                    Some((value, used)) => {
                        items.push(value);
                        off += used;
                    }
                    None => return Ok(None),
                }
            }
            Ok(Some((Value::Array(items), off)))
        }
        other => Err(RedisError::Protocol(format!(
            "unknown reply type byte {other:#x}"
        ))),
    }
}

/// Index of the `\r` in the first `\r\n`, or `None` if there is no complete line.
fn find_crlf(b: &[u8]) -> Option<usize> {
    b.windows(2).position(|w| w == b"\r\n")
}

fn as_str(b: &[u8]) -> Result<String, RedisError> {
    String::from_utf8(b.to_vec()).map_err(|_| RedisError::Protocol("reply is not UTF-8".into()))
}

fn as_int(b: &[u8]) -> Result<i64, RedisError> {
    as_str(b)?
        .parse()
        .map_err(|_| RedisError::Protocol("reply is not an integer".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_a_command_as_bulk_string_array() {
        let mut out = Vec::new();
        encode(&[b"SET", b"k", b"v"], &mut out);
        assert_eq!(out, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
    }

    #[test]
    fn parses_each_reply_type() {
        assert_eq!(
            parse(b"+OK\r\n").unwrap(),
            Some((Value::Simple("OK".into()), 5))
        );
        assert_eq!(parse(b":42\r\n").unwrap(), Some((Value::Int(42), 5)));
        assert_eq!(
            parse(b"$3\r\nfoo\r\n").unwrap(),
            Some((Value::Bulk(b"foo".to_vec()), 9))
        );
        assert_eq!(parse(b"$-1\r\n").unwrap(), Some((Value::Nil, 5)));
    }

    #[test]
    fn parses_a_nested_array() {
        let (value, used) = parse(b"*2\r\n$3\r\nfoo\r\n:7\r\n").unwrap().unwrap();
        assert_eq!(
            value,
            Value::Array(vec![Value::Bulk(b"foo".to_vec()), Value::Int(7)])
        );
        assert_eq!(used, 17);
    }

    #[test]
    fn a_server_error_reply_is_an_err() {
        let err = parse(b"-WRONGTYPE nope\r\n").unwrap_err();
        assert!(matches!(err, RedisError::Server(s) if s == "WRONGTYPE nope"));
    }

    #[test]
    fn a_partial_reply_asks_for_more() {
        // Bulk header says 5 bytes, but only 3 are present.
        assert_eq!(parse(b"$5\r\nfoo").unwrap(), None);
        // An array header with a missing element.
        assert_eq!(parse(b"*2\r\n$3\r\nfoo\r\n").unwrap(), None);
        // Not even a full line yet.
        assert_eq!(parse(b"+OK").unwrap(), None);
    }

    #[test]
    fn consumed_length_lets_the_caller_advance() {
        // Two replies back to back; parse should consume exactly the first.
        let buf = b"+OK\r\n:1\r\n";
        let (first, used) = parse(buf).unwrap().unwrap();
        assert_eq!(first, Value::Simple("OK".into()));
        assert_eq!(&buf[used..], b":1\r\n");
    }
}
