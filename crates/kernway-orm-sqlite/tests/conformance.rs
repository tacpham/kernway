//! The shared SQL conformance suite against a real SQLite database file
//! (fresh per run, WAL mode). Needs nothing installed: always runs.

use kernway_orm_core::{error::OrmError, SqlDialect};
use kernway_orm_sql::conformance::{run_all, Harness};
use kernway_orm_sqlite::{SqliteConfig, SqliteConnect, SqliteDialect, SqliteDriver};
use std::path::PathBuf;

#[derive(Clone)]
struct Sqlite(PathBuf);

impl Harness for Sqlite {
    type Connect = SqliteConnect;
    type Driver = SqliteDriver;
    async fn connect(&self) -> SqliteDriver {
        SqliteDriver::with_config(SqliteConfig::file(&self.0)).await.expect("open")
    }
    async fn exec<'a>(&'a self, db: &'a SqliteDriver, sql: &'a str) -> Result<(), OrmError> {
        db.execute(sql).await
    }
    fn dialect(&self) -> &'static dyn SqlDialect {
        &SqliteDialect
    }
}

#[test]
fn sqlite_conforms() {
    let dir = std::env::temp_dir().join(format!("kw-sqlite-conf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    run_all(Sqlite(dir.join("conformance.db")));
    let _ = std::fs::remove_dir_all(&dir);
}
