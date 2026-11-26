//! SQLite SQL dialect.

use kernway_orm_core::{dialect::SqlDialect, entity::ColumnType};

/// SQLite SQL dialect adapter.
pub struct SqliteDialect;

impl SqlDialect for SqliteDialect {
    fn name(&self) -> &'static str {
        "sqlite"
    }

    fn placeholder(&self, _index: usize) -> String {
        "?".to_string()
    }

    /// With `INTEGER PRIMARY KEY` (the rowid), `AUTOINCREMENT` also never
    /// reuses the key of a deleted row.
    fn auto_increment_keyword(&self) -> &'static str {
        "AUTOINCREMENT"
    }

    fn supports_returning(&self) -> bool {
        false
    }

    /// Declared types are what the driver reads values back by: `BOOLEAN`,
    /// `TIMESTAMP` and `JSON` keep their meaning although SQLite stores them
    /// as integers / text.
    fn col_type_ddl(&self, col_type: &ColumnType) -> &'static str {
        match col_type {
            ColumnType::BigInt | ColumnType::Integer => "INTEGER",
            ColumnType::Boolean => "BOOLEAN",
            ColumnType::Float => "REAL",
            ColumnType::Binary => "BLOB",
            ColumnType::Timestamp => "TIMESTAMP",
            ColumnType::Json => "JSON",
            ColumnType::Text | ColumnType::Unknown => "TEXT",
        }
    }

    /// SQLite locks the whole database, not rows: `FOR UPDATE` does not exist.
    /// Transactions start `BEGIN IMMEDIATE` instead, which serialises writers.
    fn lock_for_update_suffix(&self) -> &'static str {
        ""
    }

    /// SQLite has no `OFFSET` without `LIMIT`; `LIMIT -1` means no limit.
    fn limit_offset(&self, limit: Option<u64>, offset: u64) -> String {
        match (limit, offset) {
            (None, 0) => String::new(),
            (Some(n), 0) => format!(" LIMIT {n}"),
            (Some(n), m) => format!(" LIMIT {n} OFFSET {m}"),
            (None, m) => format!(" LIMIT -1 OFFSET {m}"),
        }
    }
}
