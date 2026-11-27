//! Building commands ([`Cmd`]) and turning replies into Rust types
//! ([`FromValue`]) — so any Redis command, wrapped by this crate or not, can
//! be sent and read typed:
//!
//! ```no_run
//! # async fn demo(c: &mut kernway_redis::Connection) -> Result<(), kernway_redis::RedisError> {
//! use kernway_redis::Cmd;
//! let n: i64 = c.query(Cmd::new("INCRBY").arg("hits").arg(5)).await?;
//! let pairs: Vec<(String, f64)> = c.query(Cmd::new("ZRANGE").arg("board").arg(0).arg(-1).arg("WITHSCORES")).await?;
//! # let _ = (n, pairs); Ok(()) }
//! ```

use crate::error::RedisError;
use crate::resp::Value;
use std::collections::HashMap;
use std::hash::Hash;

/// One command: its name and arguments, as bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub(crate) args: Vec<Vec<u8>>,
}

impl Cmd {
    /// A command named `name` (`"SET"`, `"CLIENT"` …).
    pub fn new(name: &str) -> Self {
        Self { args: vec![name.as_bytes().to_vec()] }
    }

    /// Append an argument.
    #[allow(clippy::should_implement_trait)]
    pub fn arg(mut self, a: impl ToArg) -> Self {
        self.args.push(a.to_arg());
        self
    }

    /// Append several arguments.
    pub fn args<A: ToArg>(mut self, items: impl IntoIterator<Item = A>) -> Self {
        self.args.extend(items.into_iter().map(|a| a.to_arg()));
        self
    }

    /// The command name and arguments.
    pub fn parts(&self) -> &[Vec<u8>] {
        &self.args
    }
}

/// A value usable as a command argument.
pub trait ToArg {
    /// The argument's bytes.
    fn to_arg(&self) -> Vec<u8>;
}

impl ToArg for &str {
    fn to_arg(&self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }
}
impl ToArg for String {
    fn to_arg(&self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }
}
impl ToArg for &String {
    fn to_arg(&self) -> Vec<u8> {
        self.as_bytes().to_vec()
    }
}
impl ToArg for &[u8] {
    fn to_arg(&self) -> Vec<u8> {
        self.to_vec()
    }
}
impl<const N: usize> ToArg for &[u8; N] {
    fn to_arg(&self) -> Vec<u8> {
        self.to_vec()
    }
}
impl ToArg for Vec<u8> {
    fn to_arg(&self) -> Vec<u8> {
        self.clone()
    }
}
impl ToArg for &Vec<u8> {
    fn to_arg(&self) -> Vec<u8> {
        (*self).clone()
    }
}
macro_rules! int_arg {
    ($($t:ty),*) => {$(
        impl ToArg for $t {
            fn to_arg(&self) -> Vec<u8> {
                self.to_string().into_bytes()
            }
        }
    )*};
}
int_arg!(i8, i16, i32, i64, u8, u16, u32, u64, usize, isize);
impl ToArg for f64 {
    /// Shortest round-trip form; infinities as Redis spells them.
    fn to_arg(&self) -> Vec<u8> {
        if self.is_infinite() {
            if *self > 0.0 { b"+inf".to_vec() } else { b"-inf".to_vec() }
        } else {
            self.to_string().into_bytes()
        }
    }
}

/// Convert a reply into a Rust value. Implemented for the usual shapes, and
/// tolerant of the RESP2 / RESP3 differences (a score is a bulk string in
/// RESP2 and a double in RESP3; `HGETALL` is a flat array or a map).
pub trait FromValue: Sized {
    /// Convert, or explain why the reply does not fit.
    fn from_value(v: Value) -> Result<Self, RedisError>;
}

fn mismatch<T>(want: &str, v: &Value) -> Result<T, RedisError> {
    Err(RedisError::Unexpected(format!("expected {want}, got {v:?}")))
}

impl FromValue for Value {
    fn from_value(v: Value) -> Result<Self, RedisError> {
        Ok(v)
    }
}

impl FromValue for () {
    fn from_value(_: Value) -> Result<Self, RedisError> {
        Ok(())
    }
}

impl FromValue for Vec<u8> {
    fn from_value(v: Value) -> Result<Self, RedisError> {
        match v {
            Value::Bulk(b) => Ok(b),
            Value::Simple(s) | Value::BigNumber(s) | Value::Verbatim(_, s) => Ok(s.into_bytes()),
            Value::Int(i) => Ok(i.to_string().into_bytes()),
            Value::Double(d) => Ok(d.to_string().into_bytes()),
            other => mismatch("bytes", &other),
        }
    }
}

impl FromValue for String {
    fn from_value(v: Value) -> Result<Self, RedisError> {
        let b = Vec::<u8>::from_value(v)?;
        String::from_utf8(b).map_err(|_| RedisError::Unexpected("reply is not UTF-8".into()))
    }
}

impl FromValue for i64 {
    fn from_value(v: Value) -> Result<Self, RedisError> {
        match v {
            Value::Int(i) => Ok(i),
            Value::Bool(b) => Ok(i64::from(b)),
            Value::Double(d) if d.fract() == 0.0 && d.is_finite() => Ok(d as i64),
            Value::Bulk(ref b) => std::str::from_utf8(b).ok().and_then(|s| s.parse().ok()).map_or_else(|| mismatch("integer", &v), Ok),
            Value::Simple(ref s) => s.parse().map_or_else(|_| mismatch("integer", &v), Ok),
            other => mismatch("integer", &other),
        }
    }
}

impl FromValue for u64 {
    fn from_value(v: Value) -> Result<Self, RedisError> {
        let i = i64::from_value(v)?;
        u64::try_from(i).map_err(|_| RedisError::Unexpected(format!("{i} is negative")))
    }
}

impl FromValue for usize {
    fn from_value(v: Value) -> Result<Self, RedisError> {
        Ok(u64::from_value(v)? as usize)
    }
}

impl FromValue for f64 {
    fn from_value(v: Value) -> Result<Self, RedisError> {
        match v {
            Value::Double(d) => Ok(d),
            Value::Int(i) => Ok(i as f64),
            Value::Bulk(ref b) => {
                let s = std::str::from_utf8(b).unwrap_or("");
                match s {
                    "inf" | "+inf" => Ok(f64::INFINITY),
                    "-inf" => Ok(f64::NEG_INFINITY),
                    _ => s.parse().map_or_else(|_| mismatch("number", &v), Ok),
                }
            }
            other => mismatch("number", &other),
        }
    }
}

impl FromValue for bool {
    fn from_value(v: Value) -> Result<Self, RedisError> {
        match v {
            Value::Bool(b) => Ok(b),
            Value::Int(i) => Ok(i != 0),
            Value::Simple(ref s) if s == "OK" => Ok(true),
            Value::Nil => Ok(false),
            other => mismatch("boolean", &other),
        }
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_value(v: Value) -> Result<Self, RedisError> {
        match v {
            Value::Nil => Ok(None),
            other => T::from_value(other).map(Some),
        }
    }
}

impl<T: FromValue> FromValue for Vec<T> {
    fn from_value(v: Value) -> Result<Self, RedisError> {
        match v {
            Value::Array(items) | Value::Set(items) | Value::Push(items) => items.into_iter().map(T::from_value).collect(),
            Value::Nil => Ok(Vec::new()),
            // A map read as a list: alternating keys and values, as RESP2 sends it.
            Value::Map(pairs) => pairs.into_iter().flat_map(|(k, v)| [k, v]).map(T::from_value).collect(),
            other => mismatch("list", &other),
        }
    }
}

impl<A: FromValue, B: FromValue> FromValue for (A, B) {
    fn from_value(v: Value) -> Result<Self, RedisError> {
        match v {
            Value::Array(items) if items.len() == 2 => {
                let mut it = items.into_iter();
                Ok((A::from_value(it.next().unwrap_or(Value::Nil))?, B::from_value(it.next().unwrap_or(Value::Nil))?))
            }
            other => mismatch("pair", &other),
        }
    }
}

/// Pairs from a RESP3 map or a RESP2 flat array (`HGETALL`, `WITHSCORES`),
/// or from an array of 2-element arrays (RESP3 `ZRANGE … WITHSCORES`).
pub fn pairs<K: FromValue, V: FromValue>(v: Value) -> Result<Vec<(K, V)>, RedisError> {
    match v {
        Value::Array(items) if items.iter().all(|i| matches!(i, Value::Array(p) if p.len() == 2)) && !items.is_empty() => {
            items.into_iter().map(<(K, V)>::from_value).collect()
        }
        Value::Nil => Ok(Vec::new()),
        other => {
            let shown = format!("{other:?}");
            other
                .into_pairs()
                .ok_or_else(|| RedisError::Unexpected(format!("expected pairs, got {shown}")))?
                .into_iter()
                .map(|(k, v)| Ok((K::from_value(k)?, V::from_value(v)?)))
                .collect()
        }
    }
}

impl<K: FromValue + Eq + Hash, V: FromValue> FromValue for HashMap<K, V> {
    fn from_value(v: Value) -> Result<Self, RedisError> {
        Ok(pairs::<K, V>(v)?.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments() {
        let c = Cmd::new("SET").arg("k").arg(b"v").arg(42).arg(1.5).arg(f64::INFINITY).arg(vec![0u8, 255]).args(["a", "b"]);
        let parts: Vec<&[u8]> = c.parts().iter().map(Vec::as_slice).collect();
        assert_eq!(parts, [&b"SET"[..], b"k", b"v", b"42", b"1.5", b"+inf", &[0, 255], b"a", b"b"]);
    }

    #[test]
    fn conversions_across_resp2_and_resp3() {
        assert_eq!(f64::from_value(Value::Bulk(b"1.5".to_vec())).unwrap(), 1.5);
        assert_eq!(f64::from_value(Value::Double(1.5)).unwrap(), 1.5);
        assert_eq!(f64::from_value(Value::Bulk(b"-inf".to_vec())).unwrap(), f64::NEG_INFINITY);
        assert_eq!(i64::from_value(Value::Bulk(b"7".to_vec())).unwrap(), 7);
        assert_eq!(Option::<String>::from_value(Value::Nil).unwrap(), None);
        assert!(bool::from_value(Value::Simple("OK".into())).unwrap());
        let flat = Value::Array(vec![Value::Bulk(b"a".to_vec()), Value::Bulk(b"1".to_vec())]);
        let map = Value::Map(vec![(Value::Bulk(b"a".to_vec()), Value::Bulk(b"1".to_vec()))]);
        let nested = Value::Array(vec![Value::Array(vec![Value::Bulk(b"a".to_vec()), Value::Double(1.0)])]);
        assert_eq!(pairs::<String, i64>(flat).unwrap(), [("a".to_string(), 1)]);
        assert_eq!(pairs::<String, i64>(map).unwrap(), [("a".to_string(), 1)]);
        assert_eq!(pairs::<String, f64>(nested).unwrap(), [("a".to_string(), 1.0)]);
        assert!(String::from_value(Value::Array(vec![])).is_err());
        assert!(u64::from_value(Value::Int(-1)).is_err());
    }
}
