//! The shared SQL conformance suite against a live PostgreSQL.
//!
//! Needs `KERNWAY_PG_URL` (see `tests/support/pg-setup.sh`); without it the
//! test says `skip:` (the CI driver job and the acceptance run fail on that).

#![cfg(feature = "postgres")]

use kernway_orm_core::{error::OrmError, SqlDialect};
use kernway_orm_postgres::{PgConnect, PostgresDialect, PostgresDriver};
use kernway_orm_sql::conformance::{run_all, Harness};

#[derive(Clone)]
struct Pg(String);

impl Harness for Pg {
    type Connect = PgConnect;
    type Driver = PostgresDriver;
    async fn connect(&self) -> PostgresDriver {
        PostgresDriver::connect(&self.0).await.expect("connect")
    }
    async fn exec<'a>(&'a self, db: &'a PostgresDriver, sql: &'a str) -> Result<(), OrmError> {
        db.execute(sql).await
    }
    fn dialect(&self) -> &'static dyn SqlDialect {
        &PostgresDialect
    }
}

#[test]
fn postgres_conforms() {
    let Ok(url) = std::env::var("KERNWAY_PG_URL") else {
        eprintln!("skip: KERNWAY_PG_URL not set (tests/support/pg-setup.sh)");
        return;
    };
    run_all(Pg(url));
}
