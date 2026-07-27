//! MySQL SQL dialect.

use kernway_orm_core::{dialect::SqlDialect, entity::ColumnType};

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

    fn col_type_ddl(&self, col_type: &ColumnType) -> &'static str {
        match col_type {
            ColumnType::Integer => "INT",
            ColumnType::BigInt => "BIGINT",
            ColumnType::Boolean => "TINYINT(1)",
            ColumnType::Float => "DOUBLE",
            ColumnType::Text | ColumnType::Unknown => "TEXT",
            ColumnType::Timestamp => "DATETIME",
            ColumnType::Json => "JSON",
        }
    }
}
