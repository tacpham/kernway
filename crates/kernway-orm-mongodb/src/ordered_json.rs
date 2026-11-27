//! JSON that keeps the order of object keys.
//!
//! BSON documents are ordered (a command's name comes first, a `sort`
//! spec's keys are its priority), but `serde_json::Map` sorts its keys
//! unless the whole build enables serde_json's `preserve_order` — and that
//! feature is unified across a workspace, changing other crates too. So
//! text is parsed into this order-keeping value instead, and documents are
//! written out by [`write`] in their own order. The variant and method names
//! mirror `serde_json::Value`, so Extended JSON is read by one function
//! from either.

use crate::bson::Bson;
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use std::fmt;

/// A JSON value with ordered object keys.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OJ {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<OJ>),
    Object(OMap),
}

/// An object: keys in the order they were written (a repeated key keeps
/// its last value, in its first position — as JSON parsers do).
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct OMap(Vec<(String, OJ)>);

impl OMap {
    pub(crate) fn get(&self, k: &str) -> Option<&OJ> {
        self.0.iter().find(|(n, _)| n == k).map(|(_, v)| v)
    }
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }
    pub(crate) fn contains_key(&self, k: &str) -> bool {
        self.get(k).is_some()
    }
    fn insert(&mut self, k: String, v: OJ) {
        match self.0.iter_mut().find(|(n, _)| *n == k) {
            Some(slot) => slot.1 = v,
            None => self.0.push((k, v)),
        }
    }
}

impl std::ops::Index<&str> for OMap {
    type Output = OJ;
    fn index(&self, k: &str) -> &OJ {
        self.get(k).unwrap_or(&OJ::Null)
    }
}

impl<'a> IntoIterator for &'a OMap {
    type Item = (&'a String, &'a OJ);
    type IntoIter = std::iter::Map<std::slice::Iter<'a, (String, OJ)>, fn(&'a (String, OJ)) -> (&'a String, &'a OJ)>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter().map(|(k, v)| (k, v))
    }
}

impl OJ {
    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            OJ::String(s) => Some(s),
            _ => None,
        }
    }
    pub(crate) fn as_object(&self) -> Option<&OMap> {
        match self {
            OJ::Object(m) => Some(m),
            _ => None,
        }
    }
    pub(crate) fn as_u64(&self) -> Option<u64> {
        match self {
            OJ::Number(n) => n.as_u64(),
            _ => None,
        }
    }

    /// As a `serde_json::Value` (for scalars and arrays of them; an object
    /// loses its order there).
    pub(crate) fn to_value(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            OJ::Null => J::Null,
            OJ::Bool(b) => J::Bool(*b),
            OJ::Number(n) => J::Number(n.clone()),
            OJ::String(s) => J::String(s.clone()),
            OJ::Array(a) => J::Array(a.iter().map(OJ::to_value).collect()),
            OJ::Object(m) => J::Object(m.0.iter().map(|(k, v)| (k.clone(), v.to_value())).collect()),
        }
    }

    /// From a `serde_json::Value` (keys in whatever order its map keeps).
    pub(crate) fn from_value(v: &serde_json::Value) -> OJ {
        use serde_json::Value as J;
        match v {
            J::Null => OJ::Null,
            J::Bool(b) => OJ::Bool(*b),
            J::Number(n) => OJ::Number(n.clone()),
            J::String(s) => OJ::String(s.clone()),
            J::Array(a) => OJ::Array(a.iter().map(OJ::from_value).collect()),
            J::Object(m) => OJ::Object(OMap(m.iter().map(|(k, v)| (k.clone(), OJ::from_value(v))).collect())),
        }
    }
}

impl<'de> Deserialize<'de> for OJ {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<OJ, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = OJ;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON")
            }
            fn visit_unit<E>(self) -> Result<OJ, E> {
                Ok(OJ::Null)
            }
            fn visit_none<E>(self) -> Result<OJ, E> {
                Ok(OJ::Null)
            }
            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<OJ, D::Error> {
                OJ::deserialize(d)
            }
            fn visit_bool<E>(self, b: bool) -> Result<OJ, E> {
                Ok(OJ::Bool(b))
            }
            fn visit_i64<E>(self, n: i64) -> Result<OJ, E> {
                Ok(OJ::Number(n.into()))
            }
            fn visit_u64<E>(self, n: u64) -> Result<OJ, E> {
                Ok(OJ::Number(n.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, n: f64) -> Result<OJ, E> {
                serde_json::Number::from_f64(n).map(OJ::Number).ok_or_else(|| E::custom("a non-finite number"))
            }
            fn visit_str<E>(self, s: &str) -> Result<OJ, E> {
                Ok(OJ::String(s.to_string()))
            }
            fn visit_string<E>(self, s: String) -> Result<OJ, E> {
                Ok(OJ::String(s))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<OJ, A::Error> {
                let mut v = Vec::new();
                while let Some(x) = a.next_element()? {
                    v.push(x);
                }
                Ok(OJ::Array(v))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<OJ, A::Error> {
                let mut m = OMap::default();
                while let Some((k, v)) = a.next_entry::<String, OJ>()? {
                    m.insert(k, v);
                }
                Ok(OJ::Object(m))
            }
        }
        d.deserialize_any(V)
    }
}

/// Write `b` as Extended JSON text (relaxed or canonical), documents in
/// their own key order.
pub(crate) fn write(b: &Bson, canonical: bool, out: &mut String) {
    match b {
        Bson::Document(d) => {
            out.push('{');
            for (i, (k, v)) in d.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::Value::String(k.to_string()).to_string());
                out.push(':');
                write(v, canonical, out);
            }
            out.push('}');
        }
        Bson::Array(a) => {
            out.push('[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(v, canonical, out);
            }
            out.push(']');
        }
        // Scalars and the `$`-wrappers (whose inner keys have no order
        // that matters): serde_json writes them.
        other => {
            let j = if canonical { other.to_canonical_json() } else { other.to_relaxed_json() };
            out.push_str(&j.to_string());
        }
    }
}
