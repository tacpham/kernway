//! The shared SQL conformance suite against a live SQL Server.
//! Needs `KERNWAY_MSSQL_URL` (tests/support/mssql-setup.sh); says `skip:` without.

#![cfg(feature = "mssql")]

use kernway_orm_core::{error::OrmError, SqlDialect};
use kernway_orm_mssql::{MssqlConnect, MssqlDriver, SqlServerDialect};
use kernway_orm_sql::conformance::{run_all, Harness};

#[derive(Clone)]
struct Mssql(String);

impl Harness for Mssql {
    type Connect = MssqlConnect;
    type Driver = MssqlDriver;
    async fn connect(&self) -> MssqlDriver {
        MssqlDriver::connect(&self.0).await.expect("connect")
    }
    async fn exec<'a>(&'a self, db: &'a MssqlDriver, sql: &'a str) -> Result<(), OrmError> {
        db.execute(sql).await
    }
    fn dialect(&self) -> &'static dyn SqlDialect {
        &SqlServerDialect
    }
}

#[test]
fn sqlserver_conforms() {
    let Ok(url) = std::env::var("KERNWAY_MSSQL_URL") else {
        eprintln!("skip: KERNWAY_MSSQL_URL not set (tests/support/mssql-setup.sh)");
        return;
    };
    run_all(Mssql(url));
}
