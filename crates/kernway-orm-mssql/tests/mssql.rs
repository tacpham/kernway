//! SQL Server-specific behaviour against a live server set up by
//! `tests/support/mssql-setup.sh`: encryption modes and certificate checks,
//! logins, every common type, large values, typed NULLs, command timeouts
//! (ATTENTION), procedures with OUTPUT parameters, GO scripts, messages,
//! typed errors, transactions, API cursors, pipelined batches.

#![cfg(feature = "mssql")]

use kernway_orm_core::error::OrmError;
use kernway_orm_mssql::{CallArg, Conn, MssqlConfig, MssqlDriver, OutKind, Param, Value};
use kernway_orm_sql::SqlConn;
use std::time::{Duration, Instant};

fn env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) => Some(v),
        Err(_) => {
            eprintln!("skip: {name} not set (tests/support/mssql-setup.sh)");
            None
        }
    }
}

fn block<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

async fn connect(url: &str) -> Result<Conn, OrmError> {
    Conn::connect(&MssqlConfig::parse(url)?).await
}

async fn scalar(c: &mut Conn, sql: &str) -> Value {
    let (_, rows, _) = c.query_params(sql, &[]).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null)
}

async fn row(c: &mut Conn, sql: &str, params: &[Param]) -> Vec<Value> {
    let (_, rows, _) = c.query_params(sql, params).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    rows.into_iter().next().unwrap()
}

#[test]
fn encryption_modes_and_certificates() {
    let Some(dir) = env("KERNWAY_MSSQL_TLS_DIR") else { return };
    block(async move {
        let base = "mssql://kw_app:Kw_app_pw1%21@";
        // Full TLS inside PRELOGIN, checked against our CA: the certificate
        // names localhost.
        let ca = format!("caFile={dir}/ca.pem");
        let mut c = connect(&format!("{base}localhost:1433/kernway_test?encrypt=true&{ca}")).await.unwrap();
        assert!(c.is_tls());
        assert_eq!(scalar(&mut c, "SELECT encrypt_option FROM sys.dm_exec_connections WHERE session_id = @@SPID").await, Value::Text("TRUE".into()));
        let err = connect(&format!("{base}127.0.0.1:1433/kernway_test?encrypt=true&{ca}")).await.err().unwrap();
        assert!(err.to_string().contains("TLS"), "the name is checked: {err}");
        assert!(connect(&format!("{base}127.0.0.1:1433/kernway_test?encrypt=true&{ca}&hostNameInCertificate=localhost")).await.is_ok());
        assert!(connect(&format!("{base}localhost:1433/kernway_test?encrypt=true")).await.is_err(), "system roots do not know our CA");
        assert!(connect(&format!("{base}127.0.0.1:1433/kernway_test?encrypt=true&trustServerCertificate=true")).await.is_ok());
        // Login-only encryption: the session itself is plaintext.
        let mut c = connect(&format!("{base}127.0.0.1:1433/kernway_test?encrypt=false&trustServerCertificate=true")).await.unwrap();
        assert!(!c.is_tls());
        assert_eq!(scalar(&mut c, "SELECT encrypt_option FROM sys.dm_exec_connections WHERE session_id = @@SPID").await, Value::Text("FALSE".into()));
        // No TLS at all.
        let mut c = connect(&format!("{base}127.0.0.1:1433/kernway_test?encrypt=none")).await.unwrap();
        assert_eq!(scalar(&mut c, "SELECT 1").await, Value::Int(1));
        // Bad credentials and a database the login cannot open.
        let err = connect("mssql://kw_app:nope@127.0.0.1:1433/kernway_test?trustServerCertificate=true").await.err().unwrap();
        assert!(err.to_string().contains("18456"), "{err}");
        let err = connect(&format!("{base}127.0.0.1:1433/no_such_db?trustServerCertificate=true")).await.err().unwrap();
        assert!(err.to_string().contains("4060") || err.to_string().contains("no_such_db"), "{err}");
        // ADO.NET connection strings.
        let ado = "Server=tcp:127.0.0.1,1433;Database=kernway_test;User Id=kw_app;Password=\"Kw_app_pw1!\";TrustServerCertificate=true;Application Name=kw-ado";
        let mut c = connect(ado).await.unwrap();
        assert_eq!(scalar(&mut c, "SELECT APP_NAME()").await, Value::Text("kw-ado".into()));
        assert_eq!(c.database(), "kernway_test");
    });
}

#[test]
fn values_of_every_common_type() {
    let Some(url) = env("KERNWAY_MSSQL_URL") else { return };
    block(async move {
        let mut c = connect(&url).await.unwrap();
        let r = row(
            &mut c,
            "SELECT CAST(1 AS tinyint), CAST(-2 AS smallint), CAST(-3 AS int), CAST(9223372036854775807 AS bigint), CAST(1 AS bit), \
             CAST(0.1 AS real), CAST(1e-300 AS float), CAST(-12345678901234567890.123456789 AS decimal(38,9)), CAST(12.3456 AS money), \
             CAST(-1.5 AS smallmoney), CAST('2024-02-29' AS date), CAST('23:59:59.1234567' AS time(7)), \
             CAST('2024-01-02 03:04:05.1234567' AS datetime2(7)), CAST('2024-01-02 03:04:05.123' AS datetime), \
             CAST('2024-01-02 03:04:00' AS smalldatetime), CAST('2024-01-02 03:04:05 +07:00' AS datetimeoffset(0)), \
             CAST('6F9619FF-8B86-D011-B42D-00C04FC964FF' AS uniqueidentifier), N'Việt 😀', CAST('café €' AS varchar(20)), \
             CAST('ab' AS char(4)), 0x00FF10, CAST(N'<a>1</a>' AS xml), CAST(NULL AS int), CAST(42 AS sql_variant), \
             CAST(REPLICATE(CAST(N'x' AS nvarchar(max)), 10000) AS nvarchar(max)), CAST(REPLICATE(CAST(0x01 AS varbinary(max)), 9000) AS varbinary(max))",
            &[],
        )
        .await;
        use Value::*;
        let expect = [
            Int(1),
            Int(-2),
            Int(-3),
            Int(i64::MAX),
            Bool(true),
            Float(0.1),
            Float(1e-300),
            Decimal("-12345678901234567890.123456789".into()),
            Decimal("12.3456".into()),
            Decimal("-1.5000".into()),
            Date("2024-02-29".into()),
            Time("23:59:59.1234567".into()),
            DateTime("2024-01-02 03:04:05.1234567".into()),
            DateTime("2024-01-02 03:04:05.123".into()),
            DateTime("2024-01-02 03:04:00".into()),
            DateTime("2024-01-02 03:04:05 +07:00".into()),
            Text("6F9619FF-8B86-D011-B42D-00C04FC964FF".into()),
            Text("Việt 😀".into()),
            Text("café €".into()),
            Text("ab  ".into()),
            Bytes(vec![0, 255, 16]),
            Text("<a>1</a>".into()),
            Null,
            Int(42),
        ];
        for (i, want) in expect.iter().enumerate() {
            assert_eq!(&r[i], want, "column {i}");
        }
        assert_eq!(r[24].as_str().map(str::len), Some(10_000), "nvarchar(max) in PLP chunks");
        assert_eq!(r[25].as_bytes().map(<[u8]>::len), Some(9000));

        // Parameters of every kind, typed for the server.
        let r = row(
            &mut c,
            "SELECT @P1 + 1, @P2 * 2, @P3, @P4, DATALENGTH(@P5), @P6, CAST(@P7 AS date), @P8, @P9, LEN(@P10), DATALENGTH(@P11)",
            &[
                Param::Int(41),
                Param::Float(1.25),
                Param::Bool(true),
                Param::Str("ü'\"\\ 😀".into()),
                Param::Bytes(vec![1, 2, 3]),
                Param::UInt(u64::MAX),
                Param::Str("2025-12-31".into()),
                Param::Null,
                Param::Array(vec![Param::Int(1), Param::Str("x".into())]),
                Param::Str("y".repeat(70_000)),
                Param::Bytes(vec![9; 100_000]),
            ],
        )
        .await;
        assert_eq!(
            r,
            vec![
                Int(42),
                Float(2.5),
                Bool(true),
                Text("ü'\"\\ 😀".into()),
                Int(3),
                Decimal(u64::MAX.to_string()),
                Date("2025-12-31".into()),
                Null,
                Text("[1,\"x\"]".into()),
                Int(70_000),
                Int(100_000),
            ]
        );
        // A NULL bound for a varbinary column must travel as binary.
        c.simple("IF OBJECT_ID('kw_bin') IS NOT NULL DROP TABLE kw_bin; CREATE TABLE kw_bin (id int, b varbinary(max))").await.unwrap();
        c.execute("INSERT INTO kw_bin VALUES (@P1, @P2)", &[Param::Int(1), Param::Null]).await.unwrap();
        c.execute("INSERT INTO kw_bin VALUES (@P1, @P2)", &[Param::Int(2), Param::Bytes(vec![7])]).await.unwrap();
        assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM kw_bin WHERE b IS NULL").await, Int(1));
        // Infinite floats do not exist in SQL Server: a clear error.
        assert!(c.query_params("SELECT @P1", &[Param::Float(f64::INFINITY)]).await.is_err());
    });
}

#[test]
fn command_timeouts_cancel_and_keep_the_connection() {
    let Some(url) = env("KERNWAY_MSSQL_URL") else { return };
    block(async move {
        let mut c = connect(&format!("{url}&commandTimeout=1")).await.unwrap();
        let started = Instant::now();
        let err = c.query_params("WAITFOR DELAY '00:00:20'; SELECT 1", &[]).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(8), "{:?}", started.elapsed());
        assert!(err.to_string().contains("cancelled"), "{err}");
        // The connection is in step afterwards.
        assert_eq!(scalar(&mut c, "SELECT 7").await, Value::Int(7));
        // A long result cut by the timeout, then reused.
        c.set_command_timeout(Some(Duration::from_millis(300)));
        let err = c.query_params("SELECT a.name FROM sys.all_objects a CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c", &[]).await;
        assert!(err.is_err());
        c.set_command_timeout(None);
        assert_eq!(scalar(&mut c, "SELECT 8").await, Value::Int(8));
    });
}

#[test]
fn procedures_scripts_messages_and_errors() {
    let Some(url) = env("KERNWAY_MSSQL_URL") else { return };
    block(async move {
        let db = MssqlDriver::connect(&url).await.unwrap();
        db.execute_script(
            "IF OBJECT_ID('kw_add') IS NOT NULL DROP PROCEDURE kw_add\nGO\n\
             CREATE PROCEDURE kw_add @a int, @total int OUTPUT, @doubled int OUTPUT, @label nvarchar(50) OUTPUT AS\n\
             BEGIN PRINT 'adding ' + CAST(@a AS varchar(10)); SET @total = @total + @a; SET @doubled = @total * 2; SET @label = N'ok ✓';\n\
             SELECT @a AS a, N'set' AS kind; RETURN 5; END\nGO\n\
             IF OBJECT_ID('kw_greet') IS NOT NULL DROP FUNCTION kw_greet\nGO\n\
             CREATE FUNCTION kw_greet (@name nvarchar(50)) RETURNS nvarchar(60) AS BEGIN RETURN N'hi ' + @name END\nGO",
        )
        .await
        .unwrap();
        let r = db
            .call("kw_add", &[CallArg::In(Param::Int(5)), CallArg::InOut(Param::Int(10)), CallArg::Out(OutKind::Number), CallArg::Out(OutKind::Text)])
            .await
            .unwrap();
        assert_eq!(r.out.len(), 3);
        assert_eq!(r.out[0].as_i64(), Some(15));
        assert_eq!(r.out[1].as_i64(), Some(30));
        assert_eq!(r.out[2].as_str(), Some("ok ✓"));
        assert_eq!(r.value, Value::Int(5), "the RETURN status");
        assert_eq!(r.output, ["adding 5"]);
        assert_eq!(r.results.len(), 1);
        assert_eq!(r.results[0].rows[0][1].as_str(), Some("set"));
        // Named arguments.
        let r = db
            .call(
                "dbo.kw_add",
                &[
                    CallArg::named("label", CallArg::Out(OutKind::Text)),
                    CallArg::named("total", CallArg::InOut(Param::Int(1))),
                    CallArg::named("a", Param::Int(2)),
                    CallArg::named("doubled", CallArg::Out(OutKind::Number)),
                ],
            )
            .await
            .unwrap();
        assert!(r.out.iter().any(|v| v.as_i64() == Some(3)), "{:?}", r.out);
        let f = db.call_function("kw_greet", OutKind::Text, &[CallArg::In(Param::Str("An".into()))]).await.unwrap();
        assert_eq!(f.value.as_str(), Some("hi An"));
        assert!(db.call("kw_add; DROP TABLE x", &[]).await.is_err());

        // Typed errors with the server's detail.
        let mut c = connect(&url).await.unwrap();
        c.simple(
            "IF OBJECT_ID('kw_child') IS NOT NULL DROP TABLE kw_child; IF OBJECT_ID('kw_parent') IS NOT NULL DROP TABLE kw_parent;\
             CREATE TABLE kw_parent (id int PRIMARY KEY, email nvarchar(100) CONSTRAINT kw_parent_email UNIQUE);\
             CREATE TABLE kw_child (p int REFERENCES kw_parent(id))",
        )
        .await
        .unwrap();
        c.execute("INSERT INTO kw_parent VALUES (@P1, @P2)", &[Param::Int(1), Param::Str("a@x".into())]).await.unwrap();
        let err = c.execute("INSERT INTO kw_parent VALUES (@P1, @P2)", &[Param::Int(2), Param::Str("a@x".into())]).await.unwrap_err();
        assert!(matches!(&err, OrmError::UniqueViolation { field } if field == "email"), "{err:?}");
        let err = c.execute("INSERT INTO kw_child VALUES (@P1)", &[Param::Int(9)]).await.unwrap_err();
        assert!(matches!(err, OrmError::ForeignKeyViolation), "{err:?}");
        assert_eq!(c.last_error().unwrap().number, 547);
        let err = c.query_params("SELECT * FROM no_such_table_kw", &[]).await.unwrap_err();
        assert!(err.to_string().contains("208"), "{err}");
        assert!(c.last_error().unwrap().line >= 1);
        // RAISERROR with a low severity is a message, not an error.
        c.simple("RAISERROR('just saying %d', 0, 1, 42) WITH NOWAIT").await.unwrap();
        assert_eq!(c.take_messages().last().map(|m| m.message.clone()), Some("just saying 42".into()));

        // A script stops at its first failing batch.
        let err = db.execute_script("SELECT 1\nGO\nSELECT * FROM nope_kw\nGO\nSELECT 3").await.unwrap_err();
        assert!(err.to_string().contains("batch 2"), "{err}");
    });
}

#[test]
fn transactions_cursors_and_batches() {
    let Some(url) = env("KERNWAY_MSSQL_URL") else { return };
    block(async move {
        let mut c = connect(&url).await.unwrap();
        c.simple("IF OBJECT_ID('kw_t') IS NOT NULL DROP TABLE kw_t; CREATE TABLE kw_t (id int PRIMARY KEY, v nvarchar(20))").await.unwrap();
        // Manual transactions; a rolled-back one leaves nothing.
        c.begin().await.unwrap();
        assert!(c.in_transaction());
        c.execute("INSERT INTO kw_t VALUES (@P1, @P2)", &[Param::Int(1), Param::Str("a".into())]).await.unwrap();
        c.rollback().await.unwrap();
        assert!(!c.in_transaction());
        assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM kw_t").await, Value::Int(0));
        // An error that rolls the transaction back (XACT_ABORT) cannot commit.
        c.begin().await.unwrap();
        c.simple("SET XACT_ABORT ON").await.unwrap();
        assert!(c.execute("INSERT INTO kw_t VALUES (@P1, @P2)", &[Param::Str("x".into()), Param::Null]).await.is_err());
        assert!(matches!(c.commit().await, Err(OrmError::Transaction(_))));
        c.simple("SET XACT_ABORT OFF").await.unwrap();
        // 3 000 pipelined rows, then a failure in the middle: rows before it stay.
        let rows: Vec<Vec<Param>> = (0..3000).map(|i| vec![Param::Int(i), Param::Str(format!("v{i}"))]).collect();
        let started = Instant::now();
        assert_eq!(c.execute_many("INSERT INTO kw_t VALUES (@P1, @P2)", &rows).await.unwrap(), 3000);
        eprintln!("3000 pipelined inserts: {:?}", started.elapsed());
        let mut more: Vec<Vec<Param>> = (5000..5010).map(|i| vec![Param::Int(i), Param::Null]).collect();
        more[4] = vec![Param::Int(1), Param::Null];
        let err = c.execute_many("INSERT INTO kw_t VALUES (@P1, @P2)", &more).await.unwrap_err();
        assert!(matches!(err, OrmError::UniqueViolation { .. }), "{err:?}");
        assert_eq!(scalar(&mut c, "SELECT COUNT(*) FROM kw_t").await, Value::Int(3004));
        // A cursor over them, other statements in between.
        let mut cur = c.cursor_open("SELECT id, v FROM kw_t WHERE id < @P1 ORDER BY id", &[Param::Int(2000)]).await.unwrap();
        assert_eq!(cur.columns(), ["id", "v"]);
        let mut seen = 0;
        loop {
            let batch = c.cursor_fetch(&mut cur, 300).await.unwrap();
            seen += batch.len();
            assert_eq!(scalar(&mut c, "SELECT 1").await, Value::Int(1));
            if cur.is_done() {
                break;
            }
        }
        assert_eq!(seen, 2000);
    });
}

#[test]
fn host_names_resolve_off_the_reactor() {
    let Some(url) = env("KERNWAY_MSSQL_URL") else { return };
    block(async move {
        let ticks = std::rc::Rc::new(std::cell::Cell::new(0));
        let t = ticks.clone();
        rt_core::spawn(async move {
            for _ in 0..5 {
                rt_core::sleep(Duration::from_millis(1)).await;
                t.set(t.get() + 1);
            }
        });
        let mut c = connect(&url.replace("127.0.0.1", "localhost")).await.unwrap();
        assert_eq!(scalar(&mut c, "SELECT 1").await, Value::Int(1));
        let started = Instant::now();
        while ticks.get() < 5 && started.elapsed() < Duration::from_secs(2) {
            rt_core::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(ticks.get(), 5);
        let err = connect("mssql://sa:x@no-such-host.invalid:1433/?connectTimeout=5").await.err().unwrap();
        assert!(err.to_string().contains("no-such-host.invalid"), "{err}");
        let started = Instant::now();
        assert!(connect("mssql://sa:x@10.255.255.1:1433/?connectTimeout=1").await.is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    });
}

#[test]
fn strict_tds8() {
    let Some(url) = env("KERNWAY_MSSQL_STRICT_URL") else { return };
    block(async move {
        // TLS before the first TDS byte (ALPN tds/8.0, TLS 1.3), the certificate checked.
        let mut c = connect(&url).await.unwrap();
        assert!(c.is_tls());
        let v = scalar(&mut c, "SELECT protocol_version FROM sys.dm_exec_connections WHERE session_id = @@SPID").await;
        assert_eq!(v.as_i64().map(|v| (v as u32) >> 24), Some(0x08), "{v:?}");
        assert_eq!(scalar(&mut c, "SELECT encrypt_option FROM sys.dm_exec_connections WHERE session_id = @@SPID").await, Value::Text("TRUE".into()));
        assert!(connect(&url.replace("localhost", "127.0.0.1")).await.is_err(), "the name is checked");
        // A TDS 7.x client cannot reach a TLS 1.3-only server: a clear error.
        assert!(connect(&url.replace("encrypt=strict", "encrypt=true")).await.is_err());
    });
}
