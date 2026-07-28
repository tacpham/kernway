//! `SqlDialect` — the SQL-syntax adapter for relational backends.
//!
//! Every SQL database speaks a slightly different dialect:
//! - Placeholders: SQLite/MySQL use `?`; PostgreSQL uses `$1`, `$2`, …
//! - Auto-increment: SQLite uses `AUTOINCREMENT`; MySQL `AUTO_INCREMENT`; PostgreSQL `SERIAL` / `GENERATED ALWAYS AS IDENTITY`
//! - Identifier quoting: standard SQL uses `"name"`; MySQL uses backticks
//! - `INSERT … RETURNING`: supported in PostgreSQL and SQLite ≥ 3.35; absent in MySQL
//!
//! A backend crate implements `SqlDialect` for its database and passes the
//! dialect into a shared `SqlRepositoryBase` (if the framework provides one)
//! or uses it in its own query-building logic. This means MySQL, PostgreSQL,
//! Oracle, and SQLite all share the same `CREATE TABLE` generator — only the
//! dialect object differs.
//!
//! Non-SQL backends (Meilisearch, MongoDB, Redis) do **not** use this trait
//! at all; they implement `Repository<T>` directly.

use crate::entity::{ColumnDef, ColumnType};

/// SQL syntax adapter.
///
/// Implement this for your database to get shared DDL / DML generation from
/// the SQL utilities in this module.
pub trait SqlDialect: Send + Sync + 'static {
    /// Human-readable name used in logging and error messages.
    fn name(&self) -> &'static str;

    /// Parameter placeholder for the nth bind value (1-indexed).
    ///
    /// - SQLite / MySQL: always returns `"?"` (ignores `index`)
    /// - PostgreSQL: returns `"$1"`, `"$2"`, …
    /// - Oracle: returns `":1"`, `":2"`, …
    fn placeholder(&self, index: usize) -> String;

    /// DDL keyword to declare an auto-increment primary key column.
    ///
    /// Examples: `"AUTOINCREMENT"` (SQLite), `"AUTO_INCREMENT"` (MySQL),
    /// `""` (PostgreSQL uses `SERIAL` or `GENERATED ALWAYS`).
    fn auto_increment_keyword(&self) -> &'static str;

    /// Whether `INSERT INTO … RETURNING id` is supported.
    ///
    /// When `true` the driver can recover the generated primary key from the
    /// INSERT statement itself. When `false` it must fall back to
    /// `last_insert_rowid()` or an equivalent.
    fn supports_returning(&self) -> bool;

    /// Wrap an identifier in the appropriate quote characters.
    ///
    /// Standard SQL and PostgreSQL use `"name"`; MySQL uses `` `name` ``.
    fn quote_identifier(&self, name: &str) -> String {
        format!("\"{}\"", name)
    }

    /// Map a [`ColumnType`] to the SQL type keyword for this dialect.
    fn col_type_ddl(&self, col_type: &ColumnType) -> &'static str;

    /// Build a `CREATE TABLE IF NOT EXISTS` statement.
    ///
    /// Default implementation uses [`Self::quote_identifier`],
    /// [`Self::col_type_ddl`], and [`Self::auto_increment_keyword`] — override
    /// only when the dialect needs something special (e.g. PostgreSQL `SERIAL`).
    fn create_table_sql(&self, table: &str, cols: &[ColumnDef]) -> String {
        let mut parts = Vec::new();
        for col in cols {
            let sql_type = self.col_type_ddl(&col.col_type);
            let mut def = format!("{} {}", self.quote_identifier(col.name), sql_type);
            if col.primary_key {
                def.push_str(" PRIMARY KEY");
                let ai = self.auto_increment_keyword();
                if col.auto && !ai.is_empty() {
                    def.push(' ');
                    def.push_str(ai);
                }
            } else {
                if !col.nullable {
                    def.push_str(" NOT NULL");
                }
                if col.unique {
                    def.push_str(" UNIQUE");
                }
            }
            parts.push(def);
        }
        format!(
            "CREATE TABLE IF NOT EXISTS {} (\n  {}\n)",
            self.quote_identifier(table),
            parts.join(",\n  ")
        )
    }

    /// Build a `SELECT col1, col2, … FROM table` statement (no WHERE).
    fn select_all_sql(&self, table: &str, cols: &[ColumnDef]) -> String {
        let col_list: Vec<String> = cols
            .iter()
            .map(|c| self.quote_identifier(c.name))
            .collect();
        format!(
            "SELECT {} FROM {}",
            col_list.join(", "),
            self.quote_identifier(table)
        )
    }

    /// Build an `INSERT INTO … VALUES (?, ?, …)` statement, skipping auto columns.
    fn insert_sql(&self, table: &str, cols: &[ColumnDef]) -> String {
        let writable: Vec<&ColumnDef> = cols.iter().filter(|c| !c.auto).collect();
        let names: Vec<String> = writable
            .iter()
            .map(|c| self.quote_identifier(c.name))
            .collect();
        let placeholders: Vec<String> = (1..=writable.len()).map(|i| self.placeholder(i)).collect();
        format!(
            "INSERT INTO {} ({}) VALUES ({})",
            self.quote_identifier(table),
            names.join(", "),
            placeholders.join(", ")
        )
    }

    /// Build an `UPDATE table SET col = ?, … WHERE pk = ?` statement.
    fn update_sql(&self, table: &str, cols: &[ColumnDef]) -> String {
        let pk = cols
            .iter()
            .find(|c| c.primary_key)
            .map(|c| c.name)
            .unwrap_or("id");
        let updatable: Vec<&ColumnDef> = cols.iter().filter(|c| !c.primary_key && !c.auto).collect();
        let assignments: Vec<String> = updatable
            .iter()
            .enumerate()
            .map(|(i, c)| {
                format!(
                    "{} = {}",
                    self.quote_identifier(c.name),
                    self.placeholder(i + 1)
                )
            })
            .collect();
        format!(
            "UPDATE {} SET {} WHERE {} = {}",
            self.quote_identifier(table),
            assignments.join(", "),
            self.quote_identifier(pk),
            self.placeholder(updatable.len() + 1)
        )
    }

    /// Build a `DELETE FROM table WHERE pk = ?` statement.
    fn delete_sql(&self, table: &str, pk_col: &str) -> String {
        format!(
            "DELETE FROM {} WHERE {} = {}",
            self.quote_identifier(table),
            self.quote_identifier(pk_col),
            self.placeholder(1)
        )
    }
}
