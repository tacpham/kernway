//! PostgreSQL-specific behaviour against a live server set up by
//! `tests/support/pg-setup.sh` (roles for every authentication method, TLS
//! with a test CA).
//!
//! Needs `KERNWAY_PG_URL` and `KERNWAY_PG_TLS_DIR`; without them each test
//! says `skip:` (the CI driver job and the acceptance run fail on that).

#![cfg(feature = "postgres")]

use kernway_orm_core::error::OrmError;
use kernway_orm_postgres::{CallArg, Conn, OutKind, Param, PgConfig, PostgresDriver, QueryResult, Value};
use std::time::{Duration, Instant};

struct Env {
    host: String,
    port: u16,
    tls: String,
}

fn env() -> Option<Env> {
    let Ok(url) = std::env::var("KERNWAY_PG_URL") else {
        eprintln!("skip: KERNWAY_PG_URL not set (tests/support/pg-setup.sh)");
        return None;
    };
    let cfg = PgConfig::from_url(&url).unwrap();
    let (host, port) = match &cfg.hosts[0] {
        kernway_orm_postgres::Host::Tcp(h, p) => (h.clone(), *p),
        other => panic!("KERNWAY_PG_URL must be TCP, not {other:?}"),
    };
    let Ok(tls) = std::env::var("KERNWAY_PG_TLS_DIR") else {
        eprintln!("skip: KERNWAY_PG_TLS_DIR not set (tests/support/pg-setup.sh)");
        return None;
    };
    Some(Env { host, port, tls })
}

impl Env {
    fn url(&self, user: &str, extra: &str) -> String {
        self.url_at(&self.host, user, extra)
    }
    fn url_at(&self, host: &str, user: &str, extra: &str) -> String {
        let pw = match user {
            "kernway" => ":kernway_pw",
            "kw_md5" => ":md5_pw",
            "kw_clear" => ":clear_pw",
            "kw_ssl" => ":ssl_pw",
            _ => "",
        };
        format!("postgres://{user}{pw}@{host}:{}/kernway_test?{extra}", self.port)
    }
    fn ca(&self) -> String {
        format!("sslrootcert={}/ca.crt", self.tls)
    }
    async fn conn(&self) -> Conn {
        Conn::connect(&PgConfig::from_url(&self.url("kernway", "")).unwrap()).await.expect("connect")
    }
}

fn block<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

async fn connect(url: &str) -> Result<Conn, OrmError> {
    Conn::connect(&PgConfig::from_url(url)?).await
}

async fn scalar(c: &mut Conn, sql: &str) -> Value {
    let (_, rows, _) = c.query_params(sql, &[]).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Value::Null)
}

async fn row(c: &mut Conn, sql: &str, params: Vec<Param>) -> Vec<Value> {
    let (_, rows, _) = c.query_params(sql, &params).await.unwrap_or_else(|err| panic!("{sql}: {err}"));
    rows.into_iter().next().unwrap()
}

async fn current_user(c: &mut Conn) -> String {
    scalar(c, "SELECT current_user").await.as_str().unwrap().to_string()
}

#[test]
fn every_authentication_method() {
    let Some(e) = env() else { return };
    block(async move {
        // SCRAM-SHA-256 (the default), over TLS (sslmode=prefer) and plaintext.
        let mut c = connect(&e.url("kernway", "")).await.unwrap();
        assert!(c.is_tls(), "prefer upgrades when the server offers TLS");
        assert_eq!(current_user(&mut c).await, "kernway");
        let mut c = connect(&e.url("kernway", "sslmode=disable")).await.unwrap();
        assert!(!c.is_tls());
        assert_eq!(current_user(&mut c).await, "kernway");
        // MD5, clear-text password, trust.
        for user in ["kw_md5", "kw_clear", "kw_trust"] {
            for mode in ["sslmode=disable", "sslmode=require"] {
                let mut c = connect(&e.url(user, mode)).await.unwrap_or_else(|err| panic!("{user} {mode}: {err}"));
                assert_eq!(current_user(&mut c).await, user);
            }
        }
        // Wrong passwords are refused, for each method.
        for (user, url) in [
            ("kernway", format!("postgres://kernway:nope@{}:{}/kernway_test", e.host, e.port)),
            ("kw_md5", format!("postgres://kw_md5:nope@{}:{}/kernway_test", e.host, e.port)),
            ("kw_clear", format!("postgres://kw_clear:nope@{}:{}/kernway_test", e.host, e.port)),
        ] {
            let err = connect(&url).await.err().unwrap_or_else(|| panic!("{user}: a wrong password was accepted"));
            assert!(err.to_string().contains("28P01"), "{user}: {err}");
        }
        // No password configured but the server asks for one.
        let err = connect(&format!("postgres://kernway@{}:{}/kernway_test", e.host, e.port)).await.err().unwrap();
        assert!(err.to_string().contains("password"), "{err}");
        // A role that may only connect over TLS.
        assert!(connect(&e.url("kw_ssl", "sslmode=disable")).await.is_err());
        let mut c = connect(&e.url("kw_ssl", "sslmode=require")).await.unwrap();
        assert_eq!(current_user(&mut c).await, "kw_ssl");
        // Client-certificate authentication (CN = role name).
        let cert = format!("sslcert={0}/client.crt&sslkey={0}/client.key", e.tls);
        let mut c = connect(&e.url_at("localhost", "kw_cert", &format!("sslmode=verify-full&{}&{cert}", e.ca()))).await.unwrap();
        assert_eq!(current_user(&mut c).await, "kw_cert");
        assert!(connect(&e.url_at("localhost", "kw_cert", &format!("sslmode=verify-full&{}", e.ca()))).await.is_err(), "no certificate");
    });
}

#[test]
fn scram_channel_binding() {
    let Some(e) = env() else { return };
    block(async move {
        // Bound to the TLS session (SCRAM-SHA-256-PLUS).
        let mut c = connect(&e.url("kernway", "sslmode=require&channel_binding=require")).await.unwrap();
        assert!(c.is_tls());
        assert_eq!(current_user(&mut c).await, "kernway");
        // Without TLS there is nothing to bind to; MD5 never binds.
        let err = connect(&e.url("kernway", "sslmode=disable&channel_binding=require")).await.err().unwrap();
        assert!(err.to_string().contains("channel_binding"), "{err}");
        let err = connect(&e.url("kw_md5", "sslmode=require&channel_binding=require")).await.err().unwrap();
        assert!(err.to_string().contains("channel_binding"), "{err}");
        let err = connect(&e.url("kw_trust", "sslmode=require&channel_binding=require")).await.err().unwrap();
        assert!(err.to_string().contains("channel_binding"), "{err}");
        // Explicitly off still authenticates.
        assert!(connect(&e.url("kernway", "sslmode=require&channel_binding=disable")).await.is_ok());
    });
}

#[test]
fn tls_modes_check_what_they_promise() {
    let Some(e) = env() else { return };
    block(async move {
        // The certificate names DNS:localhost only.
        let full = format!("sslmode=verify-full&{}", e.ca());
        assert!(connect(&e.url_at("localhost", "kernway", &full)).await.unwrap().is_tls());
        let err = connect(&e.url_at("127.0.0.1", "kernway", &full)).await.err().unwrap();
        assert!(err.to_string().contains("TLS"), "verify-full checks the name: {err}");
        // verify-ca checks the chain, not the name.
        assert!(connect(&e.url_at("127.0.0.1", "kernway", &format!("sslmode=verify-ca&{}", e.ca()))).await.is_ok());
        // Without our CA the chain does not verify.
        assert!(connect(&e.url_at("localhost", "kernway", "sslmode=verify-full")).await.is_err());
        // require without a root certificate encrypts without checking.
        assert!(connect(&e.url("kernway", "sslmode=require")).await.unwrap().is_tls());
        // A missing root certificate file is a clear error.
        let err = connect(&e.url("kernway", "sslmode=verify-ca&sslrootcert=/no/such/ca.pem")).await.err().unwrap();
        assert!(err.to_string().contains("sslrootcert"), "{err}");
    });
}

#[test]
fn hosts_tried_in_turn_and_session_attrs() {
    let Some(e) = env() else { return };
    block(async move {
        // A refused host first, then the real one.
        let url = format!("postgres://kernway:kernway_pw@127.0.0.1:1,{}:{}/kernway_test?connect_timeout=3", e.host, e.port);
        let mut c = connect(&url).await.unwrap();
        assert_eq!(current_user(&mut c).await, "kernway");
        // Every address of a name (localhost: ::1 and 127.0.0.1).
        assert!(connect(&e.url_at("localhost", "kernway", "")).await.is_ok());
        // The only server is a primary.
        assert!(connect(&e.url("kernway", "target_session_attrs=primary")).await.is_ok());
        assert!(connect(&e.url("kernway", "target_session_attrs=read-write")).await.is_ok());
        let err = connect(&e.url("kernway", "target_session_attrs=standby")).await.err().unwrap();
        assert!(err.to_string().contains("not a Standby"), "{err}");
        assert!(connect(&e.url("kernway", "target_session_attrs=read-only")).await.is_err());
        assert!(connect(&e.url("kernway", "target_session_attrs=prefer-standby")).await.is_ok(), "falls back to any");
        // An address nobody answers: the timeout bounds the attempt.
        let started = Instant::now();
        let err = connect(&e.url_at("10.255.255.1", "kernway", "connect_timeout=1")).await.err().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert!(err.to_string().contains("10.255.255.1"), "{err}");
        // Start-up settings reach the session.
        let mut c = connect(&e.url("kernway", "application_name=kw-test&search_path=pg_catalog&statement_timeout=1234")).await.unwrap();
        assert_eq!(c.parameter("application_name"), Some("kw-test"));
        assert_eq!(scalar(&mut c, "SHOW search_path").await.as_str(), Some("pg_catalog"));
        assert_eq!(scalar(&mut c, "SHOW statement_timeout").await.as_str(), Some("1234ms"));
        assert!(c.server_version_num() >= 140_000, "{}", c.server_version_num());
        assert!(c.process_id() > 0);
    });
}

#[test]
fn values_of_every_common_type() {
    let Some(e) = env() else { return };
    block(async move {
        let mut c = e.conn().await;
        let r = row(
            &mut c,
            "SELECT 1::int2, -2::int4, 9223372036854775807::int8, 0.1::float4, 1e-300::float8, 'NaN'::float8, \
             12345678901234567890.123456789::numeric, true, false, '\\x00ff10'::bytea, 'Việt 😀'::text, \
             'a  '::char(3), '{\"a\": [1, null]}'::jsonb, '{\"b\":1}'::json, '2024-02-29'::date, '23:59:59.999999'::time, \
             '2024-01-02 03:04:05.678'::timestamp, '2024-01-02 03:04:05+00'::timestamptz, '1 day 02:03:04'::interval, \
             'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid, '{1,NULL,3}'::int4[], ARRAY['x y', 'z\"']::text[], \
             '{{1,2},{3,4}}'::int8[], NULL::int4, '192.168.0.1/24'::inet",
            vec![],
        )
        .await;
        use Value::*;
        let want = vec![
            Int(1),
            Int(-2),
            Int(i64::MAX),
            Float(0.1),
            Float(1e-300),
            Float(f64::NAN),
            Decimal("12345678901234567890.123456789".into()),
            Bool(true),
            Bool(false),
            Bytes(vec![0, 255, 16]),
            Text("Việt 😀".into()),
            Text("a  ".into()),
            Json("{\"a\": [1, null]}".into()),
            Json("{\"b\":1}".into()),
            Date("2024-02-29".into()),
            Time("23:59:59.999999".into()),
            DateTime("2024-01-02 03:04:05.678".into()),
            DateTime("2024-01-02 03:04:05 +00:00".into()),
            Interval("1 day 02:03:04".into()),
            Text("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()),
            Array(vec![Int(1), Null, Int(3)]),
            Array(vec![Text("x y".into()), Text("z\"".into())]),
            Array(vec![Array(vec![Int(1), Int(2)]), Array(vec![Int(3), Int(4)])]),
            Null,
            Text("192.168.0.1/24".into()),
        ];
        assert_eq!(r.len(), want.len());
        for (i, (got, want)) in r.iter().zip(&want).enumerate() {
            match (got, want) {
                (Float(a), Float(b)) if a.is_nan() && b.is_nan() => {}
                _ => assert_eq!(got, want, "column {i}"),
            }
        }

        // Parameters of every kind, typed by what the statement expects.
        let r = row(
            &mut c,
            "SELECT $1::int4 + 1, $2::numeric * 2, $3, $4::text, $5::bytea, $6::int4[], $7::text[], $8::jsonb, $9, $10, $11::date, \
             $12::int8 IS NULL, $13",
            vec![
                Param::Int(41),
                Param::Float(1.25),
                Param::Bool(true),
                Param::Str("ü'\"\\".into()),
                Param::Bytes(vec![0, 1, 254]),
                Param::Array(vec![Param::Int(1), Param::Null, Param::Int(-3)]),
                Param::Array(vec![Param::Str("a,b".into()), Param::Str("{}".into())]),
                Param::Array(vec![Param::Int(1), Param::Str("x".into())]),
                Param::Int(7),
                Param::Float(2.5),
                Param::Str("2025-12-31".into()),
                Param::Null,
                Param::UInt(u64::MAX),
            ],
        )
        .await;
        assert_eq!(
            r,
            vec![
                Int(42),
                Decimal("2.50".into()),
                Bool(true),
                Text("ü'\"\\".into()),
                Bytes(vec![0, 1, 254]),
                Array(vec![Int(1), Null, Int(-3)]),
                Array(vec![Text("a,b".into()), Text("{}".into())]),
                Json("[1, \"x\"]".into()),
                Int(7),
                Float(2.5),
                Date("2025-12-31".into()),
                Bool(true),
                Decimal(u64::MAX.to_string()),
            ]
        );
        // Wrong parameter count is an error before anything is sent.
        assert!(c.query_params("SELECT $1, $2", &[Param::Int(1)]).await.is_err());
    });
}

#[test]
fn statement_cache_is_bounded_and_survives_schema_changes() {
    let Some(e) = env() else { return };
    block(async move {
        let mut c = connect(&e.url("kernway", "statement_cache_size=3")).await.unwrap();
        for i in 0..20 {
            let sql = format!("SELECT $1::int8 + {i}");
            let (_, rows, _) = c.query_params(&sql, &[Param::Int(1)]).await.unwrap();
            assert_eq!(rows[0][0], Value::Int(1 + i));
        }
        let n = scalar(&mut c, "SELECT count(*) FROM pg_prepared_statements").await.as_i64().unwrap();
        assert!(n <= 4, "evicted statements are closed on the server: {n} prepared");

        // A cached SELECT * whose table changed shape is re-prepared.
        c.simple("DROP TABLE IF EXISTS kw_shape; CREATE TABLE kw_shape (a int); INSERT INTO kw_shape VALUES (1)").await.unwrap();
        let sel = "SELECT * FROM kw_shape WHERE a >= $1";
        assert_eq!(c.query_params(sel, &[Param::Int(0)]).await.unwrap().0.len(), 1);
        c.simple("ALTER TABLE kw_shape ADD COLUMN b text DEFAULT 'x'").await.unwrap();
        let (cols, rows, _) = c.query_params(sel, &[Param::Int(0)]).await.unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(rows[0], vec![Value::Int(1), Value::Text("x".into())]);

        // No cache at all: every statement unnamed.
        let mut c = connect(&e.url("kernway", "statement_cache_size=0")).await.unwrap();
        for _ in 0..3 {
            assert_eq!(c.query_params("SELECT $1::int4", &[Param::Int(5)]).await.unwrap().1[0][0], Value::Int(5));
        }
        assert_eq!(scalar(&mut c, "SELECT count(*) FROM pg_prepared_statements").await.as_i64(), Some(0));
    });
}

#[test]
fn pipelined_batches_cursors_and_copy() {
    let Some(e) = env() else { return };
    block(async move {
        let db = PostgresDriver::connect(&e.url("kernway", "")).await.unwrap();
        db.execute("DROP TABLE IF EXISTS kw_big").await.unwrap();
        db.execute("CREATE TABLE kw_big (id int PRIMARY KEY, name text, data bytea)").await.unwrap();
        // 5 000 rows in one call: many flights, one Sync.
        let rows: Vec<Vec<Param>> =
            (0..5000).map(|i| vec![Param::Int(i), Param::Str(format!("n{i}")), Param::Bytes(vec![(i % 256) as u8; 64])]).collect();
        let started = Instant::now();
        assert_eq!(db.execute_many("INSERT INTO kw_big VALUES ($1, $2, $3)", &rows).await.unwrap(), 5000);
        eprintln!("5000 pipelined inserts: {:?}", started.elapsed());
        assert_eq!(db.query_scalar("SELECT count(*) FROM kw_big", &[]).await.unwrap().as_i64(), Some(5000));

        // A cursor walks them in batches.
        let mut cur = db.cursor("SELECT id, data FROM kw_big WHERE id >= $1 ORDER BY id", &[Param::Int(0)]).await.unwrap();
        let mut next = 0i64;
        while let Some(b) = cur.next_batch(700).await.unwrap() {
            for r in &b.rows {
                assert_eq!(r[0].as_i64(), Some(next));
                assert_eq!(r[1].as_bytes().unwrap().len(), 64);
                next += 1;
            }
        }
        assert_eq!(next, 5000);

        // COPY out (CSV) and back in.
        let mut csv = Vec::new();
        let n = db
            .copy_out("COPY (SELECT id, name FROM kw_big WHERE id < 1000 ORDER BY id) TO STDOUT (FORMAT csv)", |chunk| {
                csv.extend_from_slice(chunk);
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(n, 1000);
        assert!(csv.starts_with(b"0,n0\n1,n1\n"));
        db.execute("CREATE TEMP TABLE IF NOT EXISTS kw_copy (id int, name text)").await.ok();
        db.execute("DROP TABLE IF EXISTS kw_copy2").await.unwrap();
        db.execute("CREATE TABLE kw_copy2 (id int, name text)").await.unwrap();
        assert_eq!(db.copy_in("COPY kw_copy2 FROM STDIN (FORMAT csv)", &csv).await.unwrap(), 1000);
        assert_eq!(db.query_scalar("SELECT sum(id) FROM kw_copy2", &[]).await.unwrap().as_i64(), Some(499_500));
        // Bad COPY data: an error, and the connection stays usable.
        let err = db.copy_in("COPY kw_copy2 FROM STDIN (FORMAT csv)", b"x,y\n").await.unwrap_err();
        assert!(err.to_string().contains("22P02"), "{err}");
        assert!(db.copy_in("SELECT 1", b"").await.is_err(), "not a COPY");
        assert_eq!(db.query_scalar("SELECT count(*) FROM kw_copy2", &[]).await.unwrap().as_i64(), Some(1000));
    });
}

#[test]
fn listen_notify() {
    let Some(e) = env() else { return };
    block(async move {
        let db = PostgresDriver::connect(&e.url("kernway", "")).await.unwrap();
        let mut l = db.listener().await.unwrap();
        l.listen("kw events").await.unwrap();
        // Nothing yet: a bounded wait times out, and the listener still works.
        assert!(rt_core::timeout(Duration::from_millis(200), l.recv()).await.is_err());
        db.notify("kw events", "hello ✓").await.unwrap();
        db.notify("other", "ignored").await.unwrap();
        let n = rt_core::timeout(Duration::from_secs(5), l.recv()).await.expect("notification").unwrap();
        assert_eq!((n.channel.as_str(), n.payload.as_str()), ("kw events", "hello ✓"));
        // Notifications queue up while the connection runs statements.
        db.notify("kw events", "1").await.unwrap();
        db.notify("kw events", "2").await.unwrap();
        rt_core::sleep(Duration::from_millis(200)).await;
        l.conn().simple("SELECT 1").await.unwrap();
        let got: Vec<String> = l.try_recv().into_iter().map(|n| n.payload).collect();
        assert_eq!(got, ["1", "2"]);
        l.unlisten("kw events").await.unwrap();
        db.notify("kw events", "late").await.unwrap();
        assert!(rt_core::timeout(Duration::from_millis(300), l.recv()).await.is_err());
    });
}

#[test]
fn cancel_a_running_statement() {
    let Some(e) = env() else { return };
    block(async move {
        let mut c = e.conn().await;
        let token = c.cancel_token();
        let pid = c.process_id();
        let mut watcher = e.conn().await;
        let cancelled = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = cancelled.clone();
        rt_core::spawn(async move {
            // Cancel once the statement is running (a cancel that arrives
            // before it does nothing).
            for _ in 0..200 {
                let (_, rows, _) = watcher
                    .query_params(
                        "SELECT count(*) FROM pg_stat_activity WHERE pid = $1 AND state = 'active' AND query LIKE '%pg_sleep%'",
                        &[Param::Int(i64::from(pid))],
                    )
                    .await
                    .unwrap();
                if rows[0][0].as_i64() == Some(1) {
                    break;
                }
                rt_core::sleep(Duration::from_millis(50)).await;
            }
            token.cancel().await.unwrap();
            flag.set(true);
        });
        let started = Instant::now();
        let err = c.query_params("SELECT pg_sleep(30)", &[]).await.unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(err.to_string().contains("57014"), "{err}");
        assert_eq!(c.last_error().unwrap().code, "57014");
        // The canceller finishes once the server has closed its connection.
        let waited = Instant::now();
        while !cancelled.get() && waited.elapsed() < Duration::from_secs(5) {
            rt_core::sleep(Duration::from_millis(10)).await;
        }
        assert!(cancelled.get());
        // The connection is fine afterwards.
        assert_eq!(scalar(&mut c, "SELECT 7").await, Value::Int(7));
    });
}

#[test]
fn notices_errors_scripts_and_transactions() {
    let Some(e) = env() else { return };
    block(async move {
        let mut c = e.conn().await;
        c.simple("DO $$ BEGIN RAISE NOTICE 'hello %', 42; RAISE WARNING 'careful'; END $$").await.unwrap();
        let notices = c.take_notices();
        assert_eq!(notices.iter().map(|n| (n.severity.as_str(), n.message.as_str())).collect::<Vec<_>>(), [("NOTICE", "hello 42"), ("WARNING", "careful")]);

        // Errors carry the server's detail.
        let err = c.query_params("SELECT * FROM no_such_table_kw", &[]).await.unwrap_err();
        assert!(err.to_string().contains("42P01"), "{err}");
        assert_eq!(c.last_error().unwrap().position, Some(15));
        c.simple("DROP TABLE IF EXISTS kw_child; DROP TABLE IF EXISTS kw_parent; \
                  CREATE TABLE kw_parent (id int PRIMARY KEY); CREATE TABLE kw_child (p int REFERENCES kw_parent)")
            .await
            .unwrap();
        let err = c.query_params("INSERT INTO kw_child VALUES ($1)", &[Param::Int(1)]).await.unwrap_err();
        assert!(matches!(err, OrmError::ForeignKeyViolation), "{err:?}");
        assert_eq!(c.last_error().unwrap().constraint.as_deref(), Some("kw_child_p_fkey"));

        // A script stops at its first error, as one implicit transaction.
        let err = c.simple("INSERT INTO kw_parent VALUES (1); INSERT INTO kw_parent VALUES (1); INSERT INTO kw_parent VALUES (2)").await.unwrap_err();
        assert!(matches!(err, OrmError::UniqueViolation { ref field } if field == "id"), "{err:?}");
        assert_eq!(scalar(&mut c, "SELECT count(*) FROM kw_parent").await.as_i64(), Some(0));
        let rs = c.query("SELECT 1 AS a; UPDATE kw_parent SET id = id; SELECT 'x' AS b, 2.5::float8").await.unwrap();
        assert!(matches!(&rs[0], QueryResult::Rows { columns, rows, .. } if columns[0].name == "a" && rows[0] == [Value::Int(1)]));
        assert!(matches!(&rs[1], QueryResult::Ok { tag, affected: 0 } if tag == "UPDATE 0"));
        assert!(matches!(&rs[2], QueryResult::Rows { rows, .. } if rows[0] == [Value::Text("x".into()), Value::Float(2.5)]));

        // Manual transactions on a plain connection; a failed one cannot commit.
        use kernway_orm_sql::SqlConn;
        c.begin().await.unwrap();
        assert!(c.in_transaction());
        c.query_params("INSERT INTO kw_parent VALUES ($1)", &[Param::Int(10)]).await.unwrap();
        c.rollback().await.unwrap();
        assert!(!c.in_transaction());
        c.begin().await.unwrap();
        assert!(c.query_params("SELECT 1/0", &[]).await.is_err());
        assert!(c.transaction_failed());
        assert!(c.query_params("SELECT 1", &[]).await.is_err(), "refused until ROLLBACK");
        assert!(matches!(c.commit().await, Err(OrmError::Transaction(_))));
        assert!(!c.in_transaction());
        assert_eq!(scalar(&mut c, "SELECT count(*) FROM kw_parent").await.as_i64(), Some(0));
    });
}

#[test]
fn procedures_and_functions() {
    let Some(e) = env() else { return };
    block(async move {
        let db = PostgresDriver::connect(&e.url("kernway", "")).await.unwrap();
        db.execute_script(
            "CREATE OR REPLACE PROCEDURE kw_add(a int, INOUT total int, OUT doubled int) LANGUAGE plpgsql AS $$ \
               BEGIN RAISE NOTICE 'adding %', a; total := total + a; doubled := total * 2; END $$; \
             CREATE OR REPLACE FUNCTION kw_greet(name text, punct text DEFAULT '!') RETURNS text LANGUAGE sql AS \
               $$ SELECT 'hi ' || name || punct $$",
        )
        .await
        .unwrap();
        let r = db.call("kw_add", &[CallArg::In(Param::Int(5)), CallArg::InOut(Param::Int(10)), CallArg::Out(OutKind::Number)]).await.unwrap();
        assert_eq!(r.out, vec![Value::Int(15), Value::Int(30)]);
        assert_eq!(r.output, ["adding 5"]);
        let r = db.call_function("kw_greet", OutKind::Text, &[CallArg::In(Param::Str("An".into()))]).await.unwrap();
        assert_eq!(r.value.as_str(), Some("hi An!"));
        let r = db
            .call_function("kw_greet", OutKind::Text, &[CallArg::named("punct", Param::Str("?".into())), CallArg::named("name", Param::Str("Bo".into()))])
            .await
            .unwrap();
        assert_eq!(r.value.as_str(), Some("hi Bo?"));
        assert!(db.call_function("kw_greet", OutKind::Text, &[CallArg::Out(OutKind::Text)]).await.is_err());
        assert!(db.call("kw_add; DROP TABLE x", &[]).await.is_err());
    });
}

#[test]
fn host_names_resolve_off_the_reactor() {
    let Some(e) = env() else { return };
    block(async move {
        // A ticker keeps running while a name is resolved and connected.
        let ticks = std::rc::Rc::new(std::cell::Cell::new(0));
        let t = ticks.clone();
        rt_core::spawn(async move {
            for _ in 0..5 {
                rt_core::sleep(Duration::from_millis(1)).await;
                t.set(t.get() + 1);
            }
        });
        let mut c = connect(&e.url_at("localhost", "kernway", "")).await.unwrap();
        assert_eq!(current_user(&mut c).await, "kernway");
        let started = Instant::now();
        while ticks.get() < 5 && started.elapsed() < Duration::from_secs(2) {
            rt_core::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(ticks.get(), 5);
        // An unknown name is a clear error.
        let err = connect(&e.url_at("no-such-host.invalid", "kernway", "connect_timeout=5")).await.err().unwrap();
        assert!(err.to_string().contains("no-such-host.invalid"), "{err}");
    });
}

#[test]
fn unix_domain_socket() {
    // pg-setup.sh shares the socket directory: a native server's own on
    // macOS, the container's bind-mounted on Linux. Windows has no Unix
    // sockets in rt-net (TCP only there).
    if cfg!(windows) {
        eprintln!("n/a: no Unix-domain sockets on Windows");
        return;
    }
    let Ok(dir) = std::env::var("KERNWAY_PG_SOCKET_DIR") else {
        eprintln!("skip: KERNWAY_PG_SOCKET_DIR not set (pg-setup.sh with PG_SOCKET_DIR or PG_BIN)");
        return;
    };
    let port = std::env::var("KERNWAY_PG_SOCKET_PORT").unwrap_or_else(|_| "5432".into());
    block(async move {
        let url = format!("postgres://kernway@/kernway_test?host={dir}&port={port}");
        let mut c = connect(&url).await.unwrap_or_else(|err| panic!("{url}: {err}"));
        assert!(!c.is_tls());
        assert_eq!(current_user(&mut c).await, "kernway");
        assert_eq!(scalar(&mut c, "SELECT inet_client_addr() IS NULL").await, Value::Bool(true), "a local connection");
    });
}
