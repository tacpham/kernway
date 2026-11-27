//! SQL Server's T-SQL dialect.

use kernway_orm_core::{
    dialect::SqlDialect,
    entity::{ColumnDef, ColumnType},
};

/// SQL Server (T-SQL).
///
/// - Placeholders `@P1`, `@P2`, … (bound through `sp_executesql`)
/// - Identifiers in brackets: `[name]`
/// - Generated keys: `IDENTITY(1,1)`
/// - Row windows: `OFFSET m ROWS FETCH NEXT n ROWS ONLY` (with
///   `ORDER BY (SELECT NULL)` when the query has no order)
/// - Row locks: the `WITH (UPDLOCK, ROWLOCK)` table hint
/// - `CREATE TABLE` guarded by `IF OBJECT_ID(…) IS NULL`
pub struct SqlServerDialect;

impl SqlDialect for SqlServerDialect {
    fn name(&self) -> &'static str {
        "sqlserver"
    }

    fn placeholder(&self, index: usize) -> String {
        format!("@P{index}")
    }

    fn auto_increment_keyword(&self) -> &'static str {
        "IDENTITY(1,1)"
    }

    fn supports_returning(&self) -> bool {
        false
    }

    fn quote_identifier(&self, name: &str) -> String {
        format!("[{}]", name.replace(']', "]]"))
    }

    fn col_type_ddl(&self, col_type: &ColumnType) -> &'static str {
        match col_type {
            ColumnType::Integer => "INT",
            ColumnType::BigInt => "BIGINT",
            ColumnType::Boolean => "BIT",
            ColumnType::Float => "FLOAT",
            ColumnType::Timestamp => "DATETIME2",
            ColumnType::Binary => "VARBINARY(MAX)",
            ColumnType::Text | ColumnType::Json | ColumnType::Unknown => "NVARCHAR(MAX)",
        }
    }

    /// Keys and unique columns cannot be `(MAX)` types: text ones are
    /// `NVARCHAR(450)` (the 900-byte index key limit).
    fn column_ddl(&self, col: &ColumnDef) -> String {
        let text = matches!(col.col_type, ColumnType::Text | ColumnType::Unknown);
        if text && (col.primary_key || col.unique) {
            "NVARCHAR(450)".to_string()
        } else {
            self.col_type_ddl(&col.col_type).to_string()
        }
    }

    fn lock_table_hint(&self) -> &'static str {
        "WITH (UPDLOCK, ROWLOCK)"
    }

    fn limit_offset(&self, limit: Option<u64>, offset: u64) -> String {
        self.window(limit, offset, true)
    }

    fn window(&self, limit: Option<u64>, offset: u64, ordered: bool) -> String {
        if limit.is_none() && offset == 0 {
            return String::new();
        }
        let order = if ordered { "" } else { " ORDER BY (SELECT NULL)" };
        match limit {
            Some(n) => format!("{order} OFFSET {offset} ROWS FETCH NEXT {n} ROWS ONLY"),
            None => format!("{order} OFFSET {offset} ROWS"),
        }
    }

    fn create_table_sql(&self, table: &str, cols: &[ColumnDef]) -> String {
        let pk: Vec<&ColumnDef> = cols.iter().filter(|c| c.primary_key).collect();
        let composite = pk.len() > 1;
        let mut parts = Vec::new();
        for col in cols {
            let mut def = format!("{} {}", self.quote_identifier(col.name), self.column_ddl(col));
            if col.primary_key {
                if composite {
                    def.push_str(" NOT NULL");
                } else {
                    if col.auto {
                        def.push_str(" IDENTITY(1,1)");
                    }
                    def.push_str(" PRIMARY KEY");
                }
            } else {
                def.push_str(if col.nullable { " NULL" } else { " NOT NULL" });
                if col.unique {
                    def.push_str(" UNIQUE");
                }
            }
            parts.push(def);
        }
        if composite {
            let names: Vec<String> = pk.iter().map(|c| self.quote_identifier(c.name)).collect();
            parts.push(format!("PRIMARY KEY ({})", names.join(", ")));
        }
        format!(
            "IF OBJECT_ID(N'{}', N'U') IS NULL CREATE TABLE {} (\n  {}\n)",
            table.replace('\'', "''"),
            self.quote_identifier(table),
            parts.join(",\n  ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_and_windows() {
        let cols = [
            ColumnDef { name: "id", field: "id", col_type: ColumnType::BigInt, nullable: false, primary_key: true, unique: false, auto: true },
            ColumnDef { name: "email", field: "email", col_type: ColumnType::Text, nullable: false, primary_key: false, unique: true, auto: false },
            ColumnDef { name: "note", field: "note", col_type: ColumnType::Text, nullable: true, primary_key: false, unique: false, auto: false },
        ];
        let ddl = SqlServerDialect.create_table_sql("t'x", &cols);
        assert!(ddl.starts_with("IF OBJECT_ID(N't''x', N'U') IS NULL CREATE TABLE [t'x]"), "{ddl}");
        assert!(ddl.contains("[id] BIGINT IDENTITY(1,1) PRIMARY KEY"), "{ddl}");
        assert!(ddl.contains("[email] NVARCHAR(450) NOT NULL UNIQUE"), "{ddl}");
        assert!(ddl.contains("[note] NVARCHAR(MAX) NULL"), "{ddl}");
        assert_eq!(SqlServerDialect.quote_identifier("a]b"), "[a]]b]");
        assert_eq!(SqlServerDialect.window(Some(5), 0, false), " ORDER BY (SELECT NULL) OFFSET 0 ROWS FETCH NEXT 5 ROWS ONLY");
        assert_eq!(SqlServerDialect.window(None, 3, true), " OFFSET 3 ROWS");
        assert_eq!(SqlServerDialect.window(None, 0, false), "");
    }
}
