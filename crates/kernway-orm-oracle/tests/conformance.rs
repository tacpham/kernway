//! The shared SQL conformance suite against a live Oracle
//! (`KERNWAY_ORACLE_HOST`, user `kernway` on `FREEPDB1`).

use kernway_orm_core::{error::OrmError, SqlDialect};
use kernway_orm_oracle::{OracleDialect, OracleDriver};
use kernway_orm_sql::conformance::{run_all, Harness};

#[derive(Clone)]
struct Oracle(String);

impl Harness for Oracle {
    type Driver = OracleDriver;
    async fn connect(&self) -> OracleDriver {
        OracleDriver::connect(&self.0).await.expect("connect")
    }
    async fn exec<'a>(&'a self, db: &'a OracleDriver, sql: &'a str) -> Result<(), OrmError> {
        db.execute(sql).await
    }
    fn dialect(&self) -> &'static dyn SqlDialect {
        &OracleDialect
    }
}

#[test]
fn oracle_conforms() {
    let Ok(host) = std::env::var("KERNWAY_ORACLE_HOST") else {
        eprintln!("skip: KERNWAY_ORACLE_HOST not set");
        return;
    };
    run_all(Oracle(format!("oracle://kernway:kernway_pw@{host}:1521/FREEPDB1")));
}
