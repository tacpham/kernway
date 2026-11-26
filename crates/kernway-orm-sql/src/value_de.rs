//! Deserialize result rows of typed [`Value`]s into any `serde` type — a
//! struct matched to columns by name, a tuple by position — without a
//! JSON detour. Each value converts to what the target field asks for: a
//! `DECIMAL` sum into an `i64` or an `f64` or a `String`, a `0`/`1` into a
//! `bool`, and so on, so one struct reads the same from every database.

use crate::raw::Value;
use serde::de::{
    self, value::SeqDeserializer, DeserializeSeed, Deserializer, Error as _, IntoDeserializer, MapAccess, SeqAccess,
    Visitor,
};
use serde::forward_to_deserialize_any;

pub(crate) type DeError = de::value::Error;

/// One row: column names and their values.
pub(crate) struct RowDe<'a> {
    pub(crate) columns: &'a [String],
    pub(crate) row: &'a [Value],
}

impl<'de> Deserializer<'de> for RowDe<'_> {
    type Error = DeError;

    /// A map `column → value`.
    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_map(ColumnsAccess { columns: self.columns, row: self.row, next: 0 })
    }

    /// Struct fields matched to columns by name, case-insensitively (Oracle
    /// reports unquoted names in upper case); a field with no column is NULL.
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        let at = fields
            .iter()
            .map(|f| self.columns.iter().position(|c| c.eq_ignore_ascii_case(f)))
            .collect();
        visitor.visit_map(FieldsAccess { fields, at, row: self.row, next: 0 })
    }

    /// A tuple (or sequence) takes the values by position.
    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_seq(ValuesAccess { row: self.row.iter() })
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _len: usize, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _len: usize,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_seq(visitor)
    }

    /// A single-column row also reads as that value (`Vec<i64>` of ids).
    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.single()?.deserialize_i64(visitor)
    }
    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.single()?.deserialize_u64(visitor)
    }
    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.single()?.deserialize_i32(visitor)
    }
    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.single()?.deserialize_f64(visitor)
    }
    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.single()?.deserialize_string(visitor)
    }
    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.single()?.deserialize_str(visitor)
    }
    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.single()?.deserialize_bool(visitor)
    }

    forward_to_deserialize_any! {
        i8 i16 i128 u8 u16 u32 u128 f32 char bytes byte_buf option unit unit_struct
        newtype_struct map enum identifier ignored_any
    }
}

impl<'a> RowDe<'a> {
    fn single(&self) -> Result<ValueDe<'a>, DeError> {
        match self.row {
            [v] => Ok(ValueDe(v)),
            _ => Err(DeError::custom(format!("expected a single column, the row has {}", self.row.len()))),
        }
    }
}

struct ColumnsAccess<'a> {
    columns: &'a [String],
    row: &'a [Value],
    next: usize,
}

impl<'de> MapAccess<'de> for ColumnsAccess<'_> {
    type Error = DeError;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, DeError> {
        match self.columns.get(self.next) {
            Some(c) => seed.deserialize(c.as_str().into_deserializer()).map(Some),
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, DeError> {
        let v = self.row.get(self.next).unwrap_or(&Value::Null);
        self.next += 1;
        seed.deserialize(ValueDe(v))
    }
}

struct FieldsAccess<'a> {
    fields: &'static [&'static str],
    at: Vec<Option<usize>>,
    row: &'a [Value],
    next: usize,
}

impl<'de> MapAccess<'de> for FieldsAccess<'_> {
    type Error = DeError;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, DeError> {
        match self.fields.get(self.next) {
            Some(f) => seed.deserialize((*f).into_deserializer()).map(Some),
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, DeError> {
        let v = self.at[self.next].and_then(|i| self.row.get(i)).unwrap_or(&Value::Null);
        self.next += 1;
        seed.deserialize(ValueDe(v))
    }
}

struct ValuesAccess<'a> {
    row: std::slice::Iter<'a, Value>,
}

impl<'de> SeqAccess<'de> for ValuesAccess<'_> {
    type Error = DeError;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>, DeError> {
        match self.row.next() {
            Some(v) => seed.deserialize(ValueDe(v)).map(Some),
            None => Ok(None),
        }
    }
}

/// One value, converted to what the target asks for.
#[derive(Clone, Copy)]
pub(crate) struct ValueDe<'a>(pub(crate) &'a Value);

impl ValueDe<'_> {
    fn text(&self) -> Option<std::borrow::Cow<'_, str>> {
        use std::borrow::Cow;
        Some(match self.0 {
            Value::Text(s)
            | Value::Decimal(s)
            | Value::Json(s)
            | Value::Date(s)
            | Value::Time(s)
            | Value::DateTime(s)
            | Value::Interval(s) => Cow::Borrowed(s.as_str()),
            Value::Int(i) => Cow::Owned(i.to_string()),
            Value::UInt(u) => Cow::Owned(u.to_string()),
            Value::Float(f) => Cow::Owned(f.to_string()),
            Value::Bool(b) => Cow::Owned(b.to_string()),
            Value::Bytes(b) => String::from_utf8_lossy(b),
            Value::Null | Value::Rows(_) => return None,
        })
    }

    fn mismatch(&self, want: &str) -> DeError {
        DeError::custom(format!("cannot read {:?} as {want}", self.0))
    }

    fn int(&self) -> Result<i128, DeError> {
        match self.0 {
            Value::Int(i) => Ok(i128::from(*i)),
            Value::UInt(u) => Ok(i128::from(*u)),
            Value::Bool(b) => Ok(i128::from(*b)),
            Value::Float(f) if f.fract() == 0.0 && f.abs() < 1e38 => Ok(*f as i128),
            Value::Decimal(s) | Value::Text(s) => {
                let t = s.trim();
                t.parse::<i128>()
                    .ok()
                    // An exact decimal with a zero fraction ("12.000").
                    .or_else(|| t.split_once('.').filter(|(_, f)| f.bytes().all(|b| b == b'0')).and_then(|(i, _)| i.parse().ok()))
                    .ok_or_else(|| self.mismatch("an integer"))
            }
            _ => Err(self.mismatch("an integer")),
        }
    }

    fn float(&self) -> Result<f64, DeError> {
        match self.0 {
            Value::Float(f) => Ok(*f),
            Value::Int(i) => Ok(*i as f64),
            Value::UInt(u) => Ok(*u as f64),
            Value::Decimal(s) | Value::Text(s) => s.trim().parse().map_err(|_| self.mismatch("a number")),
            _ => Err(self.mismatch("a number")),
        }
    }

    fn json(&self) -> Option<serde_json::Value> {
        match self.0 {
            Value::Json(s) => serde_json::from_str(s).ok(),
            _ => None,
        }
    }
}

fn visit_text<'de, V: Visitor<'de>>(s: std::borrow::Cow<'_, str>, visitor: V) -> Result<V::Value, DeError> {
    match s {
        std::borrow::Cow::Borrowed(b) => visitor.visit_str(b),
        std::borrow::Cow::Owned(o) => visitor.visit_string(o),
    }
}

macro_rules! int_hint {
    ($($method:ident => $visit:ident as $t:ty),*) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
            if self.0.is_null() {
                return visitor.visit_unit();
            }
            let i = self.int()?;
            let v = <$t>::try_from(i).map_err(|_| DeError::custom(format!("{i} is out of range")))?;
            visitor.$visit(v)
        }
    )*};
}

impl<'de> Deserializer<'de> for ValueDe<'_> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.0 {
            Value::Null => visitor.visit_unit(),
            Value::Bool(b) => visitor.visit_bool(*b),
            Value::Int(i) => visitor.visit_i64(*i),
            Value::UInt(u) => visitor.visit_u64(*u),
            Value::Float(f) => visitor.visit_f64(*f),
            Value::Decimal(s) => match s.parse::<i64>() {
                Ok(i) => visitor.visit_i64(i),
                Err(_) => visitor.visit_str(s),
            },
            Value::Bytes(b) => visitor.visit_bytes(b),
            Value::Json(s) => match serde_json::from_str::<serde_json::Value>(s) {
                Ok(j) => j.deserialize_any(visitor).map_err(DeError::custom),
                Err(_) => visitor.visit_str(s),
            },
            Value::Text(s) | Value::Date(s) | Value::Time(s) | Value::DateTime(s) | Value::Interval(s) => {
                visitor.visit_str(s)
            }
            Value::Rows(_) => Err(self.mismatch("a scalar")),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.0 {
            Value::Null => visitor.visit_unit(),
            Value::Bool(b) => visitor.visit_bool(*b),
            Value::Text(s) | Value::Decimal(s) if s.eq_ignore_ascii_case("true") => visitor.visit_bool(true),
            Value::Text(s) | Value::Decimal(s) if s.eq_ignore_ascii_case("false") => visitor.visit_bool(false),
            _ => match self.int()? {
                0 => visitor.visit_bool(false),
                1 => visitor.visit_bool(true),
                _ => Err(self.mismatch("a boolean")),
            },
        }
    }

    int_hint!(
        deserialize_i8 => visit_i8 as i8,
        deserialize_i16 => visit_i16 as i16,
        deserialize_i32 => visit_i32 as i32,
        deserialize_i64 => visit_i64 as i64,
        deserialize_i128 => visit_i128 as i128,
        deserialize_u8 => visit_u8 as u8,
        deserialize_u16 => visit_u16 as u16,
        deserialize_u32 => visit_u32 as u32,
        deserialize_u64 => visit_u64 as u64,
        deserialize_u128 => visit_u128 as u128
    );

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_f64(visitor)
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        if self.0.is_null() {
            return visitor.visit_unit();
        }
        visitor.visit_f64(self.float()?)
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_str(visitor)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.text() {
            Some(s) => visit_text(s, visitor),
            None if self.0.is_null() => visitor.visit_unit(),
            None => Err(self.mismatch("text")),
        }
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.0 {
            Value::Null => visitor.visit_unit(),
            Value::Bytes(b) => visitor.visit_bytes(b),
            _ => match self.text() {
                Some(s) => visitor.visit_byte_buf(s.into_owned().into_bytes()),
                None => Err(self.mismatch("bytes")),
            },
        }
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_bytes(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        if self.0.is_null() {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    /// `Vec<u8>` from binary data; a JSON array from a JSON document.
    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.0 {
            Value::Bytes(b) => visitor.visit_seq(SeqDeserializer::<_, DeError>::new(b.iter().copied())),
            Value::Rows(rows) => visitor.visit_seq(SeqDeserializer::<_, DeError>::new(
                rows.rows.iter().map(|r| RowOwned { columns: rows.columns.clone(), row: r.clone() }),
            )),
            _ => match self.json() {
                Some(j) => j.deserialize_seq(visitor).map_err(DeError::custom),
                None => Err(self.mismatch("a sequence")),
            },
        }
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match self.json() {
            Some(j) => j.deserialize_map(visitor).map_err(DeError::custom),
            None => Err(self.mismatch("a map")),
        }
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        match self.json() {
            Some(j) => j.deserialize_struct(name, fields, visitor).map_err(DeError::custom),
            None => Err(self.mismatch("a struct")),
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(self, _name: &'static str, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        if let Some(j) = self.json() {
            return j.deserialize_enum(name, variants, visitor).map_err(DeError::custom);
        }
        match self.text() {
            Some(s) => visitor.visit_enum(s.into_owned().into_deserializer()),
            None => Err(self.mismatch("an enum variant")),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_unit()
    }

    forward_to_deserialize_any! { unit_struct tuple tuple_struct identifier ignored_any }
}

/// A nested result set's row (a REF CURSOR value read into `Vec<T>`).
struct RowOwned {
    columns: Vec<String>,
    row: Vec<Value>,
}

impl<'de> IntoDeserializer<'de, DeError> for RowOwned {
    type Deserializer = RowOwnedDe;
    fn into_deserializer(self) -> RowOwnedDe {
        RowOwnedDe(self)
    }
}

struct RowOwnedDe(RowOwned);

impl<'de> Deserializer<'de> for RowOwnedDe {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        RowDe { columns: &self.0.columns, row: &self.0.row }.deserialize_any(visitor)
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        RowDe { columns: &self.0.columns, row: &self.0.row }.deserialize_struct(name, fields, visitor)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        RowDe { columns: &self.0.columns, row: &self.0.row }.deserialize_seq(visitor)
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, DeError> {
        RowDe { columns: &self.0.columns, row: &self.0.row }.deserialize_tuple(len, visitor)
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
        option unit unit_struct newtype_struct tuple_struct map enum identifier ignored_any
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::Rows;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Total {
        active: bool,
        n: i64,
        total: f64,
        label: String,
        note: Option<String>,
    }

    fn rows() -> Rows {
        Rows {
            columns: vec!["ACTIVE".into(), "n".into(), "total".into(), "label".into(), "extra".into()],
            rows: vec![
                vec![Value::Int(1), Value::Decimal("10".into()), Value::Decimal("10.50".into()), Value::Int(7), Value::Null],
                vec![Value::Bool(false), Value::Int(3), Value::Float(2.0), Value::Text("x".into()), Value::Null],
            ],
        }
    }

    #[test]
    fn structs_match_columns_by_name_and_convert_per_field() {
        let got: Vec<Total> = rows().to_structs().unwrap();
        assert_eq!(
            got,
            [
                Total { active: true, n: 10, total: 10.5, label: "7".into(), note: None },
                Total { active: false, n: 3, total: 2.0, label: "x".into(), note: None },
            ]
        );
    }

    #[test]
    fn tuples_take_values_by_position_and_single_columns_read_directly() {
        let r = Rows {
            columns: vec!["a".into(), "b".into()],
            rows: vec![vec![Value::Decimal("12.000".into()), Value::Text("t".into())]],
        };
        let t: Vec<(i64, String)> = r.to_structs().unwrap();
        assert_eq!(t, [(12, "t".to_string())]);
        let one = Rows { columns: vec!["id".into()], rows: vec![vec![Value::UInt(5)], vec![Value::Int(6)]] };
        let ids: Vec<i64> = one.to_structs().unwrap();
        assert_eq!(ids, [5, 6]);
    }

    #[test]
    fn a_value_that_does_not_fit_is_an_error() {
        let r = Rows { columns: vec!["n".into()], rows: vec![vec![Value::Text("abc".into())]] };
        assert!(r.to_structs::<(i64,)>().is_err());
        let big = Rows { columns: vec!["n".into()], rows: vec![vec![Value::Int(300)]] };
        assert!(big.to_structs::<(u8,)>().is_err());
    }
}
