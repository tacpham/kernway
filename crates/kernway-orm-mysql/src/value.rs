//! Value mapping between MySQL's text protocol and entities.
//!
//! Writes bind parameters (or inline **escaped SQL literals** on the text path).
//! Reads deserialize the entity straight from the row's cells
//! ([`cells_to_entity`]): column types from the `#[entity]` metadata drive the
//! parsing so `"1"` becomes `1`/`true`/`1.0` as the field expects.

use crate::protocol::{escape_string, BindValue, Cell};
use kernway_orm_core::{
    entity::{ColumnDef, ColumnType, Entity},
    error::OrmError,
};
use serde::de::value::BorrowedStrDeserializer;
use serde::de::{
    DeserializeOwned, DeserializeSeed, Deserializer, Error as _, IntoDeserializer, MapAccess, Visitor,
};
use serde::{forward_to_deserialize_any, Serialize};

fn map_serde(e: serde_json::Error) -> OrmError {
    OrmError::TypeConversion(e.to_string())
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
/// The mapping is the one the driver has always used: integer / float columns
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

/// Render a JSON value as a MySQL SQL literal for the given column.
pub fn json_to_literal(v: &serde_json::Value, _col: &ColumnDef) -> String {
    match v {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => format!("'{}'", escape_string(s)),
        // Arrays/objects land in JSON columns — store their serialized form.
        other => format!("'{}'", escape_string(&serde_json::to_string(other).unwrap_or_default())),
    }
}

/// A single scalar value (id part, filter value) as a SQL literal.
pub fn scalar_to_literal(v: &serde_json::Value) -> Result<String, OrmError> {
    Ok(match v {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => format!("'{}'", escape_string(s)),
        other => {
            return Err(OrmError::TypeConversion(format!(
                "value must be a scalar (number/string/bool), got {other}"
            )))
        }
    })
}

/// A primary key's SQL literals — one for a scalar key, several for a composite
/// (tuple) key, in declaration order.
pub fn id_to_literals<Id: Serialize>(id: &Id) -> Result<Vec<String>, OrmError> {
    match serde_json::to_value(id).map_err(map_serde)? {
        serde_json::Value::Array(parts) => parts.iter().map(scalar_to_literal).collect(),
        scalar => Ok(vec![scalar_to_literal(&scalar)?]),
    }
}

// --- Prepared-statement binds (binary protocol) ----------------------------

/// Render a JSON value as a bound parameter for the given column.
pub fn json_to_bind(v: &serde_json::Value, col: &ColumnDef) -> BindValue {
    match v {
        serde_json::Value::Null => BindValue::Null,
        serde_json::Value::Bool(b) => BindValue::Int(*b as i64),
        serde_json::Value::Number(n) => {
            if col.col_type == ColumnType::Float {
                BindValue::Float(n.as_f64().unwrap_or(0.0))
            } else if let Some(i) = n.as_i64() {
                BindValue::Int(i)
            } else if let Some(u) = n.as_u64() {
                BindValue::UInt(u)
            } else {
                BindValue::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => BindValue::Str(s.clone()),
        other => BindValue::Str(serde_json::to_string(other).unwrap_or_default()),
    }
}

/// A single scalar (id part / filter value) as a bound parameter.
pub fn scalar_to_bind(v: &serde_json::Value) -> Result<BindValue, OrmError> {
    Ok(match v {
        serde_json::Value::Null => BindValue::Null,
        serde_json::Value::Bool(b) => BindValue::Int(*b as i64),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                BindValue::Int(i)
            } else if let Some(u) = n.as_u64() {
                BindValue::UInt(u)
            } else {
                BindValue::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => BindValue::Str(s.clone()),
        other => {
            return Err(OrmError::TypeConversion(format!(
                "value must be a scalar (number/string/bool), got {other}"
            )))
        }
    })
}

/// A primary key's bound parameters — one for a scalar key, several for a
/// composite key.
pub fn id_to_binds<Id: Serialize>(id: &Id) -> Result<Vec<BindValue>, OrmError> {
    match serde_json::to_value(id).map_err(map_serde)? {
        serde_json::Value::Array(parts) => parts.iter().map(scalar_to_bind).collect(),
        scalar => Ok(vec![scalar_to_bind(&scalar)?]),
    }
}

/// `col = ? [AND col2 = ? …]` over the PK columns, with `?` placeholders.
pub fn pk_where_ph<T: Entity>() -> String {
    pk_cols::<T>()
        .iter()
        .map(|c| format!("`{}` = ?", c))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Resolve a `field` name (Rust field or db column) to its [`ColumnDef`].
pub fn column_for_field<T: Entity>(field: &str) -> Option<&'static ColumnDef> {
    T::columns().iter().find(|c| c.field == field || c.name == field)
}

/// Resolve a `field` name to its database column name, or `None` if unknown.
pub fn resolve_column_name<T: Entity>(field: &str) -> Option<&'static str> {
    column_for_field::<T>(field).map(|c| c.name)
}

/// Escape a `LIKE` pattern for a single-quoted literal, leaving `%` and `_`
/// intact as wildcards.
pub fn escape_like(pattern: &str) -> String {
    escape_string(pattern)
}

/// Convert a stringly-typed filter value into a SQL literal, typed by the field.
pub fn filter_literal_for_field<T: Entity>(field: &str, value: &str) -> String {
    let Some(col) = column_for_field::<T>(field) else {
        return format!("'{}'", escape_string(value));
    };
    match col.col_type {
        ColumnType::Boolean => match value {
            "true" | "1" => "1".to_string(),
            "false" | "0" => "0".to_string(),
            _ => format!("'{}'", escape_string(value)),
        },
        ColumnType::Integer | ColumnType::BigInt => value
            .parse::<i64>()
            .map(|n| n.to_string())
            .or_else(|_| value.parse::<u64>().map(|n| n.to_string()))
            .unwrap_or_else(|_| format!("'{}'", escape_string(value))),
        ColumnType::Float => value
            .parse::<f64>()
            .map(|f| f.to_string())
            .unwrap_or_else(|_| format!("'{}'", escape_string(value))),
        _ => format!("'{}'", escape_string(value)),
    }
}

/// The primary-key column names — one for a scalar key, several for a composite.
pub fn pk_cols<T: Entity>() -> Vec<&'static str> {
    let cols: Vec<&'static str> = T::columns()
        .iter()
        .filter(|c| c.primary_key)
        .map(|c| c.name)
        .collect();
    if cols.is_empty() {
        vec!["id"]
    } else {
        cols
    }
}

/// `col = lit [AND col2 = lit2 …]` over the PK columns.
pub fn pk_where<T: Entity>(id_literals: &[String]) -> String {
    pk_cols::<T>()
        .iter()
        .zip(id_literals.iter())
        .map(|(c, lit)| format!("`{}` = {}", c, lit))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Whether the entity's single primary key is auto-increment.
pub fn is_auto_pk<T: Entity>() -> bool {
    T::columns().iter().any(|c| c.primary_key && c.auto)
}

/// Whether this entity is a fresh auto-increment row (its key is still 0).
pub fn is_new_auto_entity<T>(entity: &T) -> Result<bool, OrmError>
where
    T: Entity + Serialize,
    T::Id: Serialize,
{
    if !is_auto_pk::<T>() {
        return Ok(false);
    }
    let v = serde_json::to_value(entity.id()).map_err(map_serde)?;
    Ok(matches!(
        v,
        serde_json::Value::Number(ref n) if n.as_u64() == Some(0) || n.as_i64() == Some(0)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MySqlDialect;
    use kernway_orm_core::SqlDialect;
    use kernway_orm_macro::entity;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[entity(table = "items")]
    struct Item {
        #[id(strategy = "auto")]
        id: u64,
        name: String,
        value: i32,
    }

    #[test]
    fn row_roundtrips_into_entity() {
        let row = [Some("7".to_string()), Some("alpha".to_string()), Some("10".to_string())];
        let item: Item = row_to_entity(&row, Item::columns()).unwrap();
        assert_eq!(item, Item { id: 7, name: "alpha".into(), value: 10 });
    }

    #[test]
    fn literals_are_typed_and_escaped() {
        // String values are quoted + escaped; numbers are bare.
        assert_eq!(filter_literal_for_field::<Item>("name", "a'b"), "'a\\'b'");
        assert_eq!(filter_literal_for_field::<Item>("value", "42"), "42");
        // A non-numeric value for a numeric column degrades to a safe literal.
        assert_eq!(filter_literal_for_field::<Item>("value", "x"), "'x'");
    }

    #[test]
    fn pk_where_uses_backticks() {
        let lits = id_to_literals(&7u64).unwrap();
        assert_eq!(pk_where::<Item>(&lits), "`id` = 7");
    }

    #[test]
    fn create_table_is_mysql_flavoured() {
        let ddl = MySqlDialect.create_table_sql(Item::table_name(), Item::columns());
        assert!(ddl.contains("`items`"), "{ddl}");
        assert!(ddl.contains("AUTO_INCREMENT"), "{ddl}");
        assert!(ddl.contains("PRIMARY KEY"), "{ddl}");
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
}
