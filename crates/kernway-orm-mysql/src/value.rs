//! Value mapping between MySQL's text protocol and entities.
//!
//! The text protocol has no bound parameters, so writes inline **escaped SQL
//! literals** and reads parse text column values back into JSON (and then into
//! the entity via `serde`). Column types from the `#[entity]` metadata drive the
//! parsing so `"1"` becomes `1`/`true`/`1.0` as the field expects.

use crate::protocol::{escape_string, BindValue};
use kernway_orm_core::{
    entity::{ColumnDef, ColumnType, Entity},
    error::OrmError,
};
use serde::{de::DeserializeOwned, Serialize};

fn map_serde(e: serde_json::Error) -> OrmError {
    OrmError::TypeConversion(e.to_string())
}

/// Parse one text-protocol cell into JSON, guided by the column's declared type.
fn text_to_json(value: Option<&String>, col: &ColumnDef) -> serde_json::Value {
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

/// Map a text-protocol row (values in `T::columns()` order) into an entity.
pub fn row_to_entity<T: DeserializeOwned>(row: &[Option<String>], cols: &[ColumnDef]) -> Result<T, OrmError> {
    let mut map = serde_json::Map::with_capacity(cols.len());
    for (i, col) in cols.iter().enumerate() {
        let cell = row.get(i).and_then(|c| c.as_ref());
        map.insert(col.field.to_string(), text_to_json(cell, col));
    }
    serde_json::from_value(serde_json::Value::Object(map)).map_err(map_serde)
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
}
