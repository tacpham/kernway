//! Live tests against a real Oracle server; skip unless KERNWAY_ORACLE_HOST is set.

#[test]
fn logs_in_with_user_and_password() {
    let Ok(host) = std::env::var("KERNWAY_ORACLE_HOST") else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    rt_core::Executor::new().unwrap().block_on(async move {
        let cfg = kernway_orm_oracle::OracleConfig::from_url(&format!("oracle://kernway:kernway_pw@{host}:1521/FREEPDB1")).unwrap();
        let conn = kernway_orm_oracle::Conn::connect(&cfg).await.unwrap();
        println!("logged in: fv={} session_id={:?} version={:?}", conn.field_version,
            conn.session.get("AUTH_SESSION_ID"), conn.session.get("AUTH_VERSION_NO"));
        // A wrong password is refused with ORA-01017.
        let mut bad = cfg.clone();
        bad.password = "nope".into();
        let err = kernway_orm_oracle::Conn::connect(&bad).await.err().expect("wrong password must fail");
        println!("wrong password: {err}");
        assert!(err.to_string().contains("ORA-01017"), "{err}");
    }).unwrap();
}

use kernway_orm_oracle::{Bind, Conn, OracleConfig, OutType, Value};

fn cfg() -> Option<OracleConfig> {
    let host = std::env::var("KERNWAY_ORACLE_HOST").ok()?;
    Some(OracleConfig::from_url(&format!("oracle://kernway:kernway_pw@{host}:1521/FREEPDB1")).unwrap())
}

fn run<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

fn s(v: &Value) -> String {
    v.to_text().unwrap_or_else(|| "NULL".into())
}

#[test]
fn select_with_binds_and_types() {
    let Some(cfg) = cfg() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let mut c = Conn::connect(&cfg).await.unwrap();
        let r = c
            .execute(
                "SELECT :n + 1 AS n, UPPER(:s) AS s, 123.45 AS d, -0.001 AS small, NULL AS nothing, \
                 DATE '2024-03-15' + 0.5 AS dt, TIMESTAMP '2024-03-15 13:45:30.123456' AS ts, \
                 CAST(2.5 AS BINARY_DOUBLE) AS bd, N'Việt ✓' AS nv, HEXTORAW('DEADBEEF') AS rw, \
                 TRUE AS b FROM dual",
                &[Bind::from(41i64), Bind::from("chào")],
            )
            .await
            .unwrap();
        let names: Vec<&str> = r.columns.iter().map(|c| c.name.as_str()).collect();
        println!("columns: {names:?}");
        let row: Vec<String> = r.rows[0].iter().map(s).collect();
        println!("row: {row:?}");
        assert_eq!(row[0], "42");
        assert_eq!(row[1], "CHÀO");
        assert_eq!(row[2], "123.45");
        assert_eq!(row[3], "-0.001");
        assert_eq!(row[4], "NULL");
        assert_eq!(row[5], "2024-03-15 12:00:00");
        assert_eq!(row[6], "2024-03-15 13:45:30.123456000");
        assert_eq!(row[7], "2.5");
        assert_eq!(row[8], "Việt ✓");
        assert_eq!(row[9], "DEADBEEF");
        assert_eq!(r.rows[0][10], Value::Bool(true));
    });
}

#[test]
fn many_rows_are_fetched_in_rounds() {
    let Some(cfg) = cfg() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let mut c = Conn::connect(&cfg).await.unwrap();
        let r = c
            .execute(
                "SELECT level AS n, 'row ' || level AS label, MOD(level, 3) AS m FROM dual CONNECT BY level <= :n",
                &[Bind::from(1000i64)],
            )
            .await
            .unwrap();
        assert_eq!(r.rows.len(), 1000);
        for (i, row) in r.rows.iter().enumerate() {
            let n = i as i64 + 1;
            assert_eq!(row[0].as_i64(), Some(n));
            assert_eq!(s(&row[1]), format!("row {n}"));
            assert_eq!(row[2].as_i64(), Some(n % 3), "bit-vector duplicates resolved, row {n}");
        }
        // Empty result.
        let r = c.execute("SELECT 1 FROM dual WHERE 1 = 0", &[]).await.unwrap();
        assert!(r.rows.is_empty());
    });
}

#[test]
fn dml_commit_rollback_and_errors() {
    let Some(cfg) = cfg() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let mut c = Conn::connect(&cfg).await.unwrap();
        let _ = c.execute("DROP TABLE kw_items PURGE", &[]).await;
        c.execute("CREATE TABLE kw_items (id NUMBER PRIMARY KEY, name VARCHAR2(50), price NUMBER(10,2))", &[])
            .await
            .unwrap();
        let r = c
            .execute("INSERT INTO kw_items VALUES (:1, :2, :3)", &[Bind::from(1i64), Bind::from("Gel polish"), Bind::In(Value::Number("25.50".into()))])
            .await
            .unwrap();
        assert_eq!(r.row_count, 1);
        c.commit().await.unwrap();
        c.execute("INSERT INTO kw_items VALUES (2, 'temp', 1)", &[]).await.unwrap();
        c.rollback().await.unwrap();
        let r = c.execute("SELECT id, name, price FROM kw_items ORDER BY id", &[]).await.unwrap();
        assert_eq!(r.rows.len(), 1, "rolled-back row is gone");
        assert_eq!(s(&r.rows[0][2]), "25.5");
        let r = c.execute("UPDATE kw_items SET price = price * 2 WHERE id = :id", &[Bind::from(1i64)]).await.unwrap();
        assert_eq!(r.row_count, 1);

        // Errors: unknown table, duplicate key — and the session keeps working.
        let e = c.execute("SELECT * FROM kw_no_such_table", &[]).await.unwrap_err();
        println!("error: {e}");
        assert!(e.to_string().contains("ORA-00942"), "{e}");
        let e = c.execute("INSERT INTO kw_items VALUES (1, 'dup', 1)", &[]).await.unwrap_err();
        println!("error: {e}");
        assert!(matches!(e, kernway_orm_core::OrmError::UniqueViolation { .. }), "{e}");
        let r = c.execute("SELECT COUNT(*) FROM kw_items", &[]).await.unwrap();
        assert_eq!(r.rows[0][0].as_i64(), Some(1));
        c.execute("DROP TABLE kw_items PURGE", &[]).await.unwrap();
        c.close().await.unwrap();
    });
}

#[test]
fn plsql_blocks_procedures_and_functions() {
    let Some(cfg) = cfg() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let mut c = Conn::connect(&cfg).await.unwrap();
        c.execute(
            "CREATE OR REPLACE FUNCTION kw_add_tax(cents IN NUMBER) RETURN NUMBER IS \
             BEGIN RETURN cents + ROUND(cents * 0.086); END;",
            &[],
        )
        .await
        .unwrap();
        c.execute(
            "CREATE OR REPLACE PROCEDURE kw_split(total IN NUMBER, label IN OUT VARCHAR2, \
               half OUT NUMBER, at_time OUT DATE, ok OUT BOOLEAN) IS \
             BEGIN half := total / 2; label := label || ' (split)'; \
               at_time := DATE '2026-11-26'; ok := total > 0; END;",
            &[],
        )
        .await
        .unwrap();

        // Function via an anonymous block.
        let r = c
            .execute("BEGIN :r := kw_add_tax(:cents); END;", &[Bind::Out(OutType::Number), Bind::from(1000i64)])
            .await
            .unwrap();
        assert_eq!(s(&r.out[0]), "1086");
        assert_eq!(r.out[1], Value::Null, "IN binds report no value");

        // Procedure with IN, IN OUT and OUT parameters of several types.
        let r = c
            .execute(
                "BEGIN kw_split(:total, :label, :half, :at, :ok); END;",
                &[
                    Bind::from(25i64),
                    Bind::InOut(Value::from("Mani"), OutType::Str(100)),
                    Bind::Out(OutType::Number),
                    Bind::Out(OutType::Date),
                    Bind::Out(OutType::Bool),
                ],
            )
            .await
            .unwrap();
        println!("out: {:?}", r.out);
        assert_eq!(s(&r.out[1]), "Mani (split)");
        assert_eq!(s(&r.out[2]), "12.5");
        assert_eq!(s(&r.out[3]), "2026-11-26 00:00:00");
        assert_eq!(r.out[4], Value::Bool(true));

        // A repeated name in PL/SQL is one bind.
        let r = c
            .execute("BEGIN :x := :x * 2 + 1; END;", &[Bind::InOut(Value::from(20i64), OutType::Number)])
            .await
            .unwrap();
        assert_eq!(s(&r.out[0]), "41");

        // A PL/SQL error surfaces with its ORA code; the session survives.
        let e = c
            .execute("BEGIN RAISE_APPLICATION_ERROR(-20001, 'card declined'); END;", &[])
            .await
            .unwrap_err();
        println!("error: {e}");
        assert!(e.to_string().contains("ORA-20001") && e.to_string().contains("card declined"), "{e}");
        let r = c.execute("SELECT 7 FROM dual", &[]).await.unwrap();
        assert_eq!(r.rows[0][0].as_i64(), Some(7));

        c.execute("DROP PROCEDURE kw_split", &[]).await.unwrap();
        c.execute("DROP FUNCTION kw_add_tax", &[]).await.unwrap();
    });
}

#[test]
fn clob_and_blob_read_and_write() {
    let Some(cfg) = cfg() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let mut c = Conn::connect(&cfg).await.unwrap();
        let _ = c.execute("DROP TABLE kw_lobs PURGE", &[]).await;
        c.execute("CREATE TABLE kw_lobs (id NUMBER PRIMARY KEY, note VARCHAR2(20), c CLOB, nc NCLOB, b BLOB)", &[])
            .await
            .unwrap();
        let sizes = [0usize, 1, 100, 4000, 4001, 32767, 32768, 100_000, 1_000_000];
        for (i, &n) in sizes.iter().enumerate() {
            let text: String = "Việt✓ab".chars().cycle().take(n).collect();
            let bytes: Vec<u8> = (0..n as u32).map(|k| (k.wrapping_mul(2654435761) >> 24) as u8).collect();
            c.execute(
                "INSERT INTO kw_lobs (id, note, c, nc, b) VALUES (:id, :note, :c, :nc, :b)",
                &[
                    Bind::from(i as i64),
                    Bind::from(format!("n={n}")),
                    Bind::In(if n == 0 { Value::Null } else { Value::Str(text.clone()) }),
                    Bind::In(if n == 0 { Value::Null } else { Value::Str(text.clone()) }),
                    Bind::In(if n == 0 { Value::Null } else { Value::Raw(bytes.clone()) }),
                ],
            )
            .await
            .unwrap_or_else(|e| panic!("insert {n}: {e}"));
        }
        c.commit().await.unwrap();
        let r = c.execute("SELECT id, note, c, nc, b, DBMS_LOB.GETLENGTH(c) FROM kw_lobs ORDER BY id", &[]).await.unwrap();
        assert_eq!(r.rows.len(), sizes.len());
        for (row, &n) in r.rows.iter().zip(&sizes) {
            let text: String = "Việt✓ab".chars().cycle().take(n).collect();
            let bytes: Vec<u8> = (0..n as u32).map(|k| (k.wrapping_mul(2654435761) >> 24) as u8).collect();
            assert_eq!(s(&row[1]), format!("n={n}"));
            if n == 0 {
                assert_eq!(row[2], Value::Null);
                assert_eq!(row[4], Value::Null);
                continue;
            }
            assert_eq!(row[2], Value::Str(text.clone()), "CLOB of {n} chars");
            assert_eq!(row[3], Value::Str(text.clone()), "NCLOB of {n} chars");
            assert_eq!(row[4], Value::Raw(bytes), "BLOB of {n} bytes");
            assert_eq!(row[5].as_i64(), Some(n as i64), "server-side length of {n}");
        }
        c.execute("DROP TABLE kw_lobs PURGE", &[]).await.unwrap();
    });
}

#[test]
fn ref_cursors_from_procedures_and_functions() {
    let Some(cfg) = cfg() else { return eprintln!("skip: KERNWAY_ORACLE_HOST not set") };
    run(async move {
        let mut c = Conn::connect(&cfg).await.unwrap();
        c.execute(
            "CREATE OR REPLACE PROCEDURE kw_two_cursors(n IN NUMBER, a OUT SYS_REFCURSOR, b OUT SYS_REFCURSOR) IS \
             BEGIN \
               OPEN a FOR SELECT level AS id, 'item ' || level AS name, TO_CLOB(RPAD('z', 4000, 'z')) || TO_CLOB(RPAD('y', 1000, 'y')) AS big \
                 FROM dual CONNECT BY level <= n; \
               OPEN b FOR SELECT 'only' AS v FROM dual WHERE 1 = 0; \
             END;",
            &[],
        )
        .await
        .unwrap();
        c.execute(
            "CREATE OR REPLACE FUNCTION kw_cursor_fn RETURN SYS_REFCURSOR IS r SYS_REFCURSOR; \
             BEGIN OPEN r FOR SELECT level * 10 AS x FROM dual CONNECT BY level <= 250; RETURN r; END;",
            &[],
        )
        .await
        .unwrap();

        let r = c
            .execute(
                "BEGIN kw_two_cursors(:n, :a, :b); END;",
                &[Bind::from(130i64), Bind::Out(OutType::Cursor), Bind::Out(OutType::Cursor)],
            )
            .await
            .unwrap();
        let Value::Cursor(a) = &r.out[1] else { panic!("expected a cursor, got {:?}", r.out[1]) };
        assert_eq!(a.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["ID", "NAME", "BIG"]);
        assert_eq!(a.rows.len(), 130, "rows beyond one fetch round");
        assert_eq!(a.rows[129][0].as_i64(), Some(130));
        assert_eq!(s(&a.rows[0][1]), "item 1");
        assert_eq!(s(&a.rows[0][2]).len(), 5000, "CLOB column inside a ref cursor");
        let Value::Cursor(b) = &r.out[2] else { panic!("expected a cursor") };
        assert!(b.rows.is_empty(), "an empty cursor");

        let r = c.execute("BEGIN :r := kw_cursor_fn; END;", &[Bind::Out(OutType::Cursor)]).await.unwrap();
        let Value::Cursor(f) = &r.out[0] else { panic!("expected a cursor") };
        assert_eq!(f.rows.len(), 250);
        assert_eq!(f.rows.iter().map(|r| r[0].as_i64().unwrap()).sum::<i64>(), 10 * 250 * 251 / 2);

        // Cursors are closed afterwards: many calls never hit ORA-01000.
        for _ in 0..400 {
            c.execute("BEGIN :r := kw_cursor_fn; END;", &[Bind::Out(OutType::Cursor)]).await.unwrap();
        }
        c.execute("DROP PROCEDURE kw_two_cursors", &[]).await.unwrap();
        c.execute("DROP FUNCTION kw_cursor_fn", &[]).await.unwrap();
    });
}

#[test]
fn tcps_connections_with_each_certificate_check() {
    let (Ok(host), Ok(ca)) = (std::env::var("KERNWAY_ORACLE_HOST"), std::env::var("KERNWAY_ORACLE_CA")) else {
        return eprintln!("skip: KERNWAY_ORACLE_HOST / KERNWAY_ORACLE_CA not set");
    };
    let url = |q: &str| OracleConfig::from_url(&format!("oracle://kernway:kernway_pw@{host}:2484/FREEPDB1?{q}")).unwrap();
    run(async move {
        for q in [
            "ssl-mode=required".to_string(),
            format!("ssl-mode=verify_ca&ssl-ca={ca}"),
            format!("ssl-mode=verify_identity&ssl-ca={ca}"),
        ] {
            let mut c = Conn::connect(&url(&q)).await.unwrap_or_else(|e| panic!("{q}: {e}"));
            let r = c
                .execute("SELECT SYS_CONTEXT('USERENV', 'NETWORK_PROTOCOL') FROM dual", &[])
                .await
                .unwrap();
            assert_eq!(s(&r.rows[0][0]).to_lowercase(), "tcps", "{q}");
            // A 300 KB CLOB round trip over TLS (writes well past rustls's buffer).
            let big = "ü".repeat(150_000);
            let _ = c.execute("DROP TABLE kw_tls_lob PURGE", &[]).await;
            c.execute("CREATE TABLE kw_tls_lob (c CLOB)", &[]).await.unwrap();
            c.execute("INSERT INTO kw_tls_lob VALUES (:t)", &[Bind::from(big.clone())]).await.unwrap();
            let r = c.execute("SELECT c, DBMS_LOB.GETLENGTH(c) FROM kw_tls_lob", &[]).await.unwrap();
            assert_eq!(r.rows[0][1].as_i64(), Some(150_000), "{q}");
            assert_eq!(r.rows[0][0], Value::Str(big), "{q}");
            c.execute("DROP TABLE kw_tls_lob PURGE", &[]).await.unwrap();
            c.close().await.unwrap();
        }
        // The server certificate is self-signed: public roots must reject it.
        let e = Conn::connect(&url("ssl-mode=verify_ca")).await.err().expect("untrusted certificate");
        assert!(e.to_string().contains("TLS handshake"), "{e}");
        // Plain TCP to the TLS port is refused / fails cleanly, never hangs.
        assert!(Conn::connect(&url("ssl-mode=disabled")).await.is_err());
    });
}
