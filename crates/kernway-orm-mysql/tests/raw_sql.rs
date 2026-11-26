//! Hand-written SQL against a real MySQL server: every column type decoded
//! to the right typed value (and identically over the text and binary
//! protocols — the text protocol is the server's own formatting), bound
//! parameters, joins / aggregates mapped by name, streaming, scripts,
//! LOAD DATA LOCAL INFILE, and raw SQL inside transactions.
//! Skips unless `KERNWAY_MYSQL_URL` is set.
#![cfg(feature = "mysql")]

use kernway_orm_core::{transaction::TransactionalDriver, OrmError};
use kernway_orm_macro::entity;
use kernway_orm_mysql::{params, MySqlDriver, Param, Rows, Value};
use serde::{Deserialize, Serialize};

fn url() -> Option<String> {
    std::env::var("KERNWAY_MYSQL_URL").ok()
}

fn run<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

const TYPES_DDL: &str = "CREATE TABLE kwr_types (
    id INT PRIMARY KEY, t TINYINT, ut TINYINT UNSIGNED, s SMALLINT, m MEDIUMINT, i INT,
    ui INT UNSIGNED, b BIGINT, ub BIGINT UNSIGNED, f FLOAT, d DOUBLE, dec_ DECIMAL(20,6),
    dt DATE, dtm DATETIME, dtm6 DATETIME(6), ts3 TIMESTAMP(3) NULL, tm TIME, tm6 TIME(6),
    yr YEAR, ch CHAR(5), vc VARCHAR(50), tx TEXT, bn BINARY(4), vb VARBINARY(10), bl BLOB,
    js JSON, bt BIT(10), en ENUM('a','b'), st SET('x','y','z'), geo GEOMETRY, bo BOOLEAN)";

fn v_text(s: &str) -> Value {
    Value::Text(s.into())
}

#[test]
fn every_column_type_decodes_to_its_typed_value_on_both_protocols() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_MYSQL_URL not set") };
    run(async move {
        let db = MySqlDriver::connect(&url).await.unwrap();
        db.execute("DROP TABLE IF EXISTS kwr_types").await.unwrap();
        db.execute(TYPES_DDL).await.unwrap();
        db.execute_script(
            "INSERT INTO kwr_types VALUES (1, -5, 200, -300, 70000, -2147483648, 4294967295,
               -9223372036854775808, 18446744073709551615, 0.1, 1.7976931348623157e308,
               -99999999999999.999999, '1000-01-01', '2026-11-26 00:00:00',
               '2026-01-01 00:00:00.000001', '2026-11-26 00:00:00.000', '-838:59:59',
               '-00:00:00.500000', 2155, '', 'Việt Nam 😀', 'long text', x'00FF0000', x'',
               x'000102FF', '{\"a\": [1, \"x\"]}', b'1111111111', 'b', 'x,z',
               ST_GeomFromText('POINT(1 2)'), TRUE);
             INSERT INTO kwr_types (id, dtm, dtm6, tm6, ts3, f, d) VALUES
               (2, '2026-11-26 09:30:05', '2026-11-26 09:30:05.250000', '25:00:01.123456',
                '2026-11-26 09:30:05.120', -2.5, 0);
             INSERT INTO kwr_types (id) VALUES (3);",
        )
        .await
        .unwrap();

        // Text protocol (no parameters) vs binary protocol (a parameter): the
        // same rows, value for value.
        let text = db.query("SELECT * FROM kwr_types ORDER BY id", &[]).await.unwrap();
        let binary = db.query("SELECT * FROM kwr_types WHERE id > ? ORDER BY id", params![0]).await.unwrap();
        assert_eq!(text.columns, binary.columns);
        for (r, (t, b)) in text.rows.iter().zip(&binary.rows).enumerate() {
            for (c, (tv, bv)) in t.iter().zip(b).enumerate() {
                assert_eq!(tv, bv, "row {} column {}: text vs binary protocol", r + 1, text.columns[c]);
            }
        }

        let row1: Vec<(&str, Value)> = vec![
            ("id", Value::Int(1)),
            ("t", Value::Int(-5)),
            ("ut", Value::UInt(200)),
            ("s", Value::Int(-300)),
            ("m", Value::Int(70000)),
            ("i", Value::Int(i64::from(i32::MIN))),
            ("ui", Value::UInt(u64::from(u32::MAX))),
            ("b", Value::Int(i64::MIN)),
            ("ub", Value::UInt(u64::MAX)),
            ("f", Value::Float(0.1)),
            ("d", Value::Float(f64::MAX)),
            ("dec_", Value::Decimal("-99999999999999.999999".into())),
            ("dt", Value::Date("1000-01-01".into())),
            ("dtm", Value::DateTime("2026-11-26 00:00:00".into())),
            ("dtm6", Value::DateTime("2026-01-01 00:00:00.000001".into())),
            ("ts3", Value::DateTime("2026-11-26 00:00:00.000".into())),
            ("tm", Value::Time("-838:59:59".into())),
            ("tm6", Value::Time("-00:00:00.500000".into())),
            ("yr", Value::UInt(2155)), // YEAR is declared UNSIGNED by the server
            ("ch", v_text("")),
            ("vc", v_text("Việt Nam 😀")),
            ("tx", v_text("long text")),
            ("bn", Value::Bytes(vec![0, 0xFF, 0, 0])),
            ("vb", Value::Bytes(vec![])),
            ("bl", Value::Bytes(vec![0, 1, 2, 0xFF])),
            ("js", Value::Json(r#"{"a": [1, "x"]}"#.into())),
            ("bt", Value::Bytes(vec![0x03, 0xFF])),
            ("en", v_text("b")),
            ("st", v_text("x,z")),
            ("bo", Value::Int(1)),
        ];
        for (col, want) in &row1 {
            assert_eq!(binary.get(0, col), Some(want), "column {col}");
        }
        // Spatial values are WKB with a 4-byte SRID prefix: 4 + 21 bytes for a point.
        let Some(Value::Bytes(geo)) = binary.get(0, "geo") else { panic!("geometry as bytes") };
        assert_eq!(geo.len(), 25);
        assert_eq!(binary.get(0, "bo").and_then(Value::as_bool), Some(true));
        assert_eq!(binary.get(0, "dec_").and_then(Value::as_str), Some("-99999999999999.999999"));

        assert_eq!(binary.get(1, "dtm"), Some(&Value::DateTime("2026-11-26 09:30:05".into())));
        assert_eq!(binary.get(1, "dtm6"), Some(&Value::DateTime("2026-11-26 09:30:05.250000".into())));
        assert_eq!(binary.get(1, "tm6"), Some(&Value::Time("25:00:01.123456".into())));
        assert_eq!(binary.get(1, "ts3"), Some(&Value::DateTime("2026-11-26 09:30:05.120".into())));
        assert_eq!(binary.get(1, "f"), Some(&Value::Float(-2.5)));
        // Row 3: everything but the key is NULL.
        assert!(binary.rows[2][1..].iter().all(Value::is_null));

        // Server expressions, not just columns.
        let r = db
            .query(
                "SELECT ? + 1 AS n, CAST(? AS DECIMAL(10,2)) AS dc, DATE_ADD(?, INTERVAL 1 DAY) AS nd, \
                 JSON_OBJECT('k', ?) AS j, ? IS NULL AS isnull",
                params![41, "3.14159", "2026-02-28", "v", None::<i64>],
            )
            .await
            .unwrap();
        assert_eq!(r.get(0, "n"), Some(&Value::Int(42)));
        assert_eq!(r.get(0, "dc"), Some(&Value::Decimal("3.14".into())));
        assert_eq!(r.get(0, "nd").and_then(Value::as_str), Some("2026-03-01"));
        assert_eq!(r.get(0, "j"), Some(&Value::Json(r#"{"k": "v"}"#.into())));
        assert_eq!(r.get(0, "isnull"), Some(&Value::Int(1)));
    });
}

#[test]
fn bound_parameters_round_trip_every_kind() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_MYSQL_URL not set") };
    run(async move {
        let db = MySqlDriver::connect(&url).await.unwrap();
        db.execute("DROP TABLE IF EXISTS kwr_params").await.unwrap();
        db.execute(
            "CREATE TABLE kwr_params (id INT AUTO_INCREMENT PRIMARY KEY, a BIGINT, b BIGINT UNSIGNED, \
             c DOUBLE, d VARCHAR(100), e VARBINARY(100), f TINYINT(1), g DECIMAL(12,4), h DATETIME(3), k INT)",
        )
        .await
        .unwrap();
        let n = db
            .execute_with(
                "INSERT INTO kwr_params (a, b, c, d, e, f, g, h, k) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![i64::MIN, u64::MAX, -1.5e-300, "a'b\"c\\d; DROP TABLE x", vec![0u8, 0, 255], true, "12.3456", "2026-11-26 10:11:12.345", None::<i32>],
            )
            .await
            .unwrap();
        assert_eq!(n, 1);
        let r = db.query("SELECT a, b, c, d, e, f, g, h, k FROM kwr_params", &[]).await.unwrap();
        assert_eq!(
            r.rows[0],
            vec![
                Value::Int(i64::MIN),
                Value::UInt(u64::MAX),
                Value::Float(-1.5e-300),
                Value::Text("a'b\"c\\d; DROP TABLE x".into()),
                Value::Bytes(vec![0, 0, 255]),
                Value::Int(1),
                Value::Decimal("12.3456".into()),
                Value::DateTime("2026-11-26 10:11:12.345".into()),
                Value::Null,
            ]
        );
        // The generated key of a hand-written INSERT.
        let out = db.query_each("INSERT INTO kwr_params (a) VALUES (?)", params![7], |_| Ok(())).await.unwrap();
        assert_eq!(out.affected_rows, 1);
        assert_eq!(out.last_insert_id, Some(2));
        // Wrong number of parameters is an error, not a desync.
        assert!(db.query("SELECT ? + ?", params![1]).await.is_err());
        assert_eq!(db.query_scalar("SELECT COUNT(*) FROM kwr_params", &[]).await.unwrap(), Value::Int(2));
    });
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "kwr_summary")]
struct CustomerTotal {
    #[id(strategy = "manual")]
    customer: String,
    orders: i64,
    total: f64,
    last_at: Option<String>,
}

#[test]
fn joins_and_aggregates_map_onto_an_entity_by_column_name() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_MYSQL_URL not set") };
    run(async move {
        let db = MySqlDriver::connect(&url).await.unwrap();
        db.execute_script(
            "DROP TABLE IF EXISTS kwr_ord; DROP TABLE IF EXISTS kwr_cust;
             CREATE TABLE kwr_cust (id INT PRIMARY KEY, name VARCHAR(20));
             CREATE TABLE kwr_ord (id INT PRIMARY KEY, cust INT, amount DECIMAL(10,2), at DATETIME);
             INSERT INTO kwr_cust VALUES (1, 'Mai'), (2, 'Lan'), (3, 'Hoa');
             INSERT INTO kwr_ord VALUES (1, 1, 10.50, '2026-11-01 10:00:00'), (2, 1, 4.25, '2026-11-20 12:00:00'),
                                        (3, 2, 100.00, '2026-11-05 09:00:00');",
        )
        .await
        .unwrap();
        let sql = "SELECT c.name AS customer, COUNT(o.id) AS orders, COALESCE(SUM(o.amount), 0) AS total, \
                   MAX(o.at) AS last_at, 'ignored' AS extra \
                   FROM kwr_cust c LEFT JOIN kwr_ord o ON o.cust = c.id \
                   WHERE c.id <= ? GROUP BY c.id, c.name HAVING COUNT(o.id) >= ? ORDER BY total DESC";
        let got: Vec<CustomerTotal> = db.query_as(sql, params![3, 0]).await.unwrap();
        assert_eq!(
            got,
            vec![
                CustomerTotal { customer: "Lan".into(), orders: 1, total: 100.0, last_at: Some("2026-11-05 09:00:00".into()) },
                CustomerTotal { customer: "Mai".into(), orders: 2, total: 14.75, last_at: Some("2026-11-20 12:00:00".into()) },
                CustomerTotal { customer: "Hoa".into(), orders: 0, total: 0.0, last_at: None },
            ]
        );
        // Window function + CTE.
        let r = db
            .query(
                "WITH t AS (SELECT cust, amount, ROW_NUMBER() OVER (PARTITION BY cust ORDER BY amount DESC) AS rn \
                 FROM kwr_ord) SELECT cust, amount FROM t WHERE rn = 1 ORDER BY cust",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(r.rows, vec![vec![Value::Int(1), Value::Decimal("10.50".into())], vec![Value::Int(2), Value::Decimal("100.00".into())]]);
    });
}

#[test]
fn large_results_stream_without_being_collected() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_MYSQL_URL not set") };
    run(async move {
        let db = MySqlDriver::connect(&url).await.unwrap();
        let sql = "WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < ?) SELECT n, REPEAT('x', 100) FROM s";
        db.execute("SET SESSION cte_max_recursion_depth = 1000000").await.ok();
        // Each pooled connection has its own session: run on one via a transaction-free script.
        let (mut count, mut sum) = (0u64, 0i64);
        let res = db
            .query_each(
                "SELECT seq.n, REPEAT('x', 100) FROM (SELECT a.n * 1000 + b.n + 1 AS n FROM \
                 (WITH RECURSIVE s(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM s WHERE n < 199) SELECT n FROM s) a, \
                 (WITH RECURSIVE s(n) AS (SELECT 0 UNION ALL SELECT n + 1 FROM s WHERE n < 999) SELECT n FROM s) b) seq \
                 WHERE seq.n <= ?",
                params![200_000],
                |row| {
                    count += 1;
                    sum += row[0].as_i64().unwrap();
                    Ok(())
                },
            )
            .await
            .unwrap();
        assert_eq!(res.columns.len(), 2);
        assert_eq!(count, 200_000);
        assert_eq!(sum, 200_000 * 200_001 / 2);
        // An error from the callback stops decoding and leaves the connection usable.
        let err = db.query_each("SELECT 1 UNION ALL SELECT 2", &[], |_| Err(OrmError::Query("stop".into()))).await;
        assert!(err.is_err());
        let _ = sql;
        assert_eq!(db.query_scalar("SELECT 5", &[]).await.unwrap(), Value::Int(5));
    });
}

#[test]
fn scripts_run_several_statements_but_ordinary_calls_never_stack() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_MYSQL_URL not set") };
    run(async move {
        let db = MySqlDriver::connect(&url).await.unwrap();
        db.execute_script(
            "DROP TABLE IF EXISTS kwr_script; CREATE TABLE kwr_script (n INT);
             INSERT INTO kwr_script VALUES (1), (2); SELECT * FROM kwr_script; UPDATE kwr_script SET n = n * 10;",
        )
        .await
        .unwrap();
        let r = db.query("SELECT SUM(n) FROM kwr_script", &[]).await.unwrap();
        assert_eq!(r.rows[0][0], Value::Decimal("30".into()));
        // The first failing statement stops the script and is reported.
        let err = db.execute_script("INSERT INTO kwr_script VALUES (5); SELECT * FROM kwr_nope; INSERT INTO kwr_script VALUES (6);").await;
        assert!(err.unwrap_err().to_string().contains("1146"));
        assert_eq!(db.query_scalar("SELECT COUNT(*) FROM kwr_script", &[]).await.unwrap(), Value::Int(3));
        // Outside a script, a stacked statement is a syntax error: injection
        // cannot append a second statement.
        assert!(db.execute("SELECT 1; DROP TABLE kwr_script").await.is_err());
        assert_eq!(db.query_scalar("SELECT COUNT(*) FROM kwr_script", &[]).await.unwrap(), Value::Int(3));
    });
}

#[test]
fn raw_sql_inside_a_transaction_commits_or_rolls_back_with_it() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_MYSQL_URL not set") };
    run(async move {
        let db = MySqlDriver::connect(&url).await.unwrap();
        db.execute_script("DROP TABLE IF EXISTS kwr_tx; CREATE TABLE kwr_tx (id INT PRIMARY KEY, v INT) ENGINE=InnoDB; INSERT INTO kwr_tx VALUES (1, 100);")
            .await
            .unwrap();
        let r: Result<(), OrmError> = db
            .transaction(|tx| {
                Box::pin(async move {
                    let v = tx.query("SELECT v FROM kwr_tx WHERE id = ? FOR UPDATE", params![1]).await?.rows[0][0].as_i64().unwrap();
                    tx.execute_with("UPDATE kwr_tx SET v = ? WHERE id = ?", params![v - 30, 1]).await?;
                    tx.execute_with("INSERT INTO kwr_tx VALUES (?, ?)", params![2, 30]).await?;
                    assert_eq!(tx.query("SELECT COUNT(*) FROM kwr_tx", &[]).await?.rows[0][0], Value::Int(2));
                    Err(OrmError::Query("abort".into()))
                })
            })
            .await;
        assert!(r.is_err());
        assert_eq!(db.query("SELECT v FROM kwr_tx", &[]).await.unwrap().rows, vec![vec![Value::Int(100)]]);
        db.transaction(|tx| {
            Box::pin(async move {
                tx.execute_with("UPDATE kwr_tx SET v = v - ? WHERE id = ?", params![30, 1]).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        assert_eq!(db.query_scalar("SELECT v FROM kwr_tx", &[]).await.unwrap(), Value::Int(70));
    });
}

#[test]
fn load_data_local_infile_sends_only_the_given_bytes() {
    let (Some(url), Ok(root)) = (url(), std::env::var("KERNWAY_MYSQL_ROOT_URL")) else {
        return eprintln!("skip: KERNWAY_MYSQL_URL / KERNWAY_MYSQL_ROOT_URL not set");
    };
    run(async move {
        let admin = MySqlDriver::connect(&root).await.unwrap();
        let was = admin.query_scalar("SELECT @@GLOBAL.local_infile", &[]).await.unwrap();
        admin.execute("SET GLOBAL local_infile = 1").await.unwrap();
        let db = MySqlDriver::connect(&url).await.unwrap();
        db.execute_script("DROP TABLE IF EXISTS kwr_load; CREATE TABLE kwr_load (id INT, name VARCHAR(20), amount DECIMAL(8,2));")
            .await
            .unwrap();
        let mut csv = String::new();
        for i in 1..=50_000 {
            csv.push_str(&format!("{i},name {i},{}.{:02}\n", i % 1000, i % 100));
        }
        let sql = "LOAD DATA LOCAL INFILE '/etc/passwd' INTO TABLE kwr_load FIELDS TERMINATED BY ',' LINES TERMINATED BY '\\n'";
        let loaded = db.load_local_infile(sql, csv.as_bytes()).await.unwrap();
        assert_eq!(loaded, 50_000);
        let r = db.query("SELECT COUNT(*), MAX(id), SUM(id) FROM kwr_load WHERE name LIKE 'name %'", &[]).await.unwrap();
        assert_eq!(r.rows[0][..2], [Value::Int(50_000), Value::Int(50_000)]);
        // The same statement without supplied data: refused (an empty file is
        // sent, never /etc/passwd), and the connection stays usable.
        assert!(db.execute(sql).await.is_err());
        assert_eq!(db.query_scalar("SELECT COUNT(*) FROM kwr_load", &[]).await.unwrap(), Value::Int(50_000));
        admin.execute_with("SET GLOBAL local_infile = ?", &[Param::from(was.as_i64().unwrap_or(0))]).await.unwrap();
    });
}

#[allow(dead_code)]
fn _rows_is_public(_: Rows) {}
