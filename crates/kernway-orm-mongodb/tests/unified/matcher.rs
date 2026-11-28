//! "Evaluating Matches" of the unified test format: root-level documents may
//! hold extra fields, nested ones may not; numbers compare by value; arrays
//! have the same length; `$$`-operators.

use super::entities::Entities;
use kernway_orm_mongodb::{Bson, Document};

/// Why `actual` does not match `expected` (a path and the difference), or `Ok`.
pub fn matches(expected: &Bson, actual: Option<&Bson>, root: bool, entities: &Entities) -> Result<(), String> {
    m(expected, actual, root, entities, "$")
}

fn operator(d: &Document) -> Option<(&str, &Bson)> {
    if d.len() == 1 {
        let (k, v) = d.iter().next()?;
        if k.starts_with("$$") {
            return Some((k, v));
        }
    }
    None
}

fn number(b: &Bson) -> Option<f64> {
    match b {
        Bson::Int32(i) => Some(*i as f64),
        Bson::Int64(i) => Some(*i as f64),
        Bson::Double(f) => Some(*f),
        _ => None,
    }
}

/// The `$type` alias of a value.
pub fn type_name(b: &Bson) -> &'static str {
    match b {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Document(_) => "object",
        Bson::Array(_) => "array",
        Bson::Binary(_) => "binData",
        Bson::Undefined => "undefined",
        Bson::ObjectId(_) => "objectId",
        Bson::Boolean(_) => "bool",
        Bson::DateTime(_) => "date",
        Bson::Null => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::DbPointer(..) => "dbPointer",
        Bson::JavaScriptCode(_) => "javascript",
        Bson::Symbol(_) => "symbol",
        Bson::JavaScriptCodeWithScope(..) => "javascriptWithScope",
        Bson::Int32(_) => "int",
        Bson::Timestamp(_) => "timestamp",
        Bson::Int64(_) => "long",
        Bson::Decimal128(_) => "decimal",
        Bson::MinKey => "minKey",
        Bson::MaxKey => "maxKey",
    }
}

fn special(op: &str, arg: &Bson, actual: Option<&Bson>, root: bool, e: &Entities, path: &str) -> Result<(), String> {
    match op {
        // Nested in another operator ($$unsetOrMatches): the value's presence.
        "$$exists" => {
            let want = arg.as_bool().unwrap_or(true);
            if actual.is_some() == want {
                Ok(())
            } else {
                Err(format!("{path}: exists={}", actual.is_some()))
            }
        }
        "$$type" => {
            let a = actual.ok_or_else(|| format!("{path}: missing, expected $$type {arg:?}"))?;
            let names: Vec<&str> = match arg {
                Bson::String(s) => vec![s.as_str()],
                Bson::Array(v) => v.iter().filter_map(Bson::as_str).collect(),
                _ => return Err(format!("{path}: bad $$type argument")),
            };
            let t = type_name(a);
            let ok = names.iter().any(|n| *n == t || (*n == "number" && matches!(t, "int" | "long" | "double" | "decimal")));
            if ok {
                Ok(())
            } else {
                Err(format!("{path}: type {t} not in {names:?}"))
            }
        }
        "$$unsetOrMatches" => match actual {
            None => Ok(()),
            Some(a) => m(arg, Some(a), root, e, path),
        },
        "$$matchesEntity" => {
            let name = arg.as_str().ok_or_else(|| format!("{path}: bad $$matchesEntity"))?;
            let v = e.value(name).ok_or_else(|| format!("{path}: no BSON entity {name}"))?;
            m(&v, actual, root, e, path)
        }
        "$$matchesHexBytes" => {
            let hex = arg.as_str().ok_or_else(|| format!("{path}: bad $$matchesHexBytes"))?;
            let want = super::hex(hex).ok_or_else(|| format!("{path}: malformed hex {hex}"))?;
            match actual {
                Some(Bson::Binary(b)) if b.bytes == want => Ok(()),
                other => Err(format!("{path}: {other:?} is not the bytes {hex}")),
            }
        }
        "$$sessionLsid" => {
            let name = arg.as_str().ok_or_else(|| format!("{path}: bad $$sessionLsid"))?;
            let lsid = e.session_lsid(name).ok_or_else(|| format!("{path}: no session {name}"))?;
            match actual {
                Some(Bson::Document(d)) if *d == lsid => Ok(()),
                other => Err(format!("{path}: {other:?} is not session {name}'s lsid {lsid:?}")),
            }
        }
        "$$lte" | "$$gte" => {
            let want = number(arg).ok_or_else(|| format!("{path}: bad {op}"))?;
            let got = actual.and_then(number).ok_or_else(|| format!("{path}: {actual:?} is not a number"))?;
            let ok = if op == "$$lte" { got <= want } else { got >= want };
            if ok {
                Ok(())
            } else {
                Err(format!("{path}: {got} is not {op} {want}"))
            }
        }
        "$$matchAsDocument" => {
            let s = actual.and_then(Bson::as_str).ok_or_else(|| format!("{path}: {actual:?} is not an Extended JSON string"))?;
            let parsed = Bson::parse_extended_json(s).map_err(|err| format!("{path}: {err}"))?;
            m(arg, Some(&parsed), false, e, path)
        }
        "$$matchAsRoot" => m(arg, actual, true, e, path),
        other => Err(format!("{path}: unknown operator {other}")),
    }
}

fn m(expected: &Bson, actual: Option<&Bson>, root: bool, e: &Entities, path: &str) -> Result<(), String> {
    match expected {
        Bson::Document(exp) => {
            if let Some((op, arg)) = operator(exp) {
                return special(op, arg, actual, root, e, path);
            }
            let Some(Bson::Document(act)) = actual else {
                return Err(format!("{path}: expected a document, got {actual:?}"));
            };
            for (k, v) in exp.iter() {
                let p = format!("{path}.{k}");
                if let Bson::Document(vd) = v {
                    if let Some(("$$exists", want)) = operator(vd) {
                        let has = act.contains_key(k);
                        if has != want.as_bool().unwrap_or(true) {
                            return Err(format!("{p}: exists={has}"));
                        }
                        continue;
                    }
                    if let Some(("$$unsetOrMatches", arg)) = operator(vd) {
                        match act.get(k) {
                            None => continue,
                            Some(a) => {
                                m(arg, Some(a), false, e, &p)?;
                                continue;
                            }
                        }
                    }
                }
                m(v, act.get(k), false, e, &p)?;
            }
            if !root {
                if let Some(extra) = act.keys().find(|k| !exp.contains_key(k)) {
                    return Err(format!("{path}: unexpected field {extra}"));
                }
            }
            Ok(())
        }
        Bson::Array(exp) => {
            let Some(Bson::Array(act)) = actual else {
                return Err(format!("{path}: expected an array, got {actual:?}"));
            };
            if exp.len() != act.len() {
                return Err(format!("{path}: {} elements, expected {}: {act:?}", act.len(), exp.len()));
            }
            for (i, (x, y)) in exp.iter().zip(act).enumerate() {
                m(x, Some(y), false, e, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        _ => {
            let a = actual.ok_or_else(|| format!("{path}: missing, expected {expected:?}"))?;
            if let (Some(x), Some(y)) = (number(expected), number(a)) {
                return if x == y || (x.is_nan() && y.is_nan()) { Ok(()) } else { Err(format!("{path}: {y} ≠ {x}")) };
            }
            if expected == a || canonical_eq(expected, a) {
                Ok(())
            } else {
                Err(format!("{path}: {a:?} ≠ {expected:?}"))
            }
        }
    }
}

/// Equality for values with embedded documents (code with scope): key order free.
fn canonical_eq(a: &Bson, b: &Bson) -> bool {
    let norm = |v: &Bson| serde_json::to_string(&sorted(v.to_canonical_json())).unwrap_or_default();
    norm(a) == norm(b)
}

fn sorted(v: serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => {
            let b: std::collections::BTreeMap<String, serde_json::Value> = m.into_iter().map(|(k, v)| (k, sorted(v))).collect();
            serde_json::Value::Object(b.into_iter().collect())
        }
        serde_json::Value::Array(a) => serde_json::Value::Array(a.into_iter().map(sorted).collect()),
        other => other,
    }
}
