//! Row → entity decoding shared by the SQL drivers.
//!
//! A driver decodes each result row into [`Cell`]s — borrowed from its wire
//! buffer where it can — and [`cells_to_entity`] deserializes the entity
//! straight from them, typed by the entity's [`ColumnDef`]s: no intermediate
//! JSON value and no per-cell or per-key allocation; strings are copied once,
//! into the entity. Every driver that uses it maps column values to fields the
//! same way, whichever database the row came from.

use crate::entity::{ColumnDef, ColumnType};
use crate::error::OrmError;
use serde::de::value::BorrowedStrDeserializer;
use serde::de::{
    DeserializeOwned, DeserializeSeed, Deserializer, Error as _, IntoDeserializer, MapAccess,
    Visitor,
};
use serde::forward_to_deserialize_any;

/// One result-set cell, borrowed from the driver's wire buffer where possible —
/// decoding a row allocates nothing for its cells.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell<'a> {
    /// SQL `NULL`.
    Null,
    /// A value as raw bytes: a text-format value, or a binary string / decimal /
    /// blob / JSON value as sent.
    Bytes(&'a [u8]),
    /// A binary boolean (databases with a native boolean type).
    Bool(bool),
    /// A binary signed integer.
    Int(i64),
    /// A binary unsigned integer.
    UInt(u64),
    /// A binary single-precision float.
    F32(f32),
    /// A binary double-precision float.
    F64(f64),
    /// A value the driver had to format itself (e.g. a binary date / time).
    Owned(String),
}

impl Cell<'_> {
    /// The cell as text, as a text-format result would carry it (`None` = NULL).
    pub fn to_text(&self) -> Option<String> {
        match self {
            Cell::Null => None,
            Cell::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
            Cell::Bool(b) => Some(if *b { "1" } else { "0" }.to_string()),
            Cell::Int(i) => Some(i.to_string()),
            Cell::UInt(u) => Some(u.to_string()),
            Cell::F32(f) => Some(f.to_string()),
            Cell::F64(f) => Some(f.to_string()),
            Cell::Owned(s) => Some(s.clone()),
        }
    }
}

/// Re-type an **empty** cell vector to a new lifetime, keeping its allocation
/// (an in-place collect: same layout, no element is ever mapped). Lets a driver
/// reuse one scratch vector for every row although each row borrows a
/// different part of its buffer.
pub fn recycle<'b>(mut cells: Vec<Cell<'_>>) -> Vec<Cell<'b>> {
    cells.clear();
    cells.into_iter().map(|_| unreachable!("the vector is empty")).collect()
}

/// Map a row of text cells (values in `T::columns()` order) into an entity.
/// A thin wrapper over [`cells_to_entity`], kept for callers holding owned rows.
pub fn row_to_entity<T: DeserializeOwned>(row: &[Option<String>], cols: &[ColumnDef]) -> Result<T, OrmError> {
    let cells: Vec<Cell<'_>> = row
        .iter()
        .map(|c| c.as_ref().map_or(Cell::Null, |s| Cell::Bytes(s.as_bytes())))
        .collect();
    cells_to_entity(&cells, cols)
}

/// Deserialize an entity straight from a row's cells (values in `T::columns()`
/// order), guided by each column's declared type — no intermediate JSON value,
/// no per-cell or per-key allocation; strings are copied once, into the entity.
///
/// The mapping: integer / float columns
/// parse text, a boolean column is true for `1` / `true`, a JSON column parses
/// its document (falling back to the raw string), and text / timestamp columns
/// are strings. A missing trailing cell reads as `NULL`.
pub fn cells_to_entity<T: DeserializeOwned>(cells: &[Cell<'_>], cols: &[ColumnDef]) -> Result<T, OrmError> {
    T::deserialize(RowDe { cells, cols }).map_err(|e| OrmError::TypeConversion(e.to_string()))
}

type DeError = serde::de::value::Error;

/// A row as a map `field name → cell`.
struct RowDe<'r, 'a> {
    cells: &'r [Cell<'a>],
    cols: &'r [ColumnDef],
}

impl<'de> Deserializer<'de> for RowDe<'_, '_> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_map(RowAccess { cells: self.cells, cols: self.cols, next: 0 })
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

struct RowAccess<'r, 'a> {
    cells: &'r [Cell<'a>],
    cols: &'r [ColumnDef],
    next: usize,
}

impl<'de> MapAccess<'de> for RowAccess<'_, '_> {
    type Error = DeError;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, DeError> {
        match self.cols.get(self.next) {
            Some(col) => seed
                .deserialize(BorrowedStrDeserializer::<DeError>::new(col.field))
                .map(Some),
            None => Ok(None),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, DeError> {
        let col = &self.cols[self.next];
        let cell = self.cells.get(self.next).unwrap_or(&Cell::Null);
        self.next += 1;
        seed.deserialize(CellDe { cell, col })
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.cols.len() - self.next)
    }
}

/// One cell, typed by its column.
struct CellDe<'r, 'a> {
    cell: &'r Cell<'a>,
    col: &'r ColumnDef,
}

impl CellDe<'_, '_> {
    /// The cell as text, borrowed when it already is UTF-8 text.
    fn text(&self) -> Option<std::borrow::Cow<'_, str>> {
        match self.cell {
            Cell::Null => None,
            Cell::Bytes(b) => Some(String::from_utf8_lossy(b)),
            Cell::Owned(s) => Some(std::borrow::Cow::Borrowed(s.as_str())),
            other => other.to_text().map(std::borrow::Cow::Owned),
        }
    }
}

/// Hand `s` to the visitor, owned or borrowed as it is.
fn visit_text<'de, V: Visitor<'de>>(s: std::borrow::Cow<'_, str>, visitor: V) -> Result<V::Value, DeError> {
    match s {
        std::borrow::Cow::Borrowed(b) => visitor.visit_str(b),
        std::borrow::Cow::Owned(o) => visitor.visit_string(o),
    }
}

impl<'de> Deserializer<'de> for CellDe<'_, '_> {
    type Error = DeError;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        if matches!(self.cell, Cell::Null) {
            return visitor.visit_unit();
        }
        match self.col.col_type {
            ColumnType::Integer | ColumnType::BigInt => match self.cell {
                Cell::Int(i) => visitor.visit_i64(*i),
                Cell::Bool(b) => visitor.visit_i64(i64::from(*b)),
                Cell::UInt(u) => visitor.visit_u64(*u),
                _ => {
                    let s = self.text().unwrap_or_default();
                    if let Ok(i) = s.parse::<i64>() {
                        visitor.visit_i64(i)
                    } else if let Ok(u) = s.parse::<u64>() {
                        visitor.visit_u64(u)
                    } else {
                        visit_text(s, visitor)
                    }
                }
            },
            ColumnType::Float => match self.cell {
                Cell::F64(f) => visitor.visit_f64(*f),
                Cell::F32(f) => visitor.visit_f64(f64::from(*f)),
                Cell::Int(i) => visitor.visit_f64(*i as f64),
                Cell::UInt(u) => visitor.visit_f64(*u as f64),
                _ => {
                    let s = self.text().unwrap_or_default();
                    match s.parse::<f64>() {
                        Ok(f) => visitor.visit_f64(f),
                        Err(_) => visit_text(s, visitor),
                    }
                }
            },
            ColumnType::Boolean => match self.cell {
                Cell::Bool(b) => visitor.visit_bool(*b),
                Cell::Int(i) => visitor.visit_bool(*i == 1),
                Cell::UInt(u) => visitor.visit_bool(*u == 1),
                _ => {
                    let s = self.text().unwrap_or_default();
                    visitor.visit_bool(s == "1" || s.eq_ignore_ascii_case("true"))
                }
            },
            ColumnType::Json => {
                let s = self.text().unwrap_or_default();
                match serde_json::from_str::<serde_json::Value>(&s) {
                    Ok(v) => v.deserialize_any(visitor).map_err(DeError::custom),
                    Err(_) => visit_text(s, visitor),
                }
            }
            ColumnType::Text | ColumnType::Timestamp | ColumnType::Unknown => {
                visit_text(self.text().unwrap_or_default(), visitor)
            }
        }
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        if matches!(self.cell, Cell::Null) {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        let s = self.text().unwrap_or_default();
        if self.col.col_type == ColumnType::Json {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
                return v
                    .deserialize_enum(name, variants, visitor)
                    .map_err(DeError::custom);
            }
        }
        // A unit variant stored as its name, as serde_json reads a string.
        visitor.visit_enum(s.as_ref().into_deserializer())
    }

    forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf unit unit_struct seq tuple tuple_struct map struct
        identifier ignored_any
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn map_serde(e: serde_json::Error) -> OrmError {
        OrmError::TypeConversion(e.to_string())
    }

    // --- cells_to_entity must map exactly like the previous JSON-based path ---

    /// The previous mapping, verbatim: cell text → serde_json::Value by column
    /// type → `serde_json::from_value`. The oracle the new deserializer is
    /// checked against.
    fn old_text_to_json(value: Option<&String>, col: &ColumnDef) -> serde_json::Value {
        let Some(s) = value else {
            return serde_json::Value::Null;
        };
        match col.col_type {
            ColumnType::Integer | ColumnType::BigInt => s
                .parse::<i64>()
                .map(|n| serde_json::json!(n))
                .or_else(|_| s.parse::<u64>().map(|n| serde_json::json!(n)))
                .unwrap_or_else(|_| serde_json::Value::String(s.clone())),
            ColumnType::Float => s
                .parse::<f64>()
                .map(|f| serde_json::json!(f))
                .unwrap_or_else(|_| serde_json::Value::String(s.clone())),
            ColumnType::Boolean => serde_json::Value::Bool(s == "1" || s.eq_ignore_ascii_case("true")),
            ColumnType::Json => serde_json::from_str(s).unwrap_or_else(|_| serde_json::Value::String(s.clone())),
            ColumnType::Text | ColumnType::Timestamp | ColumnType::Unknown => {
                serde_json::Value::String(s.clone())
            }
        }
    }

    fn old_row_to_entity<T: DeserializeOwned>(row: &[Option<String>], cols: &[ColumnDef]) -> Result<T, OrmError> {
        let mut map = serde_json::Map::new();
        for (i, col) in cols.iter().enumerate() {
            map.insert(col.field.to_string(), old_text_to_json(row.get(i).and_then(|c| c.as_ref()), col));
        }
        serde_json::from_value(serde_json::Value::Object(map)).map_err(map_serde)
    }

    #[derive(Debug, PartialEq, serde::Deserialize)]
    enum Kind {
        Basic,
        Premium,
    }

    #[derive(Debug, PartialEq, serde::Deserialize)]
    struct Wide {
        id: u64,
        name: String,
        active: bool,
        score: f64,
        note: Option<String>,
        qty: Option<i64>,
        kind: Kind,
        meta: serde_json::Value,
        at: String,
        small: i32,
    }

    fn col(field: &'static str, col_type: ColumnType) -> ColumnDef {
        ColumnDef { name: field, field, col_type, nullable: true, primary_key: false, unique: false, auto: false }
    }

    fn wide_cols() -> Vec<ColumnDef> {
        vec![
            col("id", ColumnType::BigInt),
            col("name", ColumnType::Text),
            col("active", ColumnType::Boolean),
            col("score", ColumnType::Float),
            col("note", ColumnType::Text),
            col("qty", ColumnType::BigInt),
            col("kind", ColumnType::Unknown),
            col("meta", ColumnType::Json),
            col("at", ColumnType::Timestamp),
            col("small", ColumnType::Integer),
        ]
    }

    fn row(cells: &[Option<&str>]) -> Vec<Option<String>> {
        cells.iter().map(|c| c.map(str::to_string)).collect()
    }

    #[test]
    fn the_direct_deserializer_maps_exactly_like_the_old_json_path() {
        let cols = wide_cols();
        let cases: Vec<Vec<Option<String>>> = vec![
            row(&[Some("1"), Some("alpha"), Some("1"), Some("1.5"), Some("hi"), Some("-3"), Some("Basic"),
                  Some(r#"{"a":[1,2]}"#), Some("2026-11-26 10:00:00"), Some("7")]),
            row(&[Some("18446744073709551615"), Some(""), Some("TRUE"), Some("-0.25"), None, None,
                  Some("Premium"), Some("not json"), Some("2026-01-01"), Some("-2147483648")]),
            row(&[Some("0"), Some("ünïcode ✓"), Some("2"), Some("10"), Some(""), Some("0"), Some("Basic"),
                  Some("null"), Some(""), Some("0")]),
            row(&[Some("5"), Some("x"), Some("false"), Some("3"), None, Some("9223372036854775807"),
                  Some("Basic"), Some("[true]"), Some("t"), Some("1")]),
            // Errors must agree too: a bad enum name, an int overflowing i32,
            // NULL in a non-Option field, and a non-numeric id.
            row(&[Some("1"), Some("a"), Some("1"), Some("1"), None, None, Some("Gold"), Some("1"), Some("t"), Some("1")]),
            row(&[Some("1"), Some("a"), Some("1"), Some("1"), None, None, Some("Basic"), Some("1"), Some("t"), Some("2147483648")]),
            row(&[Some("1"), None, Some("1"), Some("1"), None, None, Some("Basic"), Some("1"), Some("t"), Some("1")]),
            row(&[Some("x"), Some("a"), Some("1"), Some("1"), None, None, Some("Basic"), Some("1"), Some("t"), Some("1")]),
            // A missing trailing cell reads as NULL (meta → Value::Null; `at` → error).
            row(&[Some("1"), Some("a"), Some("1"), Some("1"), None, None, Some("Basic")]),
        ];
        for (n, r) in cases.iter().enumerate() {
            let old: Result<Wide, _> = old_row_to_entity(r, &cols);
            let new: Result<Wide, _> = row_to_entity(r, &cols);
            match (&old, &new) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "case {n}"),
                (Err(_), Err(_)) => {}
                _ => panic!("case {n}: old {old:?} vs new {new:?}"),
            }
        }
    }

    #[test]
    fn binary_numbers_map_like_their_text_form() {
        // The binary protocol hands over numbers; they must land in the entity
        // exactly as the same values sent as text would.
        let cols = wide_cols();
        let text = row(&[Some("42"), Some("7"), Some("1"), Some("2.5"), None, Some("-1"), Some("Basic"),
                         Some("{}"), Some("2026-11-26 10:00:00"), Some("3")]);
        let at = "2026-11-26 10:00:00".to_string();
        let binary = [
            Cell::UInt(42), Cell::Int(7), Cell::Int(1), Cell::F64(2.5), Cell::Null, Cell::Int(-1),
            Cell::Bytes(b"Basic"), Cell::Bytes(b"{}"), Cell::Owned(at), Cell::Int(3),
        ];
        let from_text: Wide = row_to_entity(&text, &cols).unwrap();
        let from_binary: Wide = cells_to_entity(&binary, &cols).unwrap();
        assert_eq!(from_text, from_binary);
        // A boolean column reads binary 1 as true and anything else as false.
        assert!(matches!(cells_to_entity::<Wide>(&[Cell::UInt(1), Cell::Bytes(b"n"), Cell::Int(0),
            Cell::F64(0.0), Cell::Null, Cell::Null, Cell::Bytes(b"Basic"), Cell::Bytes(b"1"),
            Cell::Bytes(b"t"), Cell::Int(0)], &cols), Ok(Wide { active: false, .. })));
    }
    #[test]
    fn recycle_keeps_the_allocation_across_lifetimes() {
        let packet = [1u8, b'x'];
        let mut cells: Vec<Cell<'_>> = Vec::with_capacity(8);
        cells.push(Cell::Bytes(&packet[1..]));
        let ptr = cells.as_ptr() as usize;
        let back: Vec<Cell<'static>> = recycle(cells);
        assert!(back.is_empty());
        assert_eq!(back.capacity(), 8);
        assert_eq!(back.as_ptr() as usize, ptr, "same buffer, no new allocation");
    }

    #[test]
    fn a_native_boolean_cell_maps_to_bool_and_int_columns() {
        #[derive(Debug, PartialEq, serde::Deserialize)]
        struct Flags {
            on: bool,
            n: i64,
        }
        let cols = [col("on", ColumnType::Boolean), col("n", ColumnType::BigInt)];
        let f: Flags = cells_to_entity(&[Cell::Bool(true), Cell::Bool(true)], &cols).unwrap();
        assert_eq!(f, Flags { on: true, n: 1 });
        assert_eq!(Cell::Bool(false).to_text().as_deref(), Some("0"));
    }
}
