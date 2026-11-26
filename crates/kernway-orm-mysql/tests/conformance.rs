//! The shared SQL conformance suite against a live MySQL
//! (`KERNWAY_MYSQL_URL`, a scratch database).
#![cfg(feature = "mysql")]

use kernway_orm_core::{error::OrmError, SqlDialect};
use kernway_orm_mysql::{MySqlDialect, MySqlDriver};
use kernway_orm_sql::conformance::{run_all, Harness};

#[derive(Clone)]
struct MySql(String);

impl Harness for MySql {
    type Connect = kernway_orm_mysql::MySqlConnect;
    type Driver = MySqlDriver;
    async fn connect(&self) -> MySqlDriver {
        MySqlDriver::connect(&self.0).await.expect("connect")
    }
    async fn exec<'a>(&'a self, db: &'a MySqlDriver, sql: &'a str) -> Result<(), OrmError> {
        db.execute(sql).await
    }
    fn dialect(&self) -> &'static dyn SqlDialect {
        &MySqlDialect
    }
}

#[test]
fn mysql_conforms() {
    let Ok(url) = std::env::var("KERNWAY_MYSQL_URL") else {
        eprintln!("skip: KERNWAY_MYSQL_URL not set");
        return;
    };
    run_all(MySql(url));
}
