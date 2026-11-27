//! RESP2 and RESP3 — the Redis wire protocol, as pure encode/parse over byte
//! slices.
//!
//! Kept free of I/O so it can be unit-tested against fixed buffers: [`encode`]
//! writes a command as an array of bulk strings, and [`parse`] turns bytes back
//! into a [`Value`], reporting `Ok(None)` when the buffer holds only part of a
//! reply so the caller reads more and retries.
//!
//! An error reply is a value ([`Value::Error`]): one nested in an array (a
//! failed command inside `EXEC`) must not stop the rest of the array from
//! being read, and a top-level one must still be consumed whole so the
//! connection stays in step. The connection turns a top-level error into
//! [`RedisError::Server`](crate::RedisError::Server).

use crate::error::RedisError;

/// Nesting limit for aggregates — protects the parser's stack from a hostile
/// or broken peer.
const MAX_DEPTH: usize = 64;

/// A parsed reply.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// RESP2 null bulk / array (`$-1`, `*-1`) and RESP3 null (`_`).
    Nil,
    /// A simple string (`+OK`).
    Simple(String),
    /// An error reply (`-ERR …` or RESP3 blob error `!`), without the `-`.
    Error(String),
    /// An integer (`:42`).
    Int(i64),
    /// A bulk string (`$3\r\nfoo`) — arbitrary bytes.
    Bulk(Vec<u8>),
    /// An array (`*2\r\n…`) — elements in order.
    Array(Vec<Value>),
    /// RESP3 double (`,3.14`, also `inf`, `-inf`, `nan`).
    Double(f64),
    /// RESP3 boolean (`#t` / `#f`).
    Bool(bool),
    /// RESP3 big number (`(` …), as its decimal text.
    BigNumber(String),
    /// RESP3 verbatim string (`=`): its 3-letter format (`txt`, `mkd`) and text.
    Verbatim(String, String),
    /// RESP3 map (`%`): key/value pairs in order.
    Map(Vec<(Value, Value)>),
    /// RESP3 set (`~`).
    Set(Vec<Value>),
    /// RESP3 push (`>`): out-of-band data — pub/sub messages, client-side
    /// caching invalidations.
    Push(Vec<Value>),
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

    /// Text of a bulk / simple / verbatim string (UTF-8, lossy).
    #[must_use]
    pub fn as_text(&self) -> Option<String> {
        match self {
            Value::Bulk(b) => Some(String::from_utf8_lossy(b).into_owned()),
            Value::Simple(s) | Value::Verbatim(_, s) | Value::BigNumber(s) => Some(s.clone()),
            _ => None,
        }
    }

    /// The elements of an array, set or push (`None` for anything else).
    #[must_use]
    pub fn as_items(&self) -> Option<&[Value]> {
        match self {
            Value::Array(v) | Value::Set(v) | Value::Push(v) => Some(v),
            _ => None,
        }
    }

    /// Key/value pairs of a RESP3 map, or of a RESP2 flat array of alternating
    /// keys and values (how `HGETALL`, `CONFIG GET`, `HELLO` answer in RESP2).
    #[must_use]
    pub fn into_pairs(self) -> Option<Vec<(Value, Value)>> {
        match self {
            Value::Map(m) => Some(m),
            Value::Array(v) if v.len() % 2 == 0 => {
                let mut it = v.into_iter();
                let mut out = Vec::new();
                while let (Some(k), Some(v)) = (it.next(), it.next()) {
                    out.push((k, v));
                }
                Some(out)
            }
            _ => None,
        }
    }
}

/// Encode a command as a RESP array of bulk strings, appended to `out`.
///
/// `["SET", "k", "v"]` → `*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n`.
pub fn encode(args: &[&[u8]], out: &mut Vec<u8>) {
    out.push(b'*');
    out.extend_from_slice(args.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    for arg in args {
        out.push(b'$');
        out.extend_from_slice(arg.len().to_string().as_bytes());
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
}

/// Parse one reply from the front of `b`.
///
/// - `Ok(Some((value, consumed)))` — a whole reply; `consumed` bytes were used.
///   An error reply is [`Value::Error`].
/// - `Ok(None)` — the buffer holds only part of a reply; read more and retry.
/// - `Err(RedisError::Protocol)` — the bytes are not valid RESP; the
///   connection cannot be trusted any more.
pub fn parse(b: &[u8]) -> Result<Option<(Value, usize)>, RedisError> {
    parse_at(b, 0)
}

fn parse_at(b: &[u8], depth: usize) -> Result<Option<(Value, usize)>, RedisError> {
    if depth > MAX_DEPTH {
        return Err(RedisError::Protocol("reply nested too deeply".into()));
    }
    let Some(crlf) = find_crlf(b) else {
        return Ok(None);
    };
    let line = &b[1..crlf];
    let after = crlf + 2;
    let simple = |v: Value| Ok(Some((v, after)));
    match b[0] {
        b'+' => simple(Value::Simple(as_str(line)?)),
        b'-' => simple(Value::Error(as_str(line)?)),
        b':' => simple(Value::Int(as_int(line)?)),
        b'_' => simple(Value::Nil),
        b'#' => match line {
            b"t" => simple(Value::Bool(true)),
            b"f" => simple(Value::Bool(false)),
            _ => Err(RedisError::Protocol("bad boolean".into())),
        },
        b',' => simple(Value::Double(as_double(line)?)),
        b'(' => {
            let s = as_str(line)?;
            if s.is_empty() || !s.trim_start_matches('-').bytes().all(|c| c.is_ascii_digit()) {
                return Err(RedisError::Protocol(format!("bad big number {s:?}")));
            }
            simple(Value::BigNumber(s))
        }
        b'$' | b'!' | b'=' => {
            let len = as_int(line)?;
            if len < 0 {
                return if b[0] == b'$' { simple(Value::Nil) } else { Err(RedisError::Protocol("negative length".into())) };
            }
            let Some(end) = after.checked_add(len as usize) else {
                return Err(RedisError::Protocol("length overflow".into()));
            };
            if b.len() < end + 2 {
                return Ok(None);
            }
            if &b[end..end + 2] != b"\r\n" {
                return Err(RedisError::Protocol("bulk payload not followed by CRLF".into()));
            }
            let payload = &b[after..end];
            let value = match b[0] {
                b'$' => Value::Bulk(payload.to_vec()),
                b'!' => Value::Error(String::from_utf8_lossy(payload).into_owned()),
                _ => {
                    // `fmt:text`
                    if payload.len() < 4 || payload[3] != b':' {
                        return Err(RedisError::Protocol("bad verbatim string".into()));
                    }
                    Value::Verbatim(
                        String::from_utf8_lossy(&payload[..3]).into_owned(),
                        String::from_utf8_lossy(&payload[4..]).into_owned(),
                    )
                }
            };
            Ok(Some((value, end + 2)))
        }
        b'*' | b'~' | b'>' | b'%' | b'|' => {
            let count = as_int(line)?;
            if count < 0 {
                return if b[0] == b'*' { simple(Value::Nil) } else { Err(RedisError::Protocol("negative count".into())) };
            }
            let pairs = matches!(b[0], b'%' | b'|');
            let n = (count as usize).checked_mul(if pairs { 2 } else { 1 }).ok_or_else(|| RedisError::Protocol("count overflow".into()))?;
            // Never trust the count for an allocation: each element is at
            // least 3 bytes, so more than that cannot be in the buffer.
            let mut items = Vec::with_capacity(n.min(b.len() / 3 + 1));
            let mut off = after;
            for _ in 0..n {
                match parse_at(&b[off..], depth + 1)? {
                    Some((v, used)) => {
                        items.push(v);
                        off += used;
                    }
                    None => return Ok(None),
                }
            }
            let value = match b[0] {
                b'*' => Value::Array(items),
                b'~' => Value::Set(items),
                b'>' => Value::Push(items),
                b'%' => Value::Map(pair_up(items)),
                _ => {
                    // An attribute (`|`) annotates the reply that follows it:
                    // skip it and return that reply.
                    return Ok(parse_at(&b[off..], depth + 1)?.map(|(v, used)| (v, off + used)));
                }
            };
            Ok(Some((value, off)))
        }
        other => Err(RedisError::Protocol(format!("unknown reply type byte {other:#x}"))),
    }
}

fn pair_up(items: Vec<Value>) -> Vec<(Value, Value)> {
    let mut it = items.into_iter();
    let mut out = Vec::new();
    while let (Some(k), Some(v)) = (it.next(), it.next()) {
        out.push((k, v));
    }
    out
}

/// Index of the `\r` in the first `\r\n`, or `None` if there is no complete line.
fn find_crlf(b: &[u8]) -> Option<usize> {
    b.windows(2).position(|w| w == b"\r\n")
}

fn as_str(b: &[u8]) -> Result<String, RedisError> {
    std::str::from_utf8(b)
        .map(str::to_string)
        .map_err(|_| RedisError::Protocol("non-UTF-8 line".into()))
}

fn as_int(b: &[u8]) -> Result<i64, RedisError> {
    std::str::from_utf8(b)
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| RedisError::Protocol(format!("bad integer {:?}", String::from_utf8_lossy(b))))
}

fn as_double(b: &[u8]) -> Result<f64, RedisError> {
    let s = std::str::from_utf8(b).map_err(|_| RedisError::Protocol("bad double".into()))?;
    match s {
        "inf" => Ok(f64::INFINITY),
        "-inf" => Ok(f64::NEG_INFINITY),
        "nan" | "-nan" => Ok(f64::NAN),
        _ => s.parse().map_err(|_| RedisError::Protocol(format!("bad double {s:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(b: &[u8]) -> Value {
        let (v, used) = parse(b).unwrap().unwrap();
        assert_eq!(used, b.len(), "consumed the whole reply");
        v
    }

    #[test]
    fn encodes_a_command_as_bulk_strings() {
        let mut out = Vec::new();
        encode(&[b"SET", b"k", b"v\r\n"], &mut out);
        assert_eq!(out, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$3\r\nv\r\n\r\n");
    }

    #[test]
    fn resp2_types() {
        assert_eq!(one(b"+OK\r\n"), Value::Simple("OK".into()));
        assert_eq!(one(b":-42\r\n"), Value::Int(-42));
        assert_eq!(one(b"$5\r\nh\r\nlo\r\n"), Value::Bulk(b"h\r\nlo".to_vec()), "binary-safe");
        assert_eq!(one(b"$0\r\n\r\n"), Value::Bulk(vec![]));
        assert_eq!(one(b"$-1\r\n"), Value::Nil);
        assert_eq!(one(b"*-1\r\n"), Value::Nil);
        assert_eq!(one(b"*0\r\n"), Value::Array(vec![]));
        assert_eq!(one(b"*2\r\n:1\r\n$1\r\na\r\n"), Value::Array(vec![Value::Int(1), Value::Bulk(b"a".to_vec())]));
    }

    #[test]
    fn errors_are_values_consumed_whole_even_inside_arrays() {
        assert_eq!(one(b"-ERR wrong\r\n"), Value::Error("ERR wrong".into()));
        // EXEC with one failed command in the middle: the rest still parses.
        let exec = b"*3\r\n+OK\r\n-WRONGTYPE Operation against a key\r\n:7\r\n";
        assert_eq!(
            one(exec),
            Value::Array(vec![Value::Simple("OK".into()), Value::Error("WRONGTYPE Operation against a key".into()), Value::Int(7)])
        );
        assert_eq!(one(b"!21\r\nSYNTAX invalid syntax\r\n"), Value::Error("SYNTAX invalid syntax".into()));
    }

    #[test]
    fn resp3_types() {
        assert_eq!(one(b"_\r\n"), Value::Nil);
        assert_eq!(one(b"#t\r\n"), Value::Bool(true));
        assert_eq!(one(b"#f\r\n"), Value::Bool(false));
        assert_eq!(one(b",1.5\r\n"), Value::Double(1.5));
        assert_eq!(one(b",-inf\r\n"), Value::Double(f64::NEG_INFINITY));
        assert!(matches!(one(b",nan\r\n"), Value::Double(f) if f.is_nan()));
        assert_eq!(one(b"(3492890328409238509324850943850943825024385\r\n"), Value::BigNumber("3492890328409238509324850943850943825024385".into()));
        assert_eq!(one(b"=15\r\ntxt:Some string\r\n"), Value::Verbatim("txt".into(), "Some string".into()));
        assert_eq!(
            one(b"%2\r\n+first\r\n:1\r\n+second\r\n:2\r\n"),
            Value::Map(vec![(Value::Simple("first".into()), Value::Int(1)), (Value::Simple("second".into()), Value::Int(2))])
        );
        assert_eq!(one(b"~2\r\n+a\r\n+b\r\n"), Value::Set(vec![Value::Simple("a".into()), Value::Simple("b".into())]));
        assert_eq!(
            one(b">3\r\n$7\r\nmessage\r\n$2\r\nch\r\n$2\r\nhi\r\n"),
            Value::Push(vec![Value::Bulk(b"message".to_vec()), Value::Bulk(b"ch".to_vec()), Value::Bulk(b"hi".to_vec())])
        );
        // An attribute is skipped; the reply after it is returned.
        assert_eq!(one(b"|1\r\n+key-popularity\r\n%1\r\n$1\r\na\r\n,0.19\r\n:42\r\n"), Value::Int(42));
    }

    #[test]
    fn partial_input_asks_for_more_at_every_cut() {
        let full: &[u8] = b"*3\r\n$5\r\nhello\r\n%1\r\n+k\r\n,2.5\r\n-ERR x\r\n";
        for cut in 0..full.len() {
            assert_eq!(parse(&full[..cut]).unwrap(), None, "cut at {cut}");
        }
        assert!(parse(full).unwrap().is_some());
    }

    #[test]
    fn several_replies_in_one_buffer_parse_one_at_a_time() {
        let b = b"+OK\r\n:1\r\n";
        let (v, used) = parse(b).unwrap().unwrap();
        assert_eq!((v, used), (Value::Simple("OK".into()), 5));
        assert_eq!(parse(&b[used..]).unwrap().unwrap().0, Value::Int(1));
    }

    #[test]
    fn garbage_is_a_protocol_error_not_a_panic() {
        for bad in [&b"?x\r\n"[..], b":abc\r\n", b"$3\r\nabcXY", b"#x\r\n", b"(1.5\r\n", b"=3\r\nabc\r\n", b"~-1\r\n"] {
            assert!(matches!(parse(bad), Err(RedisError::Protocol(_))), "{:?}", String::from_utf8_lossy(bad));
        }
        // A huge declared count with no data does not allocate it up front.
        assert_eq!(parse(b"*4000000000\r\n").unwrap(), None);
        // Deep nesting is refused instead of overflowing the stack.
        let deep: Vec<u8> = b"*1\r\n".repeat(200).into_iter().chain(b":1\r\n".iter().copied()).collect();
        assert!(matches!(parse(&deep), Err(RedisError::Protocol(_))));
    }

    #[test]
    fn pairs_from_a_map_or_a_flat_array() {
        let flat = Value::Array(vec![Value::Bulk(b"a".to_vec()), Value::Int(1)]);
        assert_eq!(flat.into_pairs().unwrap(), vec![(Value::Bulk(b"a".to_vec()), Value::Int(1))]);
        assert!(Value::Array(vec![Value::Int(1)]).into_pairs().is_none());
    }
}
