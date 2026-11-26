//! MySQL SQL dialect.

use kernway_orm_core::{dialect::SqlDialect, entity::{ColumnDef, ColumnType}};

/// MySQL SQL dialect adapter.
pub struct MySqlDialect;

impl SqlDialect for MySqlDialect {
    fn name(&self) -> &'static str {
        "mysql"
    }

    fn placeholder(&self, _index: usize) -> String {
        "?".to_string()
    }

    fn auto_increment_keyword(&self) -> &'static str {
        "AUTO_INCREMENT"
    }

    fn supports_returning(&self) -> bool {
        false
    }

    fn quote_identifier(&self, name: &str) -> String {
        format!("`{}`", name)
    }

    /// Text keys and unique columns become `VARCHAR(255)`: MySQL cannot index
    /// an unbounded `LONGTEXT` without a prefix length.
    fn column_ddl(&self, col: &ColumnDef) -> String {
        let text = matches!(col.col_type, ColumnType::Text | ColumnType::Unknown);
        if text && (col.primary_key || col.unique) {
            "VARCHAR(255)".to_string()
        } else {
            self.col_type_ddl(&col.col_type).to_string()
        }
    }

    fn col_type_ddl(&self, col_type: &ColumnType) -> &'static str {
        match col_type {
            ColumnType::Integer => "INT",
            ColumnType::BigInt => "BIGINT",
            ColumnType::Boolean => "TINYINT(1)",
            ColumnType::Float => "DOUBLE",
            ColumnType::Text | ColumnType::Unknown => "LONGTEXT",
            ColumnType::Timestamp => "DATETIME",
            ColumnType::Json => "JSON",
            ColumnType::Binary => "LONGBLOB",
        }
    }

    fn lock_for_update_suffix(&self) -> &'static str {
        "FOR UPDATE"
    }
}
