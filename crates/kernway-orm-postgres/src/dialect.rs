//! PostgreSQL SQL dialect.

use kernway_orm_core::{dialect::SqlDialect, entity::ColumnType};

/// PostgreSQL SQL dialect adapter.
///
/// Key differences from SQLite:
/// - Placeholders are `$1`, `$2`, … (positional)
/// - Auto-increment uses `SERIAL` or `GENERATED ALWAYS AS IDENTITY` (not a keyword after the type)
/// - `INSERT … RETURNING` is supported
/// - Identifiers are quoted with double quotes (same as standard SQL)
pub struct PostgresDialect;

impl SqlDialect for PostgresDialect {
    fn name(&self) -> &'static str {
        "postgres"
    }

    fn placeholder(&self, index: usize) -> String {
        format!("${}", index)
    }

    fn auto_increment_keyword(&self) -> &'static str {
        ""
    }

    fn supports_returning(&self) -> bool {
        true
    }

    fn col_type_ddl(&self, col_type: &ColumnType) -> &'static str {
        match col_type {
            ColumnType::Integer => "INTEGER",
            ColumnType::BigInt => "BIGINT",
            ColumnType::Boolean => "BOOLEAN",
            ColumnType::Float => "DOUBLE PRECISION",
            ColumnType::Text | ColumnType::Unknown => "TEXT",
            ColumnType::Timestamp => "TIMESTAMPTZ",
            ColumnType::Json => "JSONB",
        }
    }
}
