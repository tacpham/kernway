//! Scrollable cursors against a real Oracle: first / last / absolute /
//! relative / next / prior moves over one open cursor, rows checked against
//! their row numbers; CLOB columns (defined once) and bind values.
//! Skips unless `KERNWAY_ORACLE_HOST` is set.

use kernway_orm_oracle::{Bind, Conn, OracleConfig, Scroll, Value};

fn run<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

fn nums(rows: &[Vec<Value>]) -> Vec<i64> {
    rows.iter().map(|r| r[0].as_i64().unwrap()).collect()
}

#[test]
fn scrollable_cursor_moves_anywhere_in_the_result() {
    let Ok(host) = std::env::var("KERNWAY_ORACLE_HOST") else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let cfg = OracleConfig::from_url(&format!("oracle://kernway:kernway_pw@{host}:1521/FREEPDB1")).unwrap();
        let mut c = Conn::connect(&cfg).await.unwrap();
        let mut cur = c
            .open_scrollable(
                "SELECT LEVEL AS n, TO_CLOB(RPAD('c', LEVEL, 'c')) AS c FROM dual WHERE LEVEL >= :lo CONNECT BY LEVEL <= 500",
                &[Bind::In(Value::Number("1".into()))],
            )
            .await
            .unwrap();
        assert_eq!(cur.position(), 0);
        let r = c.scroll(&mut cur, Scroll::First, 3).await.unwrap();
        assert_eq!((nums(&r), cur.position()), (vec![1, 2, 3], 1));
        assert_eq!(r[2][1], Value::Str("ccc".into()), "a CLOB column through the define");
        let r = c.scroll(&mut cur, Scroll::Next, 4).await.unwrap();
        assert_eq!((nums(&r), cur.position()), (vec![4, 5, 6, 7], 4));
        let r = c.scroll(&mut cur, Scroll::Last, 10).await.unwrap();
        assert_eq!((nums(&r), cur.position()), (vec![500], 500));
        let r = c.scroll(&mut cur, Scroll::Absolute(250), 2).await.unwrap();
        assert_eq!((nums(&r), cur.position()), (vec![250, 251], 250));
        let r = c.scroll(&mut cur, Scroll::Relative(-100), 1).await.unwrap();
        assert_eq!((nums(&r), cur.position()), (vec![150], 150));
        let r = c.scroll(&mut cur, Scroll::Prior, 5).await.unwrap();
        assert_eq!((nums(&r), cur.position()), (vec![145, 146, 147, 148, 149], 145));
        assert_eq!(r[4][1], Value::Str("c".repeat(149)));
        // Past the end, and before the start: no rows, nothing breaks.
        assert!(c.scroll(&mut cur, Scroll::Absolute(501), 3).await.unwrap().is_empty());
        assert!(c.scroll(&mut cur, Scroll::Relative(-1000), 3).await.unwrap().is_empty());
        let r = c.scroll(&mut cur, Scroll::Absolute(499), 10).await.unwrap();
        assert_eq!(nums(&r), [499, 500], "a batch stops at the last row");
        // Other statements on the session in between, then back to row 1.
        let other = c.execute("SELECT 7 FROM dual", &[]).await.unwrap();
        assert_eq!(other.rows[0][0].as_i64(), Some(7));
        let r = c.scroll(&mut cur, Scroll::First, 1).await.unwrap();
        assert_eq!(nums(&r), [1]);
        c.close_scrollable(cur);
        assert_eq!(c.execute("SELECT 8 FROM dual", &[]).await.unwrap().rows[0][0].as_i64(), Some(8));
    });
}

#[test]
fn scrollable_cursor_through_the_driver_returns_shared_rows() {
    let Ok(host) = std::env::var("KERNWAY_ORACLE_HOST") else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        use kernway_orm_sql::{params, Value as V};
        let db = kernway_orm_oracle::OracleDriver::connect(&format!("oracle://kernway:kernway_pw@{host}:1521/FREEPDB1")).await.unwrap();
        let mut cur = db.scrollable("SELECT LEVEL * :1 AS n FROM dual CONNECT BY LEVEL <= 10", params![3]).await.unwrap();
        let last = cur.fetch(Scroll::Last, 1).await.unwrap();
        assert_eq!((last.columns.clone(), last.rows[0][0].clone(), cur.position()), (vec!["N".to_string()], V::Int(30), 10));
        let r = cur.fetch(Scroll::Absolute(2), 3).await.unwrap();
        assert_eq!(r.rows.iter().map(|r| r[0].as_i64().unwrap()).collect::<Vec<_>>(), [6, 9, 12]);
        drop(cur);
        assert_eq!(db.query_scalar("SELECT 1 FROM dual", &[]).await.unwrap(), V::Int(1), "the connection is reusable");
    });
}
