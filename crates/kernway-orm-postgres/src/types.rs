//! PostgreSQL values on the wire: which types travel in binary, how a
//! result value becomes a [`Value`] / an entity [`Cell`], and how a
//! [`Param`] is sent for the type the server expects.
//!
//! Integers, floats, booleans and `bytea` come back in binary (no parsing,
//! no hex); everything else in text — PostgreSQL's own canonical rendering
//! with `DateStyle=ISO` and `extra_float_digits=3`, which is exact for
//! `numeric`, dates, times, intervals, JSON, UUIDs, network types, ranges.

use kernway_orm_core::decode::Cell;
use kernway_orm_sql::{Param, Value};

/// Type OIDs (`pg_type.oid`) the driver treats specially.
pub mod oid {
    #![allow(missing_docs)]
    pub const BOOL: u32 = 16;
    pub const BYTEA: u32 = 17;
    pub const CHAR: u32 = 18;
    pub const NAME: u32 = 19;
    pub const INT8: u32 = 20;
    pub const INT2: u32 = 21;
    pub const INT4: u32 = 23;
    pub const TEXT: u32 = 25;
    pub const OID: u32 = 26;
    pub const JSON: u32 = 114;
    pub const FLOAT4: u32 = 700;
    pub const FLOAT8: u32 = 701;
    pub const UNKNOWN: u32 = 705;
    pub const MONEY: u32 = 790;
    pub const BPCHAR: u32 = 1042;
    pub const VARCHAR: u32 = 1043;
    pub const DATE: u32 = 1082;
    pub const TIME: u32 = 1083;
    pub const TIMESTAMP: u32 = 1114;
    pub const TIMESTAMPTZ: u32 = 1184;
    pub const INTERVAL: u32 = 1186;
    pub const TIMETZ: u32 = 1266;
    pub const NUMERIC: u32 = 1700;
    pub const UUID: u32 = 2950;
    pub const JSONB: u32 = 3802;
}

/// The element type of a built-in array type, `None` for anything else.
pub fn array_element(array_oid: u32) -> Option<u32> {
    Some(match array_oid {
        1000 => oid::BOOL,
        1001 => oid::BYTEA,
        1002 => oid::CHAR,
        1003 => oid::NAME,
        1005 => oid::INT2,
        1007 => oid::INT4,
        1009 => oid::TEXT,
        1014 => oid::BPCHAR,
        1015 => oid::VARCHAR,
        1016 => oid::INT8,
        1021 => oid::FLOAT4,
        1022 => oid::FLOAT8,
        1028 => oid::OID,
        1182 => oid::DATE,
        1183 => oid::TIME,
        1115 => oid::TIMESTAMP,
        1185 => oid::TIMESTAMPTZ,
        1187 => oid::INTERVAL,
        1270 => oid::TIMETZ,
        1231 => oid::NUMERIC,
        2951 => oid::UUID,
        199 => oid::JSON,
        3807 => oid::JSONB,
        791 => oid::MONEY,
        _ => return None,
    })
}

/// The result format to ask for: 1 (binary) for the fixed-size numbers,
/// booleans and `bytea`, 0 (text) for the rest.
pub fn result_format(type_oid: u32) -> i16 {
    i16::from(matches!(type_oid, oid::BOOL | oid::BYTEA | oid::INT2 | oid::INT4 | oid::INT8 | oid::OID | oid::FLOAT4 | oid::FLOAT8))
}

/// The OID a parameter is declared with in `Parse`: numbers and booleans
/// with their own type (so `SELECT $1` is an integer, not text), text and
/// NULL left for the server to infer from the statement.
pub fn param_oid(p: &Param) -> u32 {
    match p {
        Param::Int(_) => oid::INT8,
        Param::UInt(u) if i64::try_from(*u).is_ok() => oid::INT8,
        Param::UInt(_) => oid::NUMERIC,
        Param::Float(_) => oid::FLOAT8,
        Param::Bool(_) => oid::BOOL,
        Param::Bytes(_) => oid::BYTEA,
        Param::Null | Param::Str(_) | Param::Array(_) | Param::Object(_) => 0,
    }
}

/// A parameter in the wire form for type `target` (the server's view of
/// the parameter, from `ParameterDescription`): (format, bytes; `None` = NULL).
pub fn encode_param(p: &Param, target: u32) -> (i16, Option<Vec<u8>>) {
    let text = |s: String| (0, Some(s.into_bytes()));
    match p {
        Param::Null => (0, None),
        Param::Bool(b) if matches!(target, oid::INT2 | oid::INT4 | oid::INT8) => text(i64::from(*b).to_string()),
        Param::Bool(b) => text(if *b { "t" } else { "f" }.into()),
        Param::Int(i) if target == oid::BOOL => text(if *i != 0 { "t" } else { "f" }.into()),
        Param::Int(i) => text(i.to_string()),
        Param::UInt(u) => text(u.to_string()),
        Param::Float(f) => text(float_text(*f)),
        Param::Str(s) if target == oid::BYTEA => (1, Some(s.as_bytes().to_vec())),
        Param::Str(s) => text(s.clone()),
        Param::Bytes(b) if target == oid::BYTEA || target == 0 => (1, Some(b.clone())),
        Param::Bytes(b) => text(String::from_utf8_lossy(b).into_owned()),
        Param::Array(items) if array_element(target).is_some() => text(array_literal(items)),
        p @ (Param::Array(_) | Param::Object(_)) => text(p.to_json().to_string()),
    }
}

fn float_text(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f.is_infinite() {
        if f > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else {
        // Shortest round-trip form (`{:e}` keeps 1e-300 short).
        format!("{f:e}")
    }
}

/// `{a,"b c",NULL,{1,2}}` — an array literal the server parses for any element type.
fn array_literal(items: &[Param]) -> String {
    let mut s = String::from("{");
    for (i, p) in items.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        match p {
            Param::Null => s.push_str("NULL"),
            Param::Array(inner) => s.push_str(&array_literal(inner)),
            Param::Bytes(b) => {
                s.push_str("\"\\\\x");
                for x in b {
                    s.push_str(&format!("{x:02x}"));
                }
                s.push('"');
            }
            other => {
                let t = match other {
                    Param::Bool(b) => if *b { "t" } else { "f" }.to_string(),
                    Param::Int(i) => i.to_string(),
                    Param::UInt(u) => u.to_string(),
                    Param::Float(f) => float_text(*f),
                    Param::Str(t) => t.clone(),
                    o => o.to_json().to_string(),
                };
                s.push('"');
                for c in t.chars() {
                    if c == '"' || c == '\\' {
                        s.push('\\');
                    }
                    s.push(c);
                }
                s.push('"');
            }
        }
    }
    s.push('}');
    s
}

/// A result value as an entity-decoder cell, borrowing the wire bytes.
pub fn cell<'a>(type_oid: u32, format: i16, raw: Option<&'a [u8]>) -> Cell<'a> {
    let Some(b) = raw else { return Cell::Null };
    if format == 0 {
        return match type_oid {
            oid::BOOL => Cell::Bool(b == b"t"),
            _ => Cell::Bytes(b),
        };
    }
    match binary_number(type_oid, b) {
        Some(Bin::Bool(v)) => Cell::Bool(v),
        Some(Bin::Int(v)) => Cell::Int(v),
        Some(Bin::Float(v)) => Cell::F64(v),
        None => Cell::Bytes(b),
    }
}

enum Bin {
    Bool(bool),
    Int(i64),
    Float(f64),
}

fn binary_number(type_oid: u32, b: &[u8]) -> Option<Bin> {
    Some(match (type_oid, b.len()) {
        (oid::BOOL, 1) => Bin::Bool(b[0] != 0),
        (oid::INT2, 2) => Bin::Int(i64::from(i16::from_be_bytes([b[0], b[1]]))),
        (oid::INT4, 4) => Bin::Int(i64::from(i32::from_be_bytes(b.try_into().ok()?))),
        (oid::OID, 4) => Bin::Int(i64::from(u32::from_be_bytes(b.try_into().ok()?))),
        (oid::INT8, 8) => Bin::Int(i64::from_be_bytes(b.try_into().ok()?)),
        // A real keeps its shortest decimal form (0.1, not 0.10000000149).
        (oid::FLOAT4, 4) => {
            let f = f32::from_be_bytes(b.try_into().ok()?);
            Bin::Float(if f.is_finite() { f.to_string().parse().unwrap_or(f64::from(f)) } else { f64::from(f) })
        }
        (oid::FLOAT8, 8) => Bin::Float(f64::from_be_bytes(b.try_into().ok()?)),
        _ => return None,
    })
}

/// A result value, typed by its column type.
pub fn value(type_oid: u32, format: i16, raw: Option<&[u8]>) -> Value {
    let Some(b) = raw else { return Value::Null };
    if format == 1 {
        return match binary_number(type_oid, b) {
            Some(Bin::Bool(v)) => Value::Bool(v),
            Some(Bin::Int(v)) => Value::Int(v),
            Some(Bin::Float(v)) => Value::Float(v),
            None => Value::Bytes(b.to_vec()),
        };
    }
    text_value(type_oid, &String::from_utf8_lossy(b))
}

/// A text-format value (simple-query results, array elements).
pub fn text_value(type_oid: u32, t: &str) -> Value {
    let s = || t.to_string();
    match type_oid {
        oid::BOOL => Value::Bool(t == "t"),
        oid::INT2 | oid::INT4 | oid::INT8 | oid::OID => t.parse().map_or_else(|_| Value::Text(s()), Value::Int),
        oid::FLOAT4 | oid::FLOAT8 => match t {
            "NaN" => Value::Float(f64::NAN),
            "Infinity" => Value::Float(f64::INFINITY),
            "-Infinity" => Value::Float(f64::NEG_INFINITY),
            _ => t.parse().map_or_else(|_| Value::Text(s()), Value::Float),
        },
        oid::NUMERIC => Value::Decimal(s()),
        oid::BYTEA => match t.strip_prefix("\\x").and_then(unhex) {
            Some(b) => Value::Bytes(b),
            None => Value::Bytes(t.as_bytes().to_vec()),
        },
        oid::JSON | oid::JSONB => Value::Json(s()),
        oid::DATE => Value::Date(s()),
        oid::TIME | oid::TIMETZ => Value::Time(s()),
        oid::TIMESTAMP => Value::DateTime(s()),
        oid::TIMESTAMPTZ => Value::DateTime(spaced_offset(t)),
        oid::INTERVAL => Value::Interval(s()),
        other => match array_element(other) {
            Some(elem) => parse_array(t, elem).unwrap_or_else(|| Value::Text(s())),
            None => Value::Text(s()),
        },
    }
}

/// `2024-01-02 03:04:05+07` → `2024-01-02 03:04:05 +07:00` (the shared
/// `Value::DateTime` shape).
fn spaced_offset(t: &str) -> String {
    let Some(pos) = t.rfind(['+', '-']).filter(|&p| p > 10) else { return t.to_string() };
    let (dt, off) = t.split_at(pos);
    let (sign, hm) = off.split_at(1);
    let mut parts = hm.split(':');
    let h = parts.next().unwrap_or("00");
    let m = parts.next().unwrap_or("00");
    match parts.next() {
        Some(sec) => format!("{dt} {sign}{h}:{m}:{sec}"),
        None => format!("{dt} {sign}{h}:{m}"),
    }
}

fn unhex(h: &str) -> Option<Vec<u8>> {
    if !h.len().is_multiple_of(2) {
        return None;
    }
    (0..h.len()).step_by(2).map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok()).collect()
}

/// Parse an array's text form (`{1,2,NULL}`, `{{a,b},{c,d}}`,
/// `[0:1]={1,2}`) into nested [`Value::Array`]s of `elem`-typed values.
pub fn parse_array(t: &str, elem: u32) -> Option<Value> {
    let t = match t.find("={") {
        Some(p) if t.starts_with('[') => &t[p + 1..],
        _ => t,
    };
    let chars: Vec<char> = t.chars().collect();
    let mut pos = 0;
    let v = parse_level(&chars, &mut pos, elem)?;
    (pos == chars.len()).then_some(v)
}

fn parse_level(c: &[char], pos: &mut usize, elem: u32) -> Option<Value> {
    if c.get(*pos) != Some(&'{') {
        return None;
    }
    *pos += 1;
    let mut items = Vec::new();
    if c.get(*pos) == Some(&'}') {
        *pos += 1;
        return Some(Value::Array(items));
    }
    loop {
        match c.get(*pos)? {
            '{' => items.push(parse_level(c, pos, elem)?),
            '"' => {
                *pos += 1;
                let mut s = String::new();
                loop {
                    match c.get(*pos)? {
                        '\\' => {
                            s.push(*c.get(*pos + 1)?);
                            *pos += 2;
                        }
                        '"' => {
                            *pos += 1;
                            break;
                        }
                        ch => {
                            s.push(*ch);
                            *pos += 1;
                        }
                    }
                }
                items.push(text_value(elem, &s));
            }
            _ => {
                let start = *pos;
                while !matches!(c.get(*pos)?, ',' | '}') {
                    *pos += 1;
                }
                let s: String = c[start..*pos].iter().collect();
                items.push(if s == "NULL" { Value::Null } else { text_value(elem, s.trim()) });
            }
        }
        match c.get(*pos)? {
            ',' => *pos += 1,
            '}' => {
                *pos += 1;
                return Some(Value::Array(items));
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arrays_parse() {
        assert_eq!(parse_array("{1,2,NULL}", oid::INT4), Some(Value::Array(vec![Value::Int(1), Value::Int(2), Value::Null])));
        assert_eq!(
            parse_array(r#"{"a b","c\"d",NULL,"NULL",e}"#, oid::TEXT),
            Some(Value::Array(vec![
                Value::Text("a b".into()),
                Value::Text("c\"d".into()),
                Value::Null,
                Value::Text("NULL".into()),
                Value::Text("e".into())
            ]))
        );
        assert_eq!(
            parse_array("{{1,2},{3,4}}", oid::INT8),
            Some(Value::Array(vec![Value::Array(vec![Value::Int(1), Value::Int(2)]), Value::Array(vec![Value::Int(3), Value::Int(4)])]))
        );
        assert_eq!(parse_array("[0:1]={t,f}", oid::BOOL), Some(Value::Array(vec![Value::Bool(true), Value::Bool(false)])));
        assert_eq!(parse_array("{}", oid::INT4), Some(Value::Array(vec![])));
        assert_eq!(parse_array("{1,2", oid::INT4), None);
    }

    #[test]
    fn array_literals_quote() {
        let lit = array_literal(&[Param::Str("a\"b\\c".into()), Param::Null, Param::Int(3), Param::Array(vec![Param::Bool(true)])]);
        assert_eq!(lit, r#"{"a\"b\\c",NULL,"3",{"t"}}"#);
        assert_eq!(parse_array(&lit, oid::TEXT).unwrap(), Value::Array(vec![
            Value::Text("a\"b\\c".into()), Value::Null, Value::Text("3".into()), Value::Array(vec![Value::Text("t".into())])
        ]));
    }

    #[test]
    fn values_by_type() {
        assert_eq!(value(oid::INT4, 1, Some(&(-7i32).to_be_bytes())), Value::Int(-7));
        assert_eq!(value(oid::FLOAT4, 1, Some(&0.1f32.to_be_bytes())), Value::Float(0.1));
        assert_eq!(value(oid::BOOL, 1, Some(&[1])), Value::Bool(true));
        assert_eq!(value(oid::BYTEA, 1, Some(b"\x00\xff")), Value::Bytes(vec![0, 255]));
        assert_eq!(text_value(oid::BYTEA, "\\x00ff"), Value::Bytes(vec![0, 255]));
        assert_eq!(text_value(oid::NUMERIC, "12.50"), Value::Decimal("12.50".into()));
        assert_eq!(text_value(oid::TIMESTAMPTZ, "2024-01-02 03:04:05.5+07"), Value::DateTime("2024-01-02 03:04:05.5 +07:00".into()));
        assert_eq!(text_value(oid::TIMESTAMPTZ, "2024-01-02 03:04:05-03:30"), Value::DateTime("2024-01-02 03:04:05 -03:30".into()));
        assert_eq!(text_value(oid::TIMESTAMPTZ, "infinity"), Value::DateTime("infinity".into()));
        assert_eq!(value(oid::INT8, 0, None), Value::Null);
        assert_eq!(text_value(1007, "{1,2}"), Value::Array(vec![Value::Int(1), Value::Int(2)]));
    }

    #[test]
    fn params_encode_for_the_target() {
        assert_eq!(encode_param(&Param::Float(1e-300), oid::FLOAT8), (0, Some(b"1e-300".to_vec())));
        assert_eq!(encode_param(&Param::Float(f64::NEG_INFINITY), oid::FLOAT8), (0, Some(b"-Infinity".to_vec())));
        assert_eq!(encode_param(&Param::Bool(true), oid::INT4), (0, Some(b"1".to_vec())));
        assert_eq!(encode_param(&Param::Int(0), oid::BOOL), (0, Some(b"f".to_vec())));
        assert_eq!(encode_param(&Param::Bytes(vec![1, 2]), oid::BYTEA), (1, Some(vec![1, 2])));
        assert_eq!(encode_param(&Param::Array(vec![Param::Int(1)]), 1007), (0, Some(b"{\"1\"}".to_vec())));
        assert_eq!(encode_param(&Param::Array(vec![Param::Int(1)]), oid::JSONB), (0, Some(b"[1]".to_vec())));
        assert_eq!(encode_param(&Param::Null, oid::INT4), (0, None));
    }
}
