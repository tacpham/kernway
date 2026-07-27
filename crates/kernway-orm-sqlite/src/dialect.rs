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

    fn auto_increment_keyword(&self) -> &'static str {
        "AUTOINCREMENT"
    }

    fn supports_returning(&self) -> bool {
        false
    }

    fn col_type_ddl(&self, col_type: &ColumnType) -> &'static str {
        match col_type {
            ColumnType::BigInt | ColumnType::Integer | ColumnType::Boolean => "INTEGER",
            ColumnType::Float => "REAL",
            _ => "TEXT",
        }
    }
}
