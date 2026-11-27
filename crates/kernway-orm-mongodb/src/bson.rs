//! BSON (<https://bsonspec.org>): the value model, the binary codec, and
//! Extended JSON (relaxed and canonical) — the bridge serde types cross.
//!
//! A [`Document`] keeps its keys in order (MongoDB commands depend on the
//! first key). Decoding checks every length against the input and bounds
//! nesting, so hostile input is an error, never a panic or a huge allocation.

use crate::ordered_json::OJ;
use std::fmt;

/// Nesting deeper than this is refused (the server's own limit is 100).
const MAX_DEPTH: usize = 200;

/// A BSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Bson {
    /// 64-bit IEEE 754 float.
    Double(f64),
    /// UTF-8 string.
    String(String),
    /// Embedded document.
    Document(Document),
    /// Array.
    Array(Vec<Bson>),
    /// Binary data with a subtype.
    Binary(Binary),
    /// Deprecated `undefined`.
    Undefined,
    /// 12-byte ObjectId.
    ObjectId(ObjectId),
    /// Boolean.
    Boolean(bool),
    /// UTC datetime: milliseconds since the Unix epoch.
    DateTime(DateTime),
    /// Null.
    Null,
    /// Regular expression: pattern and options (sorted letters).
    RegularExpression(Regex),
    /// Deprecated DBPointer: namespace and id.
    DbPointer(String, ObjectId),
    /// JavaScript code.
    JavaScriptCode(String),
    /// Deprecated symbol.
    Symbol(String),
    /// JavaScript code with a scope document.
    JavaScriptCodeWithScope(String, Document),
    /// 32-bit integer.
    Int32(i32),
    /// Replication timestamp.
    Timestamp(Timestamp),
    /// 64-bit integer.
    Int64(i64),
    /// IEEE 754-2008 128-bit decimal.
    Decimal128(Decimal128),
    /// Sorts before every value.
    MinKey,
    /// Sorts after every value.
    MaxKey,
}

/// Binary data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binary {
    /// Subtype: 0 generic, 4 UUID, 5 MD5, 6 encrypted, 8 sensitive, 0x80+ user.
    pub subtype: u8,
    /// The bytes.
    pub bytes: Vec<u8>,
}

/// A regular expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Regex {
    /// The pattern.
    pub pattern: String,
    /// Options, e.g. `i`, `m`, `x`, `s`, `u` (kept sorted).
    pub options: String,
}

/// A replication timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp {
    /// Seconds since the epoch.
    pub time: u32,
    /// Ordinal within the second.
    pub increment: u32,
}

/// A UTC datetime in milliseconds since the Unix epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DateTime(pub i64);

impl DateTime {
    /// Now.
    pub fn now() -> Self {
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        DateTime(d.as_millis() as i64)
    }

    /// `YYYY-MM-DDTHH:MM:SS.mmmZ` (for years 1970..=9999; outside, `None`).
    pub fn to_rfc3339(self) -> Option<String> {
        let ms = self.0;
        let secs = ms.div_euclid(1000);
        let milli = ms.rem_euclid(1000);
        let days = secs.div_euclid(86_400);
        let sod = secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        if !(1970..=9999).contains(&y) {
            return None;
        }
        Some(format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{milli:03}Z", sod / 3600, sod % 3600 / 60, sod % 60))
    }

    /// Parse `YYYY-MM-DDTHH:MM:SS[.fff][Z|±HH:MM]`.
    pub fn parse_rfc3339(s: &str) -> Option<Self> {
        let b = s.as_bytes();
        let num = |r: std::ops::Range<usize>| -> Option<i64> { s.get(r)?.parse().ok() };
        if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b' ') || b[13] != b':' || b[16] != b':' {
            return None;
        }
        let (y, mo, d, h, mi, se) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?, num(14..16)?, num(17..19)?);
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 60 {
            return None;
        }
        let mut i = 19;
        let mut ms = 0i64;
        if b.get(i) == Some(&b'.') {
            i += 1;
            let start = i;
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            let frac = &s[start..i];
            if frac.is_empty() {
                return None;
            }
            let mut digits: String = frac.chars().take(3).collect();
            while digits.len() < 3 {
                digits.push('0');
            }
            ms = digits.parse().ok()?;
        }
        let offset_min = match b.get(i) {
            Some(b'Z') if i + 1 == b.len() => 0,
            Some(&c @ (b'+' | b'-')) if b.len() == i + 6 && b[i + 3] == b':' => {
                let v = num(i + 1..i + 3)? * 60 + num(i + 4..i + 6)?;
                if c == b'-' {
                    -v
                } else {
                    v
                }
            }
            None => 0,
            _ => return None,
        };
        let days = days_from_civil(y, mo as u32, d as u32);
        let secs = days * 86_400 + h * 3600 + mi * 60 + se - offset_min * 60;
        Some(DateTime(secs * 1000 + ms))
    }
}

// Howard Hinnant's civil-date algorithms.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(m) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A 12-byte ObjectId: 4-byte seconds, 5-byte per-process random, 3-byte counter.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; 12]);

impl ObjectId {
    /// A fresh id.
    pub fn new() -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::OnceLock;
        static PROCESS: OnceLock<[u8; 5]> = OnceLock::new();
        static COUNTER: OnceLock<AtomicU32> = OnceLock::new();
        let process = PROCESS.get_or_init(|| {
            let mut b = [0u8; 5];
            let _ = kernway_tls::fill_random(&mut b);
            b
        });
        let counter = COUNTER.get_or_init(|| {
            let mut b = [0u8; 4];
            let _ = kernway_tls::fill_random(&mut b);
            AtomicU32::new(u32::from_le_bytes(b))
        });
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32);
        let n = counter.fetch_add(1, Ordering::Relaxed);
        let mut id = [0u8; 12];
        id[..4].copy_from_slice(&secs.to_be_bytes());
        id[4..9].copy_from_slice(process);
        id[9..].copy_from_slice(&n.to_be_bytes()[1..]);
        ObjectId(id)
    }

    /// Parse 24 hex digits.
    pub fn parse_str(s: &str) -> Option<Self> {
        if s.len() != 24 {
            return None;
        }
        let mut id = [0u8; 12];
        for (i, b) in id.iter_mut().enumerate() {
            *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
        }
        Some(ObjectId(id))
    }

    /// 24 lower-case hex digits.
    pub fn to_hex(self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The creation time (seconds since the epoch).
    pub fn timestamp(self) -> u32 {
        u32::from_be_bytes([self.0[0], self.0[1], self.0[2], self.0[3]])
    }
}

impl Default for ObjectId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", self.to_hex())
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// An IEEE 754-2008 decimal128 value (binary integer decimal encoding),
/// kept as its 16 little-endian bytes; converts to and from its string form.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Decimal128(pub [u8; 16]);

impl fmt::Debug for Decimal128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Decimal128({self})")
    }
}

const D128_EXP_BIAS: i32 = 6176;
const D128_EXP_MAX: i32 = 6111;
const D128_EXP_MIN: i32 = -6176;

impl Decimal128 {
    fn bits(self) -> u128 {
        u128::from_le_bytes(self.0)
    }

    /// Parse a decimal string (`-1.50`, `1E+3`, `NaN`, `-Infinity`, `Inf`).
    /// Values that do not fit 34 digits are rounded only when the dropped
    /// digits are zeros (as the BSON spec requires); otherwise `None`.
    pub fn parse(s: &str) -> Option<Self> {
        let (neg, body) = match s.as_bytes().first() {
            Some(b'-') => (true, &s[1..]),
            Some(b'+') => (false, &s[1..]),
            _ => (false, s),
        };
        let sign: u128 = if neg { 1 << 127 } else { 0 };
        let lower = body.to_ascii_lowercase();
        if lower == "nan" {
            return Some(Decimal128::from_bits(0x7c00u128 << 112));
        }
        if lower == "inf" || lower == "infinity" {
            return Some(Decimal128::from_bits(sign | 0x7800u128 << 112));
        }
        let (mant, exp_part) = match lower.find('e') {
            Some(p) => (&lower[..p], Some(&lower[p + 1..])),
            None => (lower.as_str(), None),
        };
        let mut exp: i64 = match exp_part {
            Some(e) => {
                if e.is_empty() || e == "+" || e == "-" {
                    return None;
                }
                e.parse().ok()?
            }
            None => 0,
        };
        let (int, frac) = match mant.split_once('.') {
            Some((i, f)) => (i, f),
            None => (mant, ""),
        };
        if (int.is_empty() && frac.is_empty()) || !int.bytes().chain(frac.bytes()).all(|c| c.is_ascii_digit()) {
            return None;
        }
        exp -= frac.len() as i64;
        let mut digits: String = format!("{int}{frac}").trim_start_matches('0').to_string();
        if digits.is_empty() {
            digits = "0".into();
        }
        // Too many digits: drop trailing zeros into the exponent.
        while digits.len() > 34 {
            if !digits.ends_with('0') {
                return None;
            }
            digits.pop();
            exp += 1;
        }
        // Exponent too large: pad the coefficient (clamping).
        while exp > i64::from(D128_EXP_MAX) {
            if digits == "0" {
                exp = i64::from(D128_EXP_MAX);
                break;
            }
            if digits.len() >= 34 {
                return None;
            }
            digits.push('0');
            exp -= 1;
        }
        // Exponent too small: drop trailing zeros (exact rounding only).
        while exp < i64::from(D128_EXP_MIN) {
            if digits == "0" {
                exp = i64::from(D128_EXP_MIN);
                break;
            }
            if !digits.ends_with('0') {
                return None;
            }
            digits.pop();
            if digits.is_empty() {
                digits = "0".into();
            }
            exp += 1;
        }
        let coeff: u128 = digits.parse().ok()?;
        let biased = (exp as i32 + D128_EXP_BIAS) as u128;
        let bits = if coeff >> 113 == 0 {
            sign | (biased << 113) | coeff
        } else {
            // Coefficients at or above 2^113 never fit 34 digits' canonical form.
            return None;
        };
        Some(Decimal128::from_bits(bits))
    }

    /// From the 128-bit pattern.
    pub fn from_bits(b: u128) -> Self {
        Decimal128(b.to_le_bytes())
    }
}

impl fmt::Display for Decimal128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.bits();
        let neg = b >> 127 == 1;
        let comb = (b >> 122) & 0x1f;
        let sign = if neg { "-" } else { "" };
        if comb == 0x1f {
            return f.write_str("NaN");
        }
        if comb == 0x1e {
            return write!(f, "{sign}Infinity");
        }
        let (biased, coeff) = if (b >> 125) & 0b11 == 0b11 {
            // 11 combination: the coefficient would exceed 34 digits — non-canonical, zero.
            (((b >> 111) & 0x3fff) as i32, 0u128)
        } else {
            (((b >> 113) & 0x3fff) as i32, b & ((1u128 << 113) - 1))
        };
        let coeff = if coeff > 9_999_999_999_999_999_999_999_999_999_999_999 { 0 } else { coeff };
        let exp = biased - D128_EXP_BIAS;
        let digits = coeff.to_string();
        let n = digits.len() as i32;
        let adjusted = exp + n - 1;
        if exp > 0 || adjusted < -6 {
            // Scientific notation.
            let mut s = String::from(sign);
            s.push_str(&digits[..1]);
            if n > 1 {
                s.push('.');
                s.push_str(&digits[1..]);
            }
            s.push('E');
            if adjusted >= 0 {
                s.push('+');
            }
            s.push_str(&adjusted.to_string());
            f.write_str(&s)
        } else if exp == 0 {
            write!(f, "{sign}{digits}")
        } else {
            let point = n + exp; // digits before the point
            if point > 0 {
                write!(f, "{sign}{}.{}", &digits[..point as usize], &digits[point as usize..])
            } else {
                write!(f, "{sign}0.{}{digits}", "0".repeat((-point) as usize))
            }
        }
    }
}

/// An ordered BSON document.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Document {
    entries: Vec<(String, Bson)>,
}

impl Document {
    /// An empty document.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `key` (replacing its value in place, or appending).
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Bson>) -> &mut Self {
        let key = key.into();
        let value = value.into();
        match self.entries.iter_mut().find(|(k, _)| *k == key) {
            Some(e) => e.1 = value,
            None => self.entries.push((key, value)),
        }
        self
    }

    /// Builder form of [`insert`](Self::insert).
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Bson>) -> Self {
        self.insert(key, value);
        self
    }

    /// The value at `key`.
    pub fn get(&self, key: &str) -> Option<&Bson> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Mutable value at `key`.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Bson> {
        self.entries.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Remove `key`, returning its value.
    pub fn remove(&mut self, key: &str) -> Option<Bson> {
        let i = self.entries.iter().position(|(k, _)| k == key)?;
        Some(self.entries.remove(i).1)
    }

    /// Whether `key` is present.
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Entries in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Bson)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Keys in order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|(k, _)| k.as_str())
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the document is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The first key (a command's name).
    pub fn first_key(&self) -> Option<&str> {
        self.entries.first().map(|(k, _)| k.as_str())
    }

    /// `key` as a string.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        match self.get(key)? {
            Bson::String(s) => Some(s),
            _ => None,
        }
    }

    /// `key` as an integer (Int32 / Int64 / integral Double).
    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.get(key)?.as_i64()
    }

    /// `key` as a boolean (also numbers ≠ 0).
    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            Bson::Boolean(b) => Some(*b),
            other => other.as_i64().map(|i| i != 0),
        }
    }

    /// `key` as a sub-document.
    pub fn get_document(&self, key: &str) -> Option<&Document> {
        match self.get(key)? {
            Bson::Document(d) => Some(d),
            _ => None,
        }
    }

    /// `key` as an array.
    pub fn get_array(&self, key: &str) -> Option<&Vec<Bson>> {
        match self.get(key)? {
            Bson::Array(a) => Some(a),
            _ => None,
        }
    }

    /// Encode to BSON bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        encode_document(self, &mut out);
        out
    }

    /// Append the BSON encoding to `out`.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        encode_document(self, out);
    }

    /// Decode one document that spans `bytes` exactly.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, BsonError> {
        let (doc, used) = decode_document(bytes, 0)?;
        if used != bytes.len() {
            return Err(BsonError(format!("{} trailing bytes after the document", bytes.len() - used)));
        }
        Ok(doc)
    }

    /// Decode the document at the start of `bytes`; returns it and its length.
    pub fn from_prefix(bytes: &[u8]) -> Result<(Self, usize), BsonError> {
        decode_document(bytes, 0)
    }

    /// Relaxed Extended JSON.
    pub fn to_relaxed_json(&self) -> serde_json::Value {
        Bson::Document(self.clone()).to_relaxed_json()
    }

    /// Relaxed Extended JSON text, keys in the document's order.
    pub fn to_relaxed_json_string(&self) -> String {
        Bson::Document(self.clone()).to_relaxed_json_string()
    }
}

impl IntoIterator for Document {
    type Item = (String, Bson);
    type IntoIter = std::vec::IntoIter<(String, Bson)>;
    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl FromIterator<(String, Bson)> for Document {
    fn from_iter<I: IntoIterator<Item = (String, Bson)>>(iter: I) -> Self {
        let mut d = Document::new();
        for (k, v) in iter {
            d.insert(k, v);
        }
        d
    }
}

/// Build a [`Document`]: `doc! { "find": "items", "limit": 5 }`.
#[macro_export]
macro_rules! doc {
    () => { $crate::bson::Document::new() };
    ($($k:expr => $v:expr),+ $(,)?) => {{
        let mut d = $crate::bson::Document::new();
        $( d.insert($k, $v); )+
        d
    }};
}

pub use crate::doc;

/// A malformed BSON input, or a value that cannot be represented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BsonError(pub String);

impl fmt::Display for BsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BSON: {}", self.0)
    }
}

impl std::error::Error for BsonError {}

macro_rules! from_impl {
    ($($t:ty => $v:ident),*) => { $( impl From<$t> for Bson { fn from(x: $t) -> Self { Bson::$v(x.into()) } } )* };
}
from_impl!(f64 => Double, String => String, &str => String, Document => Document, bool => Boolean, i32 => Int32, i64 => Int64,
           ObjectId => ObjectId, DateTime => DateTime, Binary => Binary, Decimal128 => Decimal128, Timestamp => Timestamp, Regex => RegularExpression);

impl From<u32> for Bson {
    fn from(x: u32) -> Self {
        Bson::Int64(i64::from(x))
    }
}

impl From<usize> for Bson {
    fn from(x: usize) -> Self {
        Bson::Int64(x as i64)
    }
}

impl<T: Into<Bson>> From<Vec<T>> for Bson {
    fn from(v: Vec<T>) -> Self {
        Bson::Array(v.into_iter().map(Into::into).collect())
    }
}

impl<T: Into<Bson>> From<Option<T>> for Bson {
    fn from(v: Option<T>) -> Self {
        v.map_or(Bson::Null, Into::into)
    }
}

impl Bson {
    /// The BSON type byte.
    pub fn element_type(&self) -> u8 {
        match self {
            Bson::Double(_) => 0x01,
            Bson::String(_) => 0x02,
            Bson::Document(_) => 0x03,
            Bson::Array(_) => 0x04,
            Bson::Binary(_) => 0x05,
            Bson::Undefined => 0x06,
            Bson::ObjectId(_) => 0x07,
            Bson::Boolean(_) => 0x08,
            Bson::DateTime(_) => 0x09,
            Bson::Null => 0x0A,
            Bson::RegularExpression(_) => 0x0B,
            Bson::DbPointer(..) => 0x0C,
            Bson::JavaScriptCode(_) => 0x0D,
            Bson::Symbol(_) => 0x0E,
            Bson::JavaScriptCodeWithScope(..) => 0x0F,
            Bson::Int32(_) => 0x10,
            Bson::Timestamp(_) => 0x11,
            Bson::Int64(_) => 0x12,
            Bson::Decimal128(_) => 0x13,
            Bson::MinKey => 0xFF,
            Bson::MaxKey => 0x7F,
        }
    }

    /// As an integer: Int32, Int64, or a Double with no fraction.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Bson::Int32(i) => Some(i64::from(*i)),
            Bson::Int64(i) => Some(*i),
            Bson::Double(f) if f.fract() == 0.0 && f.abs() < 9.2e18 => Some(*f as i64),
            _ => None,
        }
    }

    /// As a float (any number).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Bson::Double(f) => Some(*f),
            Bson::Int32(i) => Some(f64::from(*i)),
            Bson::Int64(i) => Some(*i as f64),
            _ => None,
        }
    }

    /// As a string slice.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Bson::String(s) | Bson::Symbol(s) => Some(s),
            _ => None,
        }
    }

    /// As a document.
    pub fn as_document(&self) -> Option<&Document> {
        match self {
            Bson::Document(d) => Some(d),
            _ => None,
        }
    }

    /// As an array.
    pub fn as_array(&self) -> Option<&Vec<Bson>> {
        match self {
            Bson::Array(a) => Some(a),
            _ => None,
        }
    }

    /// As a boolean.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Bson::Boolean(b) => Some(*b),
            _ => None,
        }
    }

    /// Relaxed Extended JSON: numbers as JSON numbers where exact, special
    /// types in their `$`-wrapper form (`{"$oid": …}`, `{"$date": …}`, …).
    pub fn to_relaxed_json(&self) -> serde_json::Value {
        self.to_json(false)
    }

    /// Canonical Extended JSON: every number typed (`{"$numberInt": "1"}`).
    pub fn to_canonical_json(&self) -> serde_json::Value {
        self.to_json(true)
    }

    fn to_json(&self, canonical: bool) -> serde_json::Value {
        use serde_json::{json, Value as J};
        match self {
            Bson::Double(f) => {
                if !canonical && f.is_finite() {
                    // Integral doubles keep a fraction marker in relaxed form
                    // only where JSON numbers could not tell them apart.
                    serde_json::Number::from_f64(*f).map_or_else(|| json!({"$numberDouble": double_text(*f)}), J::Number)
                } else {
                    json!({"$numberDouble": double_text(*f)})
                }
            }
            Bson::String(s) => J::String(s.clone()),
            Bson::Document(d) => J::Object(d.iter().map(|(k, v)| (k.to_string(), v.to_json(canonical))).collect()),
            Bson::Array(a) => J::Array(a.iter().map(|v| v.to_json(canonical)).collect()),
            Bson::Binary(b) => json!({"$binary": {"base64": kernway_wire::scram::b64_encode(&b.bytes), "subType": format!("{:02x}", b.subtype)}}),
            Bson::Undefined => json!({"$undefined": true}),
            Bson::ObjectId(o) => json!({"$oid": o.to_hex()}),
            Bson::Boolean(b) => J::Bool(*b),
            Bson::DateTime(d) => match (canonical, d.to_rfc3339()) {
                (false, Some(s)) => json!({"$date": s}),
                _ => json!({"$date": {"$numberLong": d.0.to_string()}}),
            },
            Bson::Null => J::Null,
            Bson::RegularExpression(r) => json!({"$regularExpression": {"pattern": r.pattern, "options": r.options}}),
            Bson::DbPointer(ns, id) => json!({"$dbPointer": {"$ref": ns, "$id": {"$oid": id.to_hex()}}}),
            Bson::JavaScriptCode(c) => json!({"$code": c}),
            Bson::Symbol(s) => json!({"$symbol": s}),
            Bson::JavaScriptCodeWithScope(c, s) => json!({"$code": c, "$scope": Bson::Document(s.clone()).to_json(canonical)}),
            Bson::Int32(i) if canonical => json!({"$numberInt": i.to_string()}),
            Bson::Int32(i) => J::from(*i),
            Bson::Timestamp(t) => json!({"$timestamp": {"t": t.time, "i": t.increment}}),
            Bson::Int64(i) if canonical => json!({"$numberLong": i.to_string()}),
            Bson::Int64(i) => J::from(*i),
            Bson::Decimal128(d) => json!({"$numberDecimal": d.to_string()}),
            Bson::MinKey => json!({"$minKey": 1}),
            Bson::MaxKey => json!({"$maxKey": 1}),
        }
    }

    /// Parse Extended JSON (relaxed or canonical; plain JSON too). JSON
    /// integers become Int32 when they fit, else Int64; other numbers Double.
    ///
    /// Document keys come in the order the `serde_json::Value` keeps them —
    /// sorted, unless serde_json's `preserve_order` feature is on. Parse
    /// text with [`Bson::parse_extended_json`] to keep the order as written.
    pub fn from_extended_json(v: &serde_json::Value) -> Result<Bson, BsonError> {
        Bson::from_ordered(&OJ::from_value(v))
    }

    /// Parse Extended JSON text, keeping every document's keys in the order
    /// they are written (a command's name first, a sort's priorities).
    pub fn parse_extended_json(text: &str) -> Result<Bson, BsonError> {
        let v: OJ = serde_json::from_str(text).map_err(|e| BsonError(format!("Extended JSON: {e}")))?;
        Bson::from_ordered(&v)
    }

    /// Relaxed Extended JSON text, documents in their own key order.
    pub fn to_relaxed_json_string(&self) -> String {
        let mut out = String::new();
        crate::ordered_json::write(self, false, &mut out);
        out
    }

    /// Canonical Extended JSON text, documents in their own key order.
    pub fn to_canonical_json_string(&self) -> String {
        let mut out = String::new();
        crate::ordered_json::write(self, true, &mut out);
        out
    }

    pub(crate) fn from_ordered(v: &OJ) -> Result<Bson, BsonError> {
        use OJ as J;
        let bad = |m: &str| BsonError(format!("Extended JSON: {m}"));
        Ok(match v {
            J::Null => Bson::Null,
            J::Bool(b) => Bson::Boolean(*b),
            J::Number(n) => {
                if let Some(i) = n.as_i64() {
                    i32::try_from(i).map_or(Bson::Int64(i), Bson::Int32)
                } else if let Some(u) = n.as_u64() {
                    // Beyond i64: as a double, as JSON itself would.
                    Bson::Double(u as f64)
                } else {
                    Bson::Double(n.as_f64().unwrap_or(f64::NAN))
                }
            }
            J::String(s) => Bson::String(s.clone()),
            J::Array(a) => Bson::Array(a.iter().map(Bson::from_ordered).collect::<Result<_, _>>()?),
            J::Object(m) => {
                let key = |k: &str| m.get(k);
                let only = |k: &str| m.len() == 1 && m.contains_key(k);
                if only("$oid") {
                    let s = key("$oid").and_then(J::as_str).ok_or_else(|| bad("$oid must be a string"))?;
                    return ObjectId::parse_str(s).map(Bson::ObjectId).ok_or_else(|| bad("bad $oid"));
                }
                if only("$numberInt") {
                    let s = key("$numberInt").and_then(J::as_str).ok_or_else(|| bad("$numberInt must be a string"))?;
                    return s.parse().map(Bson::Int32).map_err(|_| bad("bad $numberInt"));
                }
                if only("$numberLong") {
                    let s = key("$numberLong").and_then(J::as_str).ok_or_else(|| bad("$numberLong must be a string"))?;
                    return s.parse().map(Bson::Int64).map_err(|_| bad("bad $numberLong"));
                }
                if only("$numberDouble") {
                    let s = key("$numberDouble").and_then(J::as_str).ok_or_else(|| bad("$numberDouble must be a string"))?;
                    return parse_double_text(s).map(Bson::Double).ok_or_else(|| bad("bad $numberDouble"));
                }
                if only("$numberDecimal") {
                    let s = key("$numberDecimal").and_then(J::as_str).ok_or_else(|| bad("$numberDecimal must be a string"))?;
                    return Decimal128::parse(s).map(Bson::Decimal128).ok_or_else(|| bad("bad $numberDecimal"));
                }
                if only("$date") {
                    return match key("$date") {
                        Some(J::String(s)) => DateTime::parse_rfc3339(s).map(Bson::DateTime).ok_or_else(|| bad("bad $date")),
                        Some(J::Number(n)) => n.as_i64().map(|i| Bson::DateTime(DateTime(i))).ok_or_else(|| bad("bad $date")),
                        Some(J::Object(o)) => o
                            .get("$numberLong")
                            .and_then(J::as_str)
                            .and_then(|s| s.parse().ok())
                            .map(|i| Bson::DateTime(DateTime(i)))
                            .ok_or_else(|| bad("bad $date")),
                        _ => Err(bad("bad $date")),
                    };
                }
                if only("$binary") {
                    let b = key("$binary").and_then(J::as_object).ok_or_else(|| bad("$binary must be an object"))?;
                    let bytes = b.get("base64").and_then(J::as_str).and_then(kernway_wire::scram::b64_decode).ok_or_else(|| bad("bad $binary.base64"))?;
                    let subtype = b
                        .get("subType")
                        .and_then(J::as_str)
                        .and_then(|s| u8::from_str_radix(s, 16).ok())
                        .ok_or_else(|| bad("bad $binary.subType"))?;
                    return Ok(Bson::Binary(Binary { subtype, bytes }));
                }
                if only("$uuid") {
                    let s = key("$uuid").and_then(J::as_str).ok_or_else(|| bad("$uuid must be a string"))?;
                    let hex: String = s.chars().filter(|c| *c != '-').collect();
                    if hex.len() != 32 {
                        return Err(bad("bad $uuid"));
                    }
                    let bytes = (0..16).map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)).collect::<Result<Vec<_>, _>>().map_err(|_| bad("bad $uuid"))?;
                    return Ok(Bson::Binary(Binary { subtype: 4, bytes }));
                }
                if only("$timestamp") {
                    let t = key("$timestamp").and_then(J::as_object).ok_or_else(|| bad("bad $timestamp"))?;
                    let get = |k: &str| t.get(k).and_then(J::as_u64).and_then(|v| u32::try_from(v).ok());
                    return match (get("t"), get("i")) {
                        (Some(time), Some(increment)) => Ok(Bson::Timestamp(Timestamp { time, increment })),
                        _ => Err(bad("bad $timestamp")),
                    };
                }
                if only("$regularExpression") {
                    let r = key("$regularExpression").and_then(J::as_object).ok_or_else(|| bad("bad $regularExpression"))?;
                    let pattern = r.get("pattern").and_then(J::as_str).ok_or_else(|| bad("bad $regularExpression"))?;
                    let options = r.get("options").and_then(J::as_str).ok_or_else(|| bad("bad $regularExpression"))?;
                    return Ok(Bson::RegularExpression(Regex::new(pattern, options)));
                }
                if only("$minKey") {
                    return Ok(Bson::MinKey);
                }
                if only("$maxKey") {
                    return Ok(Bson::MaxKey);
                }
                if only("$undefined") {
                    return Ok(Bson::Undefined);
                }
                if only("$symbol") {
                    return Ok(Bson::Symbol(key("$symbol").and_then(J::as_str).ok_or_else(|| bad("bad $symbol"))?.to_string()));
                }
                if only("$code") {
                    return Ok(Bson::JavaScriptCode(key("$code").and_then(J::as_str).ok_or_else(|| bad("bad $code"))?.to_string()));
                }
                if m.len() == 2 && m.contains_key("$code") && m.contains_key("$scope") {
                    let code = key("$code").and_then(J::as_str).ok_or_else(|| bad("bad $code"))?.to_string();
                    let Bson::Document(scope) = Bson::from_ordered(&m["$scope"])? else { return Err(bad("bad $scope")) };
                    return Ok(Bson::JavaScriptCodeWithScope(code, scope));
                }
                if only("$dbPointer") {
                    let p = key("$dbPointer").and_then(J::as_object).ok_or_else(|| bad("bad $dbPointer"))?;
                    let ns = p.get("$ref").and_then(J::as_str).ok_or_else(|| bad("bad $dbPointer"))?.to_string();
                    let Bson::ObjectId(id) = Bson::from_ordered(p.get("$id").ok_or_else(|| bad("bad $dbPointer"))?)? else {
                        return Err(bad("bad $dbPointer.$id"));
                    };
                    return Ok(Bson::DbPointer(ns, id));
                }
                let mut d = Document::new();
                for (k, v) in m {
                    d.insert(k.clone(), Bson::from_ordered(v)?);
                }
                Bson::Document(d)
            }
        })
    }
}

impl Regex {
    /// A regex with its options sorted, as BSON stores them.
    pub fn new(pattern: &str, options: &str) -> Self {
        let mut o: Vec<char> = options.chars().collect();
        o.sort_unstable();
        o.dedup();
        Regex { pattern: pattern.to_string(), options: o.into_iter().collect() }
    }
}

fn double_text(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else if f == 0.0 && f.is_sign_negative() {
        "-0.0".into()
    } else {
        let s = format!("{f:?}");
        s
    }
}

fn parse_double_text(s: &str) -> Option<f64> {
    match s {
        "NaN" => Some(f64::NAN),
        "Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        _ => s.parse().ok(),
    }
}

// ---- binary codec ----------------------------------------------------------

fn encode_document(d: &Document, out: &mut Vec<u8>) {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    for (k, v) in &d.entries {
        encode_element(k, v, out);
    }
    out.push(0);
    let len = (out.len() - start) as i32;
    out[start..start + 4].copy_from_slice(&len.to_le_bytes());
}

fn cstring(s: &str, out: &mut Vec<u8>) {
    // A key or pattern with NUL cannot be encoded: cut at the NUL rather than
    // corrupt the document (the server would reject it anyway).
    out.extend_from_slice(s.split('\0').next().unwrap_or("").as_bytes());
    out.push(0);
}

fn string(s: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(&((s.len() + 1) as i32).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

fn encode_element(key: &str, v: &Bson, out: &mut Vec<u8>) {
    out.push(v.element_type());
    cstring(key, out);
    encode_value(v, out);
}

fn encode_value(v: &Bson, out: &mut Vec<u8>) {
    match v {
        Bson::Double(f) => out.extend_from_slice(&f.to_le_bytes()),
        Bson::String(s) | Bson::JavaScriptCode(s) | Bson::Symbol(s) => string(s, out),
        Bson::Document(d) => encode_document(d, out),
        Bson::Array(a) => {
            let start = out.len();
            out.extend_from_slice(&[0; 4]);
            for (i, x) in a.iter().enumerate() {
                encode_element(&i.to_string(), x, out);
            }
            out.push(0);
            let len = (out.len() - start) as i32;
            out[start..start + 4].copy_from_slice(&len.to_le_bytes());
        }
        Bson::Binary(b) => {
            if b.subtype == 2 {
                // Old binary: an inner length.
                out.extend_from_slice(&((b.bytes.len() + 4) as i32).to_le_bytes());
                out.push(2);
                out.extend_from_slice(&(b.bytes.len() as i32).to_le_bytes());
            } else {
                out.extend_from_slice(&(b.bytes.len() as i32).to_le_bytes());
                out.push(b.subtype);
            }
            out.extend_from_slice(&b.bytes);
        }
        Bson::Undefined | Bson::Null | Bson::MinKey | Bson::MaxKey => {}
        Bson::ObjectId(o) => out.extend_from_slice(&o.0),
        Bson::Boolean(b) => out.push(u8::from(*b)),
        Bson::DateTime(d) => out.extend_from_slice(&d.0.to_le_bytes()),
        Bson::RegularExpression(r) => {
            cstring(&r.pattern, out);
            cstring(&r.options, out);
        }
        Bson::DbPointer(ns, id) => {
            string(ns, out);
            out.extend_from_slice(&id.0);
        }
        Bson::JavaScriptCodeWithScope(code, scope) => {
            let start = out.len();
            out.extend_from_slice(&[0; 4]);
            string(code, out);
            encode_document(scope, out);
            let len = (out.len() - start) as i32;
            out[start..start + 4].copy_from_slice(&len.to_le_bytes());
        }
        Bson::Int32(i) => out.extend_from_slice(&i.to_le_bytes()),
        Bson::Timestamp(t) => {
            out.extend_from_slice(&t.increment.to_le_bytes());
            out.extend_from_slice(&t.time.to_le_bytes());
        }
        Bson::Int64(i) => out.extend_from_slice(&i.to_le_bytes()),
        Bson::Decimal128(d) => out.extend_from_slice(&d.0),
    }
}

fn err(m: impl Into<String>) -> BsonError {
    BsonError(m.into())
}

struct Cur<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], BsonError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.b.len()).ok_or_else(|| err("truncated"))?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, BsonError> {
        Ok(self.take(1)?[0])
    }
    fn i32(&mut self) -> Result<i32, BsonError> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap_or([0; 4])))
    }
    fn i64(&mut self) -> Result<i64, BsonError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap_or([0; 8])))
    }
    fn cstr(&mut self) -> Result<String, BsonError> {
        let rest = &self.b[self.pos..];
        let n = rest.iter().position(|&c| c == 0).ok_or_else(|| err("unterminated key / cstring"))?;
        let s = std::str::from_utf8(&rest[..n]).map_err(|_| err("cstring is not UTF-8"))?.to_string();
        self.pos += n + 1;
        Ok(s)
    }
    fn string(&mut self) -> Result<String, BsonError> {
        let len = self.i32()?;
        if len < 1 {
            return Err(err(format!("string length {len}")));
        }
        let body = self.take(len as usize)?;
        let (text, nul) = body.split_at(body.len() - 1);
        if nul != [0] {
            return Err(err("string not NUL-terminated"));
        }
        Ok(std::str::from_utf8(text).map_err(|_| err("string is not UTF-8"))?.to_string())
    }
}

fn decode_document(b: &[u8], depth: usize) -> Result<(Document, usize), BsonError> {
    if depth > MAX_DEPTH {
        return Err(err("nested too deeply"));
    }
    if b.len() < 5 {
        return Err(err("document shorter than 5 bytes"));
    }
    let len = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    if len < 5 || len as usize > b.len() {
        return Err(err(format!("document length {len} with {} bytes available", b.len())));
    }
    let len = len as usize;
    if b[len - 1] != 0 {
        return Err(err("document not NUL-terminated"));
    }
    let mut c = Cur { b: &b[..len - 1], pos: 4 };
    let mut doc = Document::new();
    while c.pos < c.b.len() {
        let t = c.u8()?;
        let key = c.cstr()?;
        let v = decode_value(t, &mut c, depth)?;
        doc.entries.push((key, v));
    }
    Ok((doc, len))
}

fn decode_value(t: u8, c: &mut Cur<'_>, depth: usize) -> Result<Bson, BsonError> {
    Ok(match t {
        0x01 => Bson::Double(f64::from_bits(c.i64()? as u64)),
        0x02 => Bson::String(c.string()?),
        0x03 | 0x04 => {
            // An embedded document must end inside its parent.
            let (d, used) = decode_document(&c.b[c.pos..], depth + 1)?;
            c.pos += used;
            if t == 0x03 {
                Bson::Document(d)
            } else {
                Bson::Array(d.entries.into_iter().map(|(_, v)| v).collect())
            }
        }
        0x05 => {
            let len = c.i32()?;
            if len < 0 {
                return Err(err("negative binary length"));
            }
            let subtype = c.u8()?;
            let mut bytes = c.take(len as usize)?;
            if subtype == 2 {
                if bytes.len() < 4 {
                    return Err(err("old binary too short"));
                }
                let inner = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                if inner < 0 || inner as usize != bytes.len() - 4 {
                    return Err(err("old binary inner length mismatch"));
                }
                bytes = &bytes[4..];
            }
            Bson::Binary(Binary { subtype, bytes: bytes.to_vec() })
        }
        0x06 => Bson::Undefined,
        0x07 => Bson::ObjectId(ObjectId(c.take(12)?.try_into().unwrap_or([0; 12]))),
        0x08 => match c.u8()? {
            0 => Bson::Boolean(false),
            1 => Bson::Boolean(true),
            x => return Err(err(format!("boolean byte {x}"))),
        },
        0x09 => Bson::DateTime(DateTime(c.i64()?)),
        0x0A => Bson::Null,
        0x0B => {
            let pattern = c.cstr()?;
            let options = c.cstr()?;
            Bson::RegularExpression(Regex { pattern, options })
        }
        0x0C => {
            let ns = c.string()?;
            Bson::DbPointer(ns, ObjectId(c.take(12)?.try_into().unwrap_or([0; 12])))
        }
        0x0D => Bson::JavaScriptCode(c.string()?),
        0x0E => Bson::Symbol(c.string()?),
        0x0F => {
            let start = c.pos;
            let total = c.i32()?;
            let code = c.string()?;
            let (scope, used) = decode_document(&c.b[c.pos..], depth + 1)?;
            c.pos += used;
            if total < 0 || c.pos - start != total as usize {
                return Err(err("code-with-scope length mismatch"));
            }
            Bson::JavaScriptCodeWithScope(code, scope)
        }
        0x10 => Bson::Int32(c.i32()?),
        0x11 => {
            let increment = c.i32()? as u32;
            let time = c.i32()? as u32;
            Bson::Timestamp(Timestamp { time, increment })
        }
        0x12 => Bson::Int64(c.i64()?),
        0x13 => Bson::Decimal128(Decimal128(c.take(16)?.try_into().unwrap_or([0; 16]))),
        0xFF => Bson::MinKey,
        0x7F => Bson::MaxKey,
        other => return Err(err(format!("unknown element type {other:#04x}"))),
    })
}

// ---- serde bridge ------------------------------------------------------------

/// A serde value as a BSON document, through relaxed Extended JSON
/// (`ObjectId`, `DateTime`, `Decimal128`, `Binary` serialize to their
/// `$`-wrappers and come back typed). Fields keep their declaration order:
/// the value is written as JSON text (in order) and read back ordered —
/// never through `serde_json::Map`, which sorts.
pub fn to_document<T: serde::Serialize + ?Sized>(value: &T) -> Result<Document, BsonError> {
    let text = serde_json::to_string(value).map_err(|e| err(e.to_string()))?;
    match Bson::parse_extended_json(&text)? {
        Bson::Document(d) => Ok(d),
        other => Err(err(format!("expected a struct or map, got {other:?}"))),
    }
}

/// A BSON value as a serde type, through relaxed Extended JSON text (in
/// document order: a `Document` field comes back in the stored order).
pub fn from_bson<T: serde::de::DeserializeOwned>(b: &Bson) -> Result<T, BsonError> {
    serde_json::from_str(&b.to_relaxed_json_string()).map_err(|e| err(e.to_string()))
}

/// A BSON document as a serde type.
pub fn from_document<T: serde::de::DeserializeOwned>(d: &Document) -> Result<T, BsonError> {
    serde_json::from_str(&d.to_relaxed_json_string()).map_err(|e| err(e.to_string()))
}

macro_rules! via_json {
    ($t:ty, $variant:ident) => {
        impl serde::Serialize for $t {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                Bson::$variant(self.clone()).serialize(s)
            }
        }
        impl<'de> serde::Deserialize<'de> for $t {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let j = OJ::deserialize(d)?;
                match Bson::from_ordered(&j).map_err(serde::de::Error::custom)? {
                    Bson::$variant(v) => Ok(v),
                    other => Err(serde::de::Error::custom(format!(concat!("expected ", stringify!($variant), ", got {:?}"), other))),
                }
            }
        }
    };
}
via_json!(ObjectId, ObjectId);
via_json!(DateTime, DateTime);
via_json!(Decimal128, Decimal128);
via_json!(Binary, Binary);
via_json!(Timestamp, Timestamp);
via_json!(Document, Document);

/// Relaxed Extended JSON; documents as maps in their own key order.
impl serde::Serialize for Bson {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{SerializeMap, SerializeSeq};
        match self {
            Bson::Document(d) => {
                let mut m = s.serialize_map(Some(d.len()))?;
                for (k, v) in d.iter() {
                    m.serialize_entry(k, v)?;
                }
                m.end()
            }
            Bson::Array(a) => {
                let mut q = s.serialize_seq(Some(a.len()))?;
                for v in a {
                    q.serialize_element(v)?;
                }
                q.end()
            }
            other => other.to_relaxed_json().serialize(s),
        }
    }
}

impl<'de> serde::Deserialize<'de> for Bson {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let j = OJ::deserialize(d)?;
        Bson::from_ordered(&j).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02X}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn corpus_round_trips() {
        // From the BSON corpus (bson-corpus/tests/*.json): canonical bytes.
        for (bytes, json) in [
            ("160000000268656C6C6F0006000000776F726C640000", r#"{"hello":"world"}"#),
            ("0C0000001069000100000000", r#"{"i":{"$numberInt":"1"}}"#),
            ("10000000126100FFFFFFFFFFFFFF7F00", r#"{"a":{"$numberLong":"9223372036854775807"}}"#),
            ("10000000016400000000000000008000", r#"{"d":{"$numberDouble":"-0.0"}}"#),
            ("1400000007610056E1FC72E0C917E9C471416100", r#"{"a":{"$oid":"56e1fc72e0c917e9c4714161"}}"#),
            ("10000000096100000000000000000000", r#"{"a":{"$date":{"$numberLong":"0"}}}"#),
            ("080000000A610000", r#"{"a":null}"#),
            ("090000000862000100", r#"{"b":true}"#),
            ("0F0000000562000200000000FFFF00", r#"{"b":{"$binary":{"base64":"//8=","subType":"00"}}}"#),
            ("0F0000000B610061626300696D0000", r#"{"a":{"$regularExpression":{"pattern":"abc","options":"im"}}}"#),
            ("100000001161002A00000015CD5B0700", r#"{"a":{"$timestamp":{"t":123456789,"i":42}}}"#),
            ("080000007F610000", r#"{"a":{"$maxKey":1}}"#),
            ("08000000FF610000", r#"{"a":{"$minKey":1}}"#),
            ("160000000361000E0000000262000200000063000000", r#"{"a":{"b":"c"}}"#),
            ("140000000461000C0000001030000A0000000000", r#"{"a":[{"$numberInt":"10"}]}"#),
            ("180000001364000000000000000000000000000000403000", r#"{"d":{"$numberDecimal":"0"}}"#),
        ] {
            let raw = unhex(bytes);
            let d = Document::from_bytes(&raw).unwrap_or_else(|e| panic!("{bytes}: {e}"));
            assert_eq!(hex(&d.to_bytes()), bytes, "re-encode");
            let canonical = Bson::Document(d.clone()).to_canonical_json();
            assert_eq!(canonical, serde_json::from_str::<serde_json::Value>(json).unwrap(), "{bytes}");
            let back = Bson::from_extended_json(&canonical).unwrap();
            assert_eq!(back, Bson::Document(d));
        }
    }

    #[test]
    fn hostile_input_is_an_error() {
        for bad in [
            "",
            "05000000",                         // too short
            "0600000000",                       // length past the end
            "0500000001",                       // not NUL-terminated
            "0C0000000261000500000062636465", // string length past the document
            "0D0000000261000100000000FF00",   // trailing garbage inside
            "0C0000000861000200",             // boolean 2
            "0C000000206100000000000000",     // unknown type 0x20
            "1200000002610006000000E29882FF0000", // invalid UTF-8
        ] {
            assert!(Document::from_bytes(&unhex(bad)).is_err(), "{bad}");
        }
        // Deep nesting is refused, not a stack overflow.
        let mut d = Document::new();
        for _ in 0..300 {
            d = doc! { "a" => d };
        }
        assert!(Document::from_bytes(&d.to_bytes()).is_err());
    }

    #[test]
    fn decimal128_text() {
        for (s, canonical) in [
            ("0", "0"),
            ("-0", "-0"),
            ("1.5", "1.5"),
            ("-1.50", "-1.50"),
            ("0.001", "0.001"),
            ("1E+3", "1E+3"),
            ("1000", "1000"),
            ("0.0000001", "1E-7"),
            ("12345678901234567890123456789012.34", "12345678901234567890123456789012.34"),
            ("9.999999999999999999999999999999999E+6144", "9.999999999999999999999999999999999E+6144"),
            ("1E-6176", "1E-6176"),
            ("NaN", "NaN"),
            ("-Infinity", "-Infinity"),
            ("Inf", "Infinity"),
        ] {
            let d = Decimal128::parse(s).unwrap_or_else(|| panic!("{s}"));
            assert_eq!(d.to_string(), canonical, "{s}");
        }
        assert!(Decimal128::parse("1.2.3").is_none());
        assert!(Decimal128::parse("12345678901234567890123456789012345").is_none(), "35 significant digits");
        assert!(Decimal128::parse("1E-6177").is_none());
        assert!(Decimal128::parse("e5").is_none());
    }

    #[test]
    fn dates_and_ids() {
        let d = DateTime::parse_rfc3339("2024-02-29T12:34:56.789Z").unwrap();
        assert_eq!(d.to_rfc3339().as_deref(), Some("2024-02-29T12:34:56.789Z"));
        assert_eq!(DateTime::parse_rfc3339("1970-01-01T01:00:00+01:00"), Some(DateTime(0)));
        assert_eq!(DateTime(0).to_rfc3339().as_deref(), Some("1970-01-01T00:00:00.000Z"));
        assert!(DateTime::parse_rfc3339("2024-13-01T00:00:00Z").is_none());
        let a = ObjectId::new();
        let b = ObjectId::new();
        assert_ne!(a, b);
        assert_eq!(ObjectId::parse_str(&a.to_hex()), Some(a));
        assert!(ObjectId::parse_str("xyz").is_none());
        assert!(a.timestamp() > 1_700_000_000);
    }

    #[test]
    fn serde_bridge() {
        #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
        struct Item {
            _id: ObjectId,
            name: String,
            qty: i64,
            price: f64,
            at: DateTime,
            tags: Vec<String>,
            note: Option<String>,
        }
        let it = Item { _id: ObjectId::new(), name: "Việt".into(), qty: 1 << 40, price: 2.5, at: DateTime(1_700_000_000_123), tags: vec!["a".into()], note: None };
        let d = to_document(&it).unwrap();
        assert!(matches!(d.get("_id"), Some(Bson::ObjectId(_))));
        assert!(matches!(d.get("at"), Some(Bson::DateTime(_))));
        assert_eq!(d.get("qty"), Some(&Bson::Int64(1 << 40)));
        let back: Item = from_document(&Document::from_bytes(&d.to_bytes()).unwrap()).unwrap();
        assert_eq!(back, it);
    }

    #[test]
    fn key_order_is_kept_through_json_and_serde() {
        let keys = |b: &Bson| match b {
            Bson::Document(d) => d.iter().map(|(k, _)| k.to_string()).collect::<Vec<String>>(),
            other => panic!("not a document: {other:?}"),
        };
        // Text in, order kept (serde_json::Map would sort: a, m, z).
        let b = Bson::parse_extended_json(r#"{"z": 1, "a": {"y": 1, "b": 2}, "m": {"$oid": "5f0c2b8e9d1e8a3c4b5a6978"}}"#).unwrap();
        assert_eq!(keys(&b), ["z", "a", "m"]);
        let Bson::Document(d) = &b else { unreachable!() };
        assert_eq!(keys(d.get("a").unwrap()), ["y", "b"], "nested documents too");
        assert!(matches!(d.get("m"), Some(Bson::ObjectId(_))), "Extended JSON wrappers still typed");
        // Text out, order kept.
        assert_eq!(b.to_relaxed_json_string(), r#"{"z":1,"a":{"y":1,"b":2},"m":{"$oid":"5f0c2b8e9d1e8a3c4b5a6978"}}"#);
        assert_eq!(serde_json::to_string(&b).unwrap(), b.to_relaxed_json_string(), "Serialize keeps the order");
        assert_eq!(Bson::Int32(1).to_canonical_json_string(), r#"{"$numberInt":"1"}"#);
        // A struct: fields in declaration order, a Document field in its own order.
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Ticket {
            zone: String,
            amount: i64,
            meta: Document,
            items: Vec<Item>,
        }
        #[derive(serde::Serialize, serde::Deserialize)]
        struct Item {
            sku: String,
            cents: i64,
        }
        let mut meta = Document::new();
        meta.insert("tip", Bson::Int32(5));
        meta.insert("cash", Bson::Boolean(true));
        let t = Ticket { zone: "A".into(), amount: 10, meta, items: vec![Item { sku: "gel".into(), cents: 3500 }] };
        let d = to_document(&t).unwrap();
        assert_eq!(keys(&Bson::Document(d.clone())), ["zone", "amount", "meta", "items"]);
        assert_eq!(keys(d.get("meta").unwrap()), ["tip", "cash"]);
        let Some(Bson::Array(items)) = d.get("items") else { panic!() };
        assert_eq!(keys(&items[0]), ["sku", "cents"]);
        // And back: a Document field deserializes in its order.
        let back: Ticket = from_document(&d).unwrap();
        assert_eq!(keys(&Bson::Document(back.meta)), ["tip", "cash"]);
        // Duplicate keys: the last value, in the first position.
        let b = Bson::parse_extended_json(r#"{"a": 1, "b": 2, "a": 3}"#).unwrap();
        assert_eq!(b.to_relaxed_json_string(), r#"{"a":3,"b":2}"#);
        assert!(Bson::parse_extended_json("{").is_err());
    }

}
