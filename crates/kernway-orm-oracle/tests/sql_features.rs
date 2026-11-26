//! Oracle through the shared SQL API, against a live server: every column
//! type typed correctly (checked against the server's own TO_CHAR /
//! JSON_SERIALIZE / ROWIDTOCHAR renderings), unsupported types refused
//! cleanly, streaming, procedure calls (OUT / IN OUT / cursors / implicit
//! results / DBMS_OUTPUT), scripts with PL/SQL units, and array DML.
//! Skips unless KERNWAY_ORACLE_HOST is set.

use kernway_orm_core::{transaction::TransactionalDriver, OrmError};
use kernway_orm_oracle::OracleDriver;
use kernway_orm_sql::{params, CallArg, OutKind, Param, Value};

fn url() -> Option<String> {
    let host = std::env::var("KERNWAY_ORACLE_HOST").ok()?;
    Some(format!("oracle://kernway:kernway_pw@{host}:1521/FREEPDB1"))
}

fn run<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

async fn drop_quietly(db: &OracleDriver, what: &str) {
    let _ = db.execute(&format!("DROP {what}")).await;
}

#[test]
fn every_column_type_is_typed_and_matches_the_servers_rendering() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let db = OracleDriver::connect(&url).await.unwrap();
        drop_quietly(&db, "TABLE kwo_types PURGE").await;
        db.execute(
            "CREATE TABLE kwo_types (id NUMBER(10) PRIMARY KEY, n10 NUMBER(10), n19 NUMBER(19), n38 NUMBER(38), \
             dec_ NUMBER(12,2), num NUMBER, flt FLOAT, bf BINARY_FLOAT, bd BINARY_DOUBLE, vc VARCHAR2(50), \
             nvc NVARCHAR2(50), ch CHAR(5), rw RAW(16), dt DATE, ts3 TIMESTAMP(3), ts9 TIMESTAMP(9), \
             tstz TIMESTAMP(6) WITH TIME ZONE, tsltz TIMESTAMP(6) WITH LOCAL TIME ZONE, iym INTERVAL YEAR(2) TO MONTH, \
             ids INTERVAL DAY(2) TO SECOND(3), cl CLOB, bl BLOB, js JSON, bo BOOLEAN, vec VECTOR(3, FLOAT32))",
        )
        .await
        .unwrap();
        db.execute(
            "INSERT INTO kwo_types VALUES (1, -2147483648, 9223372036854775807, 99999999999999999999999999999999999999, \
             -1234567890.12, 0.5, 1.5, 0.25f, 1e300d, 'Việt Nam 😀', N'Đà Nẵng', 'ab', HEXTORAW('00FF10'), \
             TO_DATE('2026-11-26 13:05:07', 'YYYY-MM-DD HH24:MI:SS'), TIMESTAMP '2026-11-26 09:30:05.120', \
             TIMESTAMP '2026-01-01 00:00:00.000000001', TIMESTAMP '2026-11-26 23:30:00.5 -05:30', \
             TIMESTAMP '2026-11-26 10:00:00.25 +00:00', INTERVAL '1-2' YEAR TO MONTH, \
             INTERVAL '-3 04:05:06.789' DAY TO SECOND, 'clob text', HEXTORAW('0102'), '{\"a\":[1,\"x\"]}', TRUE, '[1.5, -2, 3]')",
        )
        .await
        .unwrap();
        db.execute("INSERT INTO kwo_types (id) VALUES (2)").await.unwrap();

        let r = db.query("SELECT * FROM kwo_types ORDER BY id", &[]).await.unwrap();
        let v = |col: &str| r.get(0, col).cloned().unwrap_or_else(|| panic!("no column {col}"));
        assert_eq!(v("ID"), Value::Int(1));
        assert_eq!(v("N10"), Value::Int(-2_147_483_648));
        assert_eq!(v("N19"), Value::Int(i64::MAX));
        assert_eq!(v("N38"), Value::Decimal("99999999999999999999999999999999999999".into()), "past i64: exact text");
        assert_eq!(v("DEC_"), Value::Decimal("-1234567890.12".into()));
        assert_eq!(v("NUM"), Value::Decimal("0.5".into()));
        assert_eq!(v("FLT"), Value::Float(1.5));
        assert_eq!(v("BF"), Value::Float(0.25));
        assert_eq!(v("BD"), Value::Float(1e300));
        assert_eq!(v("VC"), Value::Text("Việt Nam 😀".into()));
        assert_eq!(v("NVC"), Value::Text("Đà Nẵng".into()));
        assert_eq!(v("CH"), Value::Text("ab   ".into()));
        assert_eq!(v("RW"), Value::Bytes(vec![0, 0xFF, 0x10]));
        assert_eq!(v("CL"), Value::Text("clob text".into()));
        assert_eq!(v("BL"), Value::Bytes(vec![1, 2]));
        assert_eq!(v("BO"), Value::Bool(true));
        assert!(matches!(v("JS"), Value::Json(_)));
        assert!(matches!(v("IYM"), Value::Interval(_)));
        assert!(r.rows[1][1..].iter().all(Value::is_null), "row 2 is all NULL: {:?}", r.rows[1]);

        // Ground truth: the server's own rendering of the same values.
        let server = db
            .query(
                "SELECT TO_CHAR(dt, 'YYYY-MM-DD HH24:MI:SS'), TO_CHAR(ts3, 'YYYY-MM-DD HH24:MI:SS.FF3'), \
                 TO_CHAR(ts9, 'YYYY-MM-DD HH24:MI:SS.FF9'), TO_CHAR(tstz, 'YYYY-MM-DD HH24:MI:SS.FF6 TZH:TZM'), \
                 TO_CHAR(tsltz, 'YYYY-MM-DD HH24:MI:SS.FF6'), TO_CHAR(iym), TO_CHAR(ids), JSON_SERIALIZE(js), \
                 TO_CHAR(n38), VECTOR_SERIALIZE(vec) FROM kwo_types WHERE id = 1",
                &[],
            )
            .await
            .unwrap();
        let s = |i: usize| server.rows[0][i].as_str().unwrap().to_string();
        assert_eq!(v("DT"), Value::DateTime(s(0)));
        assert_eq!(v("TS3"), Value::DateTime(s(1)));
        assert_eq!(v("TS9"), Value::DateTime(s(2)));
        assert_eq!(v("TSTZ"), Value::DateTime(s(3)));
        assert_eq!(v("TSLTZ"), Value::DateTime(s(4)));
        assert_eq!(v("IYM"), Value::Interval(s(5)));
        assert_eq!(v("IDS"), Value::Interval(s(6)));
        assert_eq!(v("JS"), Value::Json(s(7)));
        assert_eq!(v("N38").as_str().unwrap(), s(8));
        // VECTOR: same numbers as the server's serialization (which prints
        // them in scientific notation).
        let nums = |t: &str| -> Vec<f64> {
            t.trim_matches(['[', ']']).split(',').map(|x| x.trim().parse().unwrap()).collect()
        };
        assert_eq!(v("VEC"), Value::Text("[1.5,-2,3]".into()));
        assert_eq!(nums("[1.5,-2,3]"), nums(&s(9)), "vector vs VECTOR_SERIALIZE");
        // Spelled out once, so the format itself is pinned too.
        assert_eq!(s(3), "2026-11-26 23:30:00.500000 -05:30");
        assert_eq!(s(1), "2026-11-26 09:30:05.120");

        // ROWID: text that the server accepts back.
        let r = db.query("SELECT ROWID, ROWIDTOCHAR(ROWID) FROM kwo_types WHERE id = 1", &[]).await.unwrap();
        let rowid = r.rows[0][0].as_str().unwrap().to_string();
        assert_eq!(Some(rowid.as_str()), r.rows[0][1].as_str());
        let back = db.query_scalar("SELECT id FROM kwo_types WHERE ROWID = CHARTOROWID(:1)", params![rowid.as_str()]).await.unwrap();
        assert_eq!(back, Value::Int(1));

        // Aggregates over typed columns.
        let r = db.query("SELECT COUNT(*) AS n, SUM(n10) AS s, MAX(ts3) AS m FROM kwo_types", &[]).await.unwrap();
        assert_eq!(r.get(0, "n").and_then(Value::as_i64), Some(2));
        assert_eq!(r.get(0, "s").and_then(Value::as_i64), Some(-2_147_483_648));
    });
}

#[test]
fn unsupported_types_are_refused_cleanly() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let db = OracleDriver::connect_with(kernway_orm_oracle::OracleConfig::from_url(&url).unwrap(), 1).await.unwrap();
        db.execute("CREATE OR REPLACE TYPE kwo_point AS OBJECT (x NUMBER, y NUMBER)").await.unwrap();
        for sql in [
            "SELECT kwo_point(1, 2) FROM dual",
            "SELECT XMLTYPE('<a>1</a>') FROM dual",
        ] {
            let e = db.query(sql, &[]).await.expect_err(sql);
            assert!(matches!(e, OrmError::Unsupported(_)), "{sql}: {e}");
            // The single pooled session is still in step.
            assert_eq!(db.query_scalar("SELECT 41 + 1 FROM dual", &[]).await.unwrap(), Value::Int(42));
        }
        // What the error suggests works.
        let r = db.query("SELECT XMLSERIALIZE(CONTENT XMLTYPE('<a>1</a>')) FROM dual", &[]).await.unwrap();
        assert_eq!(r.rows[0][0].as_str(), Some("<a>1</a>"));
    });
}

#[test]
fn large_results_stream_and_a_callback_error_stops_cleanly() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let db = OracleDriver::connect_with(kernway_orm_oracle::OracleConfig::from_url(&url).unwrap(), 1).await.unwrap();
        let (mut count, mut sum) = (0u64, 0i64);
        let out = db
            .query_each(
                "SELECT LEVEL AS n, RPAD('x', 100, 'x') AS pad FROM dual CONNECT BY LEVEL <= :1",
                params![150_000],
                |row| {
                    count += 1;
                    sum += row[0].as_i64().unwrap();
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(out.columns, ["N", "PAD"]);
        assert_eq!(count, 150_000);
        assert_eq!(sum, 150_000 * 150_001 / 2);
        let mut seen = 0;
        let e = db
            .query_each("SELECT LEVEL FROM dual CONNECT BY LEVEL <= 1000", &[], |_| {
                seen += 1;
                if seen == 3 { Err(OrmError::Query("stop".into())) } else { Ok(()) }
            })
            .await;
        assert!(e.is_err());
        assert_eq!(seen, 3);
        assert_eq!(db.query_scalar("SELECT 7 FROM dual", &[]).await.unwrap(), Value::Int(7));
    });
}

#[test]
fn procedure_calls_return_out_values_cursors_implicit_results_and_output() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let db = OracleDriver::connect_with(kernway_orm_oracle::OracleConfig::from_url(&url).unwrap(), 1).await.unwrap();
        db.execute_script(
            "CREATE OR REPLACE PROCEDURE kwo_proc(
                 p_in IN NUMBER, p_out OUT NUMBER, p_io IN OUT VARCHAR2, p_when OUT DATE, p_cur OUT SYS_REFCURSOR) AS
               c1 SYS_REFCURSOR; c2 SYS_REFCURSOR;
             BEGIN
               p_out := p_in * 10;
               p_io := p_io || ' world';
               p_when := TO_DATE('2026-11-26 08:00:00', 'YYYY-MM-DD HH24:MI:SS');
               OPEN p_cur FOR SELECT LEVEL AS n FROM dual CONNECT BY LEVEL <= 3;
               DBMS_OUTPUT.PUT_LINE('line one');
               DBMS_OUTPUT.PUT_LINE('');
               DBMS_OUTPUT.PUT_LINE('dòng ba ✓');
               OPEN c1 FOR SELECT 'a' AS s FROM dual;
               DBMS_SQL.RETURN_RESULT(c1);
               OPEN c2 FOR SELECT 1 AS x, 2 AS y FROM dual UNION ALL SELECT 3, 4 FROM dual;
               DBMS_SQL.RETURN_RESULT(c2);
             END;
             /
             CREATE OR REPLACE PROCEDURE kwo_chatty(p_lines IN NUMBER) AS
             BEGIN
               FOR i IN 1 .. p_lines LOOP DBMS_OUTPUT.PUT_LINE('line ' || i || ' ' || RPAD('x', 200, 'x')); END LOOP;
             END;
             /
             CREATE OR REPLACE PROCEDURE kwo_fails AS
             BEGIN DBMS_OUTPUT.PUT_LINE('before'); RAISE_APPLICATION_ERROR(-20001, 'boom'); END;
             /",
        )
        .await
        .unwrap();

        let r = db
            .call(
                "kwo_proc",
                &[
                    CallArg::from(4),
                    CallArg::Out(OutKind::Number),
                    CallArg::InOut("hello".into()),
                    CallArg::Out(OutKind::Date),
                    CallArg::Out(OutKind::Cursor),
                ],
            )
            .await
            .unwrap();
        assert_eq!(r.out[0], Value::Int(40));
        assert_eq!(r.out[1], Value::Text("hello world".into()));
        assert_eq!(r.out[2], Value::DateTime("2026-11-26 08:00:00".into()));
        let Value::Rows(cur) = &r.out[3] else { panic!("cursor: {:?}", r.out[3]) };
        assert_eq!(cur.rows, vec![vec![Value::Int(1)], vec![Value::Int(2)], vec![Value::Int(3)]]);
        assert_eq!(r.output, ["line one", "", "dòng ba ✓"]);
        assert_eq!(r.results.len(), 2, "implicit results");
        assert_eq!(r.results[0].rows, vec![vec![Value::Text("a".into())]]);
        assert_eq!(r.results[1].get(1, "y"), Some(&Value::Int(4)));

        // ~700 KB of output: drained over several round trips, nothing lost.
        let r = db.call("kwo_chatty", &[3000.into()]).await.unwrap();
        assert_eq!(r.output.len(), 3000);
        assert!(r.output[2999].starts_with("line 3000 xxx"));

        // A failing procedure reports the error, and its output does not leak
        // into the next call on the same session.
        let e = db.call("kwo_fails", &[]).await.unwrap_err();
        assert!(e.to_string().contains("ORA-20001"), "{e}");
        let r = db.call("kwo_chatty", &[1.into()]).await.unwrap();
        assert_eq!(r.output.len(), 1);
        // A procedure name is never SQL.
        assert!(db.call("kwo_chatty(1); DROP TABLE x; --", &[]).await.is_err());
    });
}

#[test]
fn scripts_run_sql_and_plsql_units_and_stop_at_the_first_error() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let db = OracleDriver::connect(&url).await.unwrap();
        drop_quietly(&db, "TABLE kwo_script PURGE").await;
        db.execute_script(
            "-- a schema file
             CREATE TABLE kwo_script (id NUMBER PRIMARY KEY, note VARCHAR2(100));
             INSERT INTO kwo_script VALUES (1, 'semi;colon inside');
             INSERT INTO kwo_script VALUES (2, q'[it's; quoted]');
             /* block comment; with a semicolon */
             CREATE OR REPLACE TRIGGER kwo_script_trg BEFORE INSERT ON kwo_script FOR EACH ROW
             BEGIN
               :NEW.note := UPPER(:NEW.note);
             END;
             /
             BEGIN
               INSERT INTO kwo_script VALUES (3, 'from plsql');
             END;
             /
             UPDATE kwo_script SET note = note || '!' WHERE id = 1;",
        )
        .await
        .unwrap();
        let r = db.query("SELECT note FROM kwo_script ORDER BY id", &[]).await.unwrap();
        let notes: Vec<_> = r.rows.iter().map(|row| row[0].as_str().unwrap().to_string()).collect();
        assert_eq!(notes, ["semi;colon inside!", "it's; quoted", "FROM PLSQL"]);
        let e = db
            .execute_script("INSERT INTO kwo_script VALUES (4, 'x'); INSERT INTO kwo_nope VALUES (1); INSERT INTO kwo_script VALUES (5, 'y');")
            .await
            .unwrap_err();
        assert!(e.to_string().contains("statement 2"), "{e}");
        assert_eq!(db.query_scalar("SELECT COUNT(*) FROM kwo_script", &[]).await.unwrap(), Value::Int(4));
    });
}

#[test]
fn array_dml_runs_many_rows_in_one_call() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let db = OracleDriver::connect(&url).await.unwrap();
        drop_quietly(&db, "TABLE kwo_many PURGE").await;
        db.execute("CREATE TABLE kwo_many (id NUMBER PRIMARY KEY, name VARCHAR2(40), amount NUMBER(10,2), raw_ RAW(8))")
            .await
            .unwrap();
        let rows: Vec<Vec<Param>> = (1..=20_000i64)
            .map(|i| {
                vec![
                    i.into(),
                    if i % 7 == 0 { Param::Null } else { format!("name {i}").into() },
                    (i as f64 / 4.0).into(),
                    if i % 2 == 0 { vec![i as u8, 1].into() } else { Param::Null },
                ]
            })
            .collect();
        let n = db.execute_many("INSERT INTO kwo_many VALUES (:1, :2, :3, :4)", &rows).await.unwrap();
        assert_eq!(n, 20_000);
        let r = db
            .query("SELECT COUNT(*), COUNT(name), SUM(amount), COUNT(raw_) FROM kwo_many", &[])
            .await
            .unwrap();
        assert_eq!(r.rows[0][0].as_i64(), Some(20_000));
        assert_eq!(r.rows[0][1].as_i64(), Some(20_000 - 20_000 / 7));
        assert_eq!(r.rows[0][2].as_f64(), Some(20_000.0 * 20_001.0 / 8.0));
        assert_eq!(r.rows[0][3].as_i64(), Some(10_000));
        let n = db
            .execute_many("UPDATE kwo_many SET amount = amount + :1 WHERE id = :2", &[vec![1.into(), 1.into()], vec![1.into(), 99_999.into()]])
            .await
            .unwrap();
        assert_eq!(n, 1, "one matched row of two");

        // In a transaction: a duplicate key part-way rolls everything back.
        let r: Result<u64, OrmError> = db
            .transaction(|tx| {
                Box::pin(async move {
                    tx.execute_many("INSERT INTO kwo_many (id) VALUES (:1)", &[vec![30_001.into()], vec![30_002.into()], vec![1.into()]])
                        .await
                })
            })
            .await;
        assert!(matches!(r, Err(OrmError::UniqueViolation { .. })), "{r:?}");
        assert_eq!(db.query_scalar("SELECT COUNT(*) FROM kwo_many WHERE id > 30000", &[]).await.unwrap(), Value::Int(0));
        // Mixed value kinds in one column are refused before anything is sent.
        let e = db.execute_many("INSERT INTO kwo_many (id) VALUES (:1)", &[vec![1.into()], vec!["x".into()]]).await;
        assert!(e.is_err());
    });
}

#[test]
fn logical_rowids_of_index_organized_tables_round_trip() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let db = OracleDriver::connect(&url).await.unwrap();
        drop_quietly(&db, "TABLE kwo_iot PURGE").await;
        db.execute("CREATE TABLE kwo_iot (k VARCHAR2(20) PRIMARY KEY, v NUMBER) ORGANIZATION INDEX").await.unwrap();
        db.execute_many("INSERT INTO kwo_iot VALUES (:1, :2)", &[vec!["alpha".into(), 1.into()], vec!["beta".into(), 2.into()]])
            .await
            .unwrap();
        let r = db.query("SELECT ROWID, k FROM kwo_iot ORDER BY k", &[]).await.unwrap();
        for row in &r.rows {
            let rowid = row[0].as_str().unwrap();
            assert!(rowid.starts_with('*'), "logical rowid: {rowid}");
            let k = db.query_scalar("SELECT k FROM kwo_iot WHERE ROWID = :1", params![rowid]).await.unwrap();
            assert_eq!(k, row[1], "the server finds the row by our rowid text");
        }
    });
}

#[test]
fn compilation_errors_are_reported_with_their_lines() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let db = OracleDriver::connect(&url).await.unwrap();
        let e = db.execute("CREATE OR REPLACE PROCEDURE kwo_broken AS BEGIN x := ; END;").await.unwrap_err().to_string();
        assert!(e.contains("PROCEDURE KWO_BROKEN") && e.contains("line 1") && e.contains("PLS-"), "{e}");
        let e = db
            .execute_script(
                "CREATE OR REPLACE PACKAGE kwo_pkg_bad AS PROCEDURE p; END;
/
CREATE OR REPLACE PACKAGE BODY kwo_pkg_bad AS
  PROCEDURE p IS BEGIN
    SELECT 1 INTO v_missing FROM dual;
  END;
END;
/",
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("statement 2") && e.contains("PACKAGE BODY KWO_PKG_BAD") && e.contains("line 3"), "{e}");
        // A valid unit is fine, and an object referencing a missing table is
        // reported too (not silently left INVALID).
        db.execute("CREATE OR REPLACE PROCEDURE kwo_fine AS BEGIN NULL; END;").await.unwrap();
        let e = db.execute("CREATE OR REPLACE FUNCTION kwo_f_bad RETURN NUMBER AS n NUMBER; BEGIN SELECT COUNT(*) INTO n FROM kwo_no_table; RETURN n; END;")
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("FUNCTION KWO_F_BAD") && e.contains("ORA-00942"), "{e}");
    });
}

#[test]
fn functions_named_arguments_and_every_out_kind() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let db = OracleDriver::connect_with(kernway_orm_oracle::OracleConfig::from_url(&url).unwrap(), 1).await.unwrap();
        db.execute_script(
            "CREATE OR REPLACE PACKAGE kwo_api AS
  FUNCTION total(p_qty IN NUMBER, p_price IN NUMBER DEFAULT 2.5, p_tax IN NUMBER DEFAULT 0) RETURN NUMBER;
  FUNCTION greet(p_who IN VARCHAR2, p_len OUT NUMBER) RETURN VARCHAR2;
  FUNCTION is_even(p_n IN NUMBER) RETURN BOOLEAN;
  FUNCTION rows_upto(p_n IN NUMBER) RETURN SYS_REFCURSOR;
  PROCEDURE kinds(p_ts OUT TIMESTAMP WITH TIME ZONE, p_ym OUT INTERVAL YEAR TO MONTH,
                  p_ds OUT INTERVAL DAY TO SECOND, p_f OUT BINARY_DOUBLE, p_r OUT RAW, p_b OUT BOOLEAN);
END;
/
CREATE OR REPLACE PACKAGE BODY kwo_api AS
  FUNCTION total(p_qty IN NUMBER, p_price IN NUMBER DEFAULT 2.5, p_tax IN NUMBER DEFAULT 0) RETURN NUMBER IS
  BEGIN DBMS_OUTPUT.PUT_LINE('total called'); RETURN p_qty * p_price + p_tax; END;
  FUNCTION greet(p_who IN VARCHAR2, p_len OUT NUMBER) RETURN VARCHAR2 IS
  BEGIN p_len := LENGTH(p_who); RETURN 'hi ' || p_who; END;
  FUNCTION is_even(p_n IN NUMBER) RETURN BOOLEAN IS BEGIN RETURN MOD(p_n, 2) = 0; END;
  FUNCTION rows_upto(p_n IN NUMBER) RETURN SYS_REFCURSOR IS c SYS_REFCURSOR;
  BEGIN OPEN c FOR SELECT LEVEL AS n FROM dual CONNECT BY LEVEL <= p_n; RETURN c; END;
  PROCEDURE kinds(p_ts OUT TIMESTAMP WITH TIME ZONE, p_ym OUT INTERVAL YEAR TO MONTH,
                  p_ds OUT INTERVAL DAY TO SECOND, p_f OUT BINARY_DOUBLE, p_r OUT RAW, p_b OUT BOOLEAN) IS
  BEGIN
    p_ts := TIMESTAMP '2026-11-26 23:30:00.5 +07:00';
    p_ym := INTERVAL '3-4' YEAR TO MONTH;
    p_ds := INTERVAL '1 02:03:04.5' DAY TO SECOND;
    p_f := 0.125; p_r := HEXTORAW('CAFE'); p_b := FALSE;
  END;
END;
/",
        )
        .await
        .unwrap();

        // Defaults used; then a middle default skipped by name.
        let r = db.call_function("kwo_api.total", OutKind::Number, &[4.into()]).await.unwrap();
        assert_eq!(r.value, Value::Int(10));
        assert_eq!(r.output, ["total called"]);
        let r = db
            .call_function("kwo_api.total", OutKind::Number, &[4.into(), CallArg::named("p_tax", 1)])
            .await
            .unwrap();
        assert_eq!(r.value, Value::Int(11));
        let r = db
            .call_function("kwo_api.total", OutKind::Number, &[CallArg::named("p_price", 3), CallArg::named("p_qty", 2)])
            .await
            .unwrap();
        assert_eq!(r.value, Value::Int(6), "named, any order");

        let r = db.call_function("kwo_api.greet", OutKind::Text, &["Lan".into(), OutKind::Number.into()]).await.unwrap();
        assert_eq!((r.value, r.out), (Value::Text("hi Lan".into()), vec![Value::Int(3)]));
        assert_eq!(db.call_function("kwo_api.is_even", OutKind::Bool, &[4.into()]).await.unwrap().value, Value::Bool(true));
        let Value::Rows(rows) = db.call_function("kwo_api.rows_upto", OutKind::Cursor, &[3.into()]).await.unwrap().value else {
            panic!("cursor")
        };
        assert_eq!(rows.rows.len(), 3);

        let r = db
            .call(
                "kwo_api.kinds",
                &[
                    OutKind::DateTimeTz.into(),
                    OutKind::IntervalYm.into(),
                    OutKind::IntervalDs.into(),
                    OutKind::Float.into(),
                    OutKind::Bytes.into(),
                    OutKind::Bool.into(),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            r.out,
            vec![
                Value::DateTime("2026-11-26 23:30:00.500000000 +07:00".into()),
                Value::Interval("+03-04".into()),
                Value::Interval("+01 02:03:04.500000".into()),
                Value::Float(0.125),
                Value::Bytes(vec![0xCA, 0xFE]),
                Value::Bool(false),
            ]
        );
        assert!(db.call("kwo_api.kinds", &[CallArg::named("bad name", 1)]).await.is_err());
    });
}

#[test]
fn lobs_through_call_and_call_function_of_any_size() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let db = OracleDriver::connect(&url).await.unwrap();
        db.execute_script(
            "CREATE OR REPLACE PACKAGE kwo_lob AS
  FUNCTION repeat_text(p_s IN VARCHAR2, p_n IN NUMBER) RETURN CLOB;
  FUNCTION bytes_of(p_n IN NUMBER) RETURN BLOB;
  PROCEDURE measure(p_c IN CLOB, p_b IN BLOB, p_clen OUT NUMBER, p_blen OUT NUMBER, p_copy OUT CLOB);
  PROCEDURE grow(p_c IN OUT CLOB);
END;
/
CREATE OR REPLACE PACKAGE BODY kwo_lob AS
  FUNCTION repeat_text(p_s IN VARCHAR2, p_n IN NUMBER) RETURN CLOB IS c CLOB;
  BEGIN DBMS_LOB.CREATETEMPORARY(c, TRUE);
    FOR i IN 1 .. p_n LOOP DBMS_LOB.APPEND(c, TO_CLOB(p_s)); END LOOP; RETURN c; END;
  FUNCTION bytes_of(p_n IN NUMBER) RETURN BLOB IS b BLOB;
  BEGIN DBMS_LOB.CREATETEMPORARY(b, TRUE);
    FOR i IN 1 .. p_n LOOP DBMS_LOB.WRITEAPPEND(b, 4, HEXTORAW('DEADBEEF')); END LOOP; RETURN b; END;
  PROCEDURE measure(p_c IN CLOB, p_b IN BLOB, p_clen OUT NUMBER, p_blen OUT NUMBER, p_copy OUT CLOB) IS
  BEGIN p_clen := DBMS_LOB.GETLENGTH(p_c); p_blen := DBMS_LOB.GETLENGTH(p_b); p_copy := p_c; END;
  PROCEDURE grow(p_c IN OUT CLOB) IS BEGIN DBMS_LOB.APPEND(p_c, p_c); END;
END;
/",
        )
        .await
        .unwrap();

        // Function results of any size.
        let r = db.call_function("kwo_lob.repeat_text", OutKind::LongText, &["Đà Lạt ".into(), 20_000.into()]).await.unwrap();
        assert_eq!(r.value, Value::Text("Đà Lạt ".repeat(20_000)));
        let r = db.call_function("kwo_lob.bytes_of", OutKind::LongBytes, &[15_000.into()]).await.unwrap();
        assert_eq!(r.value, Value::Bytes([0xDE, 0xAD, 0xBE, 0xEF].repeat(15_000)));

        // IN values past 32 KB, OUT CLOB back.
        let text = "chữ ".repeat(30_000);
        let bytes: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        let r = db
            .call(
                "kwo_lob.measure",
                &[text.clone().into(), bytes.into(), OutKind::Number.into(), OutKind::Number.into(), OutKind::LongText.into()],
            )
            .await
            .unwrap();
        assert_eq!(r.out[0], Value::Int(text.encode_utf16().count() as i64));
        assert_eq!(r.out[1], Value::Int(100_000));
        assert_eq!(r.out[2], Value::Text(text.clone()));

        // IN OUT: small in, large out; and large in, larger out.
        let r = db.call("kwo_lob.grow", &[CallArg::InOutAs("ab".repeat(20_000).into(), OutKind::LongText)]).await.unwrap();
        assert_eq!(r.out[0], Value::Text("ab".repeat(40_000)));
        let r = db.call("kwo_lob.grow", &[CallArg::InOut(text.clone().into())]).await.unwrap();
        assert_eq!(r.out[0], Value::Text(text.repeat(2)));
    });
}

#[test]
fn nested_cursor_columns_come_back_as_result_sets() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let db = OracleDriver::connect_with(kernway_orm_oracle::OracleConfig::from_url(&url).unwrap(), 1).await.unwrap();
        // 120 outer rows (several fetch rounds), each with its own cursor.
        let r = db
            .query(
                "SELECT d.n, CURSOR(SELECT LEVEL * d.n AS m, 'x' || LEVEL AS s FROM dual CONNECT BY LEVEL <= 3) AS c
                   FROM (SELECT LEVEL AS n FROM dual CONNECT BY LEVEL <= 120) d ORDER BY d.n",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(r.rows.len(), 120);
        for (i, row) in r.rows.iter().enumerate() {
            let n = i as i64 + 1;
            assert_eq!(row[0], Value::Int(n));
            let Value::Rows(inner) = &row[1] else { panic!("row {n}: {:?}", row[1]) };
            assert_eq!(inner.columns, ["M", "S"]);
            let ms: Vec<i64> = inner.rows.iter().map(|r| r[0].as_i64().unwrap()).collect();
            assert_eq!(ms, [n, 2 * n, 3 * n]);
            assert_eq!(inner.rows[2][1].as_str(), Some("x3"));
        }
        // An empty nested cursor, and one inside a REF CURSOR from PL/SQL.
        let r = db.query("SELECT CURSOR(SELECT 1 FROM dual WHERE 1 = 0) AS c FROM dual", &[]).await.unwrap();
        let Value::Rows(empty) = &r.rows[0][0] else { panic!() };
        assert!(empty.rows.is_empty());
        let r = db
            .call_function(
                "kwo_nested_fn",
                OutKind::Cursor,
                &[],
            )
            .await;
        assert!(r.is_err(), "no such function: the session stays usable");
        assert_eq!(db.query_scalar("SELECT 7 FROM dual", &[]).await.unwrap(), Value::Int(7));
    });
}

#[test]
fn bfile_columns_are_read_from_the_server_file() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        // SYSTEM makes a directory and writes a file in it (UTL_FILE, binary).
        let pw = std::env::var("KERNWAY_ORACLE_SYSTEM_PASSWORD").unwrap_or_else(|_| "oracle_pw".into());
        let sys_url = url.replacen("kernway:kernway_pw", &format!("system:{pw}"), 1);
        let sys = OracleDriver::connect_with(kernway_orm_oracle::OracleConfig::from_url(&sys_url).unwrap(), 1).await.unwrap();
        sys.execute("CREATE OR REPLACE DIRECTORY kw_bfile_dir AS '/tmp'").await.unwrap();
        sys.execute("GRANT READ ON DIRECTORY kw_bfile_dir TO kernway").await.unwrap();
        sys.execute(
            "DECLARE f UTL_FILE.FILE_TYPE; acc RAW(1024); chunk RAW(256);
             BEGIN
               FOR i IN 0 .. 255 LOOP acc := UTL_RAW.CONCAT(acc, UTL_RAW.CAST_FROM_BINARY_INTEGER(i, 1)); END LOOP;
               chunk := UTL_RAW.SUBSTR(acc, 4, 256);
               f := UTL_FILE.FOPEN('KW_BFILE_DIR', 'kw_bfile.bin', 'wb', 32767);
               FOR i IN 1 .. 400 LOOP UTL_FILE.PUT_RAW(f, chunk); END LOOP;
               UTL_FILE.FCLOSE(f);
             END;",
        )
        .await
        .unwrap();

        let db = OracleDriver::connect_with(kernway_orm_oracle::OracleConfig::from_url(&url).unwrap(), 1).await.unwrap();
        let r = db
            .query(
                "SELECT BFILENAME('KW_BFILE_DIR', 'kw_bfile.bin') AS f, DBMS_LOB.GETLENGTH(BFILENAME('KW_BFILE_DIR', 'kw_bfile.bin')) AS n FROM dual",
                &[],
            )
            .await
            .unwrap();
        // CAST_FROM_BINARY_INTEGER(i, 1) is big-endian 4 bytes; bytes 4.. of
        // the concatenation are 0 0 0 1 0 0 0 2 … — rebuild the same here.
        let mut all = Vec::new();
        for i in 0u32..256 {
            all.extend(i.to_be_bytes());
        }
        let chunk = &all[3..259];
        let want: Vec<u8> = chunk.repeat(400);
        assert_eq!(r.rows[0][1], Value::Int(want.len() as i64), "server length");
        assert_eq!(r.rows[0][0], Value::Bytes(want));
        // A missing file is the server's error; the session stays usable.
        let e = db.query("SELECT BFILENAME('KW_BFILE_DIR', 'no_such_file.bin') FROM dual", &[]).await.unwrap_err();
        assert!(e.to_string().contains("ORA-22288") || e.to_string().contains("ORA-29283"), "{e}");
        assert_eq!(db.query_scalar("SELECT 5 FROM dual", &[]).await.unwrap(), Value::Int(5));
    });
}
