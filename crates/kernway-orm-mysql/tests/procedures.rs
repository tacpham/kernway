//! Stored procedures and multi-result responses against a real MySQL server.
//! Skips unless `KERNWAY_MYSQL_URL` is set (see `integration.rs`).
#![cfg(feature = "mysql")]

use kernway_orm_core::OrmError;
use kernway_orm_macro::entity;
use kernway_orm_mysql::{BindValue, Conn, MySqlDriver, MysqlConfig, QueryResult};
use serde::{Deserialize, Serialize};

/// The server URL, with the test procedures created exactly once per run
/// (the tests run in parallel and would otherwise race on DROP/CREATE).
fn url() -> Option<String> {
    static SETUP: std::sync::Once = std::sync::Once::new();
    let url = std::env::var("KERNWAY_MYSQL_URL").ok()?;
    SETUP.call_once(|| {
        let u = url.clone();
        run(async move {
            let mut c = Conn::connect(&MysqlConfig::from_url(&u).unwrap()).await.unwrap();
            setup(&mut c).await;
        });
    });
    Some(url)
}

fn run<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

/// (Re)create the procedures these tests call. Plain COM_QUERY: no DELIMITER
/// needed outside the mysql CLI.
async fn setup(conn: &mut Conn) {
    for sql in [
        "DROP PROCEDURE IF EXISTS kwp_none",
        "CREATE PROCEDURE kwp_none(IN n INT) BEGIN DO n; END",
        "DROP PROCEDURE IF EXISTS kwp_two",
        "CREATE PROCEDURE kwp_two(IN n INT) BEGIN SELECT n AS a; SELECT n * 2 AS b, 'x' AS c; END",
        "DROP PROCEDURE IF EXISTS kwp_out",
        "CREATE PROCEDURE kwp_out(IN n INT, OUT r INT) BEGIN SET r = n * 10; END",
        "DROP PROCEDURE IF EXISTS kwp_people",
        "CREATE PROCEDURE kwp_people() BEGIN \
           SELECT 'Mai' AS name, 2 AS extra, 7 AS id UNION ALL SELECT 'Lan', 0, 8; END",
        "DROP PROCEDURE IF EXISTS kwp_fails_late",
        "CREATE PROCEDURE kwp_fails_late() BEGIN SELECT 1 AS ok; SELECT * FROM kwp_no_such_table; END",
    ] {
        conn.query(sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}

fn rows(r: &QueryResult) -> &Vec<Vec<Option<String>>> {
    match r {
        QueryResult::Rows { rows, .. } => rows,
        QueryResult::Ok { .. } => panic!("expected rows, got {r:?}"),
    }
}

async fn alive(conn: &mut Conn) {
    let r = conn.query("SELECT 41 + 1").await.expect("connection still usable");
    assert_eq!(rows(&r)[0][0].as_deref(), Some("42"));
}

#[test]
fn call_returns_every_result_set_then_the_call_status() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_MYSQL_URL not set") };
    run(async move {
        let mut c = Conn::connect(&MysqlConfig::from_url(&url).unwrap()).await.unwrap();

        // No result set: just the call's OK.
        let r = c.call("kwp_none", &[BindValue::Int(1)]).await.unwrap();
        assert!(matches!(r.as_slice(), [QueryResult::Ok { .. }]), "{r:?}");

        // Two SELECTs → two result sets, then the OK.
        let r = c.call("kwp_two", &[BindValue::Int(21)]).await.unwrap();
        assert_eq!(r.len(), 3, "{r:?}");
        assert_eq!(rows(&r[0]), &vec![vec![Some("21".to_string())]]);
        assert_eq!(rows(&r[1]), &vec![vec![Some("42".to_string()), Some("x".to_string())]]);
        assert!(matches!(r[2], QueryResult::Ok { .. }));
        alive(&mut c).await;

        // The same over the text protocol, and an OUT parameter.
        let r = c.query_multi("CALL kwp_two(5)").await.unwrap();
        assert_eq!(r.len(), 3);
        c.query_multi("CALL kwp_out(4, @kw_r)").await.unwrap();
        let r = c.query("SELECT @kw_r").await.unwrap();
        assert_eq!(rows(&r)[0][0].as_deref(), Some("40"));

        // A single-result API on a multi-result CALL returns the first result
        // set and drains the rest, leaving the connection in sync.
        let r = c.query("CALL kwp_two(3)").await.unwrap();
        assert_eq!(rows(&r)[0][0].as_deref(), Some("3"));
        alive(&mut c).await;
        let r = c.exec_prepared("CALL kwp_two(?)", &[BindValue::Int(4)]).await.unwrap();
        assert_eq!(rows(&r)[0][0].as_deref(), Some("4"));
        alive(&mut c).await;

        // Schema-qualified names work; anything that is not an identifier is
        // refused before it reaches SQL.
        let db: String = rows(&c.query("SELECT DATABASE()").await.unwrap())[0][0].clone().unwrap();
        c.call(&format!("{db}.kwp_none"), &[BindValue::Int(0)]).await.unwrap();
        for bad in ["kwp_none; DROP TABLE x", "a.b.c", "", "kwp`none"] {
            assert!(c.call(bad, &[]).await.is_err(), "{bad:?} must be refused");
        }
        alive(&mut c).await;
    });
}

#[test]
fn an_error_inside_a_procedure_leaves_the_connection_usable() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_MYSQL_URL not set") };
    run(async move {
        let mut c = Conn::connect(&MysqlConfig::from_url(&url).unwrap()).await.unwrap();
        // First result set arrives, then the second SELECT fails: the ERR ends
        // the response, the error surfaces, and nothing is left unread.
        let err = c.call("kwp_fails_late", &[]).await.unwrap_err();
        assert!(err.to_string().contains("1146"), "{err}");
        alive(&mut c).await;
        let err = c.query("CALL kwp_fails_late()").await.unwrap_err();
        assert!(err.to_string().contains("1146"), "{err}");
        alive(&mut c).await;
    });
}

#[test]
fn a_row_that_fails_to_decode_does_not_desync_the_connection() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_MYSQL_URL not set") };
    run(async move {
        let mut c = Conn::connect(&MysqlConfig::from_url(&url).unwrap()).await.unwrap();
        c.query("SET SESSION cte_max_recursion_depth = 10000").await.unwrap();
        let sql = "WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 3000) \
                   SELECT n FROM s";
        // The row function fails on row 2 of 3000: the error is returned only
        // after every remaining row has been read off the wire.
        let mut seen = 0;
        let err = c
            .query_with(sql, |_cells| {
                seen += 1;
                if seen == 2 {
                    Err(OrmError::TypeConversion("boom".into()))
                } else {
                    Ok(())
                }
            })
            .await
            .unwrap_err();
        assert!(matches!(err, OrmError::TypeConversion(_)));
        assert_eq!(seen, 2, "rows after the failure are drained, not decoded");
        alive(&mut c).await;
        // Same over the binary protocol.
        let err = c
            .exec_prepared_with(&format!("{sql} WHERE n > ?"), &[BindValue::Int(0)], |_cells| {
                Err::<(), _>(OrmError::TypeConversion("boom".into()))
            })
            .await
            .unwrap_err();
        assert!(matches!(err, OrmError::TypeConversion(_)));
        alive(&mut c).await;
    });
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "kwp_person")]
struct Person {
    #[id(strategy = "manual")]
    id: i64,
    name: String,
}

#[test]
fn call_as_maps_the_first_result_set_by_column_name() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_MYSQL_URL not set") };
    run(async move {
        let db = MySqlDriver::connect(&url).await.unwrap();
        // Columns come back as (name, extra, id): out of entity order, plus one
        // the entity does not have.
        let people: Vec<Person> = db.call_as("kwp_people", &[]).await.unwrap();
        assert_eq!(
            people,
            vec![Person { id: 7, name: "Mai".into() }, Person { id: 8, name: "Lan".into() }]
        );
        assert!(db.call_as::<Person>("kwp_none", &[BindValue::Int(1)]).await.unwrap().is_empty());
        db.execute("SELECT 1").await.unwrap();
    });
}
