//! What only SQLite has, or does its own way, against real database files:
//! in-memory databases shared by the pool, read-only mode, WAL, foreign
//! keys, lock waiting that never stalls the reactor, value typing by
//! declared column type, FTS5, JSON, Rust functions callable from SQL,
//! online backup, streaming cursors, and files released on drop.

use kernway_orm_core::{error::OrmError, transaction::TransactionalDriver};
use kernway_orm_sql::{params, CallArg, OutKind, Param, Value};
use kernway_orm_sqlite::{JournalMode, SqliteConfig, SqliteDriver};
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

fn run<F: std::future::Future>(f: F) -> F::Output {
    rt_core::Executor::new().unwrap().block_on(f).unwrap()
}

/// A fresh directory per test, removed afterwards.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("kw-sqlite-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
    fn file(&self, f: &str) -> PathBuf {
        self.0.join(f)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn scalar(db: &SqliteDriver, sql: &str) -> Value {
    db.query_scalar(sql, &[]).await.unwrap()
}

#[test]
fn an_in_memory_database_is_shared_by_the_pool_and_private_to_its_driver() {
    run(async {
        let db = SqliteDriver::with_config(SqliteConfig::memory().pool_size(4)).await.unwrap();
        db.execute("CREATE TABLE t (n INTEGER)").await.unwrap();
        // Hold several connections at once: every one sees the same table.
        let rows: Vec<Vec<Param>> = (0..8).map(|i| vec![Param::Int(i)]).collect();
        let writes: Vec<_> = rows.iter().map(|r| db.execute_with("INSERT INTO t VALUES (?)", r)).collect();
        for w in writes {
            w.await.unwrap();
        }
        assert_eq!(scalar(&db, "SELECT COUNT(*) FROM t").await.as_i64(), Some(8));

        let other = SqliteDriver::open_in_memory().await.unwrap();
        assert!(other.query("SELECT * FROM t", &[]).await.is_err(), "another driver has its own database");
    });
}

#[test]
fn a_file_database_persists_uses_wal_and_enforces_foreign_keys() {
    let dir = Dir::new("file");
    let path = dir.file("app.db");
    run(async {
        let db = SqliteDriver::open(&path).await.unwrap();
        assert_eq!(scalar(&db, "PRAGMA journal_mode").await.as_str(), Some("wal"));
        assert_eq!(scalar(&db, "PRAGMA foreign_keys").await.as_i64(), Some(1));
        db.execute_script(
            "CREATE TABLE parent (id INTEGER PRIMARY KEY);
             CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id));
             INSERT INTO parent VALUES (1);",
        )
        .await
        .unwrap();
        db.execute("INSERT INTO child VALUES (1, 1)").await.unwrap();
        let e = db.execute("INSERT INTO child VALUES (2, 99)").await.unwrap_err();
        assert!(matches!(e, OrmError::ForeignKeyViolation), "{e:?}");
        let e = db.execute("DELETE FROM parent WHERE id = 1").await.unwrap_err();
        assert!(matches!(e, OrmError::ForeignKeyViolation), "still referenced: {e:?}");
    });
    // A second driver (a restart) sees the data.
    run(async {
        let db = SqliteDriver::open(&path).await.unwrap();
        assert_eq!(scalar(&db, "SELECT COUNT(*) FROM child").await.as_i64(), Some(1));
    });
    // Foreign keys can be turned off.
    run(async {
        let db = SqliteDriver::connect(&format!("sqlite:{}?foreign_keys=off", path.display())).await.unwrap();
        db.execute("INSERT INTO child VALUES (3, 42)").await.unwrap();
    });
}

#[test]
fn journal_mode_and_pragmas_are_applied_to_every_connection() {
    let dir = Dir::new("pragma");
    run(async {
        let cfg = SqliteConfig::file(dir.file("d.db")).journal_mode(JournalMode::Delete).pragma("cache_size", "-4000").pool_size(3);
        let db = SqliteDriver::with_config(cfg).await.unwrap();
        // Several connections at once, each asked separately.
        let (a, b, c) = (
            db.query_scalar("PRAGMA cache_size", &[]),
            db.query_scalar("PRAGMA cache_size", &[]),
            db.query_scalar("PRAGMA journal_mode", &[]),
        );
        assert_eq!(a.await.unwrap().as_i64(), Some(-4000));
        assert_eq!(b.await.unwrap().as_i64(), Some(-4000));
        assert_eq!(c.await.unwrap().as_str(), Some("delete"));
    });
}

#[test]
fn read_only_mode_reads_and_refuses_writes() {
    let dir = Dir::new("ro");
    let path = dir.file("ro.db");
    run(async {
        let db = SqliteDriver::open(&path).await.unwrap();
        db.execute("CREATE TABLE t (n INTEGER)").await.unwrap();
        db.execute("INSERT INTO t VALUES (7)").await.unwrap();
    });
    run(async {
        let db = SqliteDriver::with_config(SqliteConfig::file(&path).read_only()).await.unwrap();
        assert_eq!(scalar(&db, "SELECT n FROM t").await.as_i64(), Some(7));
        let e = db.execute("INSERT INTO t VALUES (8)").await.unwrap_err();
        assert!(e.to_string().contains("readonly"), "{e}");
        // A transaction still works for consistent reads.
        let n: i64 = db
            .transaction(|tx| Box::pin(async move { Ok(tx.query("SELECT COUNT(*) FROM t", &[]).await?.rows[0][0].as_i64().unwrap()) }))
            .await
            .unwrap();
        assert_eq!(n, 1);
    });
    run(async {
        // Read-only never creates a file.
        let missing = dir.file("missing.db");
        assert!(SqliteDriver::with_config(SqliteConfig::file(&missing).read_only()).await.is_err());
        assert!(!missing.exists());
        assert!(SqliteDriver::connect(&format!("sqlite:{}?mode=rw", missing.display())).await.is_err(), "rw does not create either");
    });
}

/// A tick counter on the same executor: if SQLite ran on the reactor, the
/// ticker could not advance while it works.
fn ticker() -> Rc<Cell<u32>> {
    let ticks = Rc::new(Cell::new(0));
    let t = Rc::clone(&ticks);
    rt_core::spawn(async move {
        loop {
            rt_core::sleep(Duration::from_millis(5)).await;
            t.set(t.get() + 1);
        }
    });
    ticks
}

#[test]
fn a_long_query_does_not_stall_the_reactor() {
    run(async {
        let db = SqliteDriver::open_in_memory().await.unwrap();
        let ticks = ticker();
        let started = Instant::now();
        // A few hundred milliseconds of pure SQLite work.
        let n = scalar(
            &db,
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 3000000) SELECT COUNT(*) FROM c",
        )
        .await;
        assert_eq!(n.as_i64(), Some(3_000_000));
        let ran = started.elapsed();
        assert!(ran > Duration::from_millis(50), "the query was too quick to prove anything: {ran:?}");
        assert!(ticks.get() >= 3, "the reactor stalled during the query: {} ticks in {ran:?}", ticks.get());
    });
}

#[test]
fn waiting_for_a_lock_queues_without_stalling_and_times_out_cleanly() {
    let dir = Dir::new("busy");
    let path = dir.file("busy.db");
    run(async {
        let db = SqliteDriver::open(&path).await.unwrap();
        db.execute("CREATE TABLE t (n INTEGER)").await.unwrap();
    });
    // Another process-like holder of the write lock, on its own thread.
    let (locked_tx, locked) = std::sync::mpsc::channel();
    let holder = {
        let path = path.clone();
        std::thread::spawn(move || {
            run(async move {
                let db = SqliteDriver::open(&path).await.unwrap();
                db.transaction(|tx| {
                    Box::pin(async move {
                        tx.execute_with("INSERT INTO t VALUES (1)", &[]).await?;
                        // The write lock is held from here on.
                        locked_tx.send(()).unwrap();
                        rt_core::sleep(Duration::from_millis(300)).await;
                        Ok(())
                    })
                })
                .await
                .unwrap();
            })
        })
    };
    run(async {
        // Default 5 s busy timeout: the write waits for the holder, then runs;
        // meanwhile the reactor keeps going.
        let db = SqliteDriver::open(&path).await.unwrap();
        locked.recv().expect("the holder took the lock");
        let ticks = ticker();
        let started = Instant::now();
        db.execute("INSERT INTO t VALUES (2)").await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(100), "it did wait: {:?}", started.elapsed());
        assert!(ticks.get() >= 5, "the reactor stalled while waiting for the lock");
        assert_eq!(scalar(&db, "SELECT COUNT(*) FROM t").await.as_i64(), Some(2));
    });
    holder.join().unwrap();

    // A short busy timeout gives up with "database is locked".
    let (locked_tx, locked) = std::sync::mpsc::channel();
    let holder = {
        let path = path.clone();
        std::thread::spawn(move || {
            run(async move {
                let db = SqliteDriver::open(&path).await.unwrap();
                db.transaction(|tx| {
                    Box::pin(async move {
                        tx.execute_with("INSERT INTO t VALUES (3)", &[]).await?;
                        // The write lock is held from here on.
                        locked_tx.send(()).unwrap();
                        rt_core::sleep(Duration::from_millis(400)).await;
                        Ok(())
                    })
                })
                .await
                .unwrap();
            })
        })
    };
    run(async {
        let db = SqliteDriver::with_config(SqliteConfig::file(&path).busy_timeout(Duration::from_millis(50))).await.unwrap();
        locked.recv().expect("the holder took the lock");
        let e = db.execute("INSERT INTO t VALUES (4)").await.unwrap_err();
        assert!(e.to_string().contains("locked"), "{e}");
        // Readers are not blocked by the writer in WAL mode.
        assert!(db.query("SELECT COUNT(*) FROM t", &[]).await.is_ok());
    });
    holder.join().unwrap();
}

#[test]
fn opening_never_fails_on_a_lock_held_for_a_moment() {
    // Many drivers open the same fresh file at once (WAL set up, recovered)
    // while others already write, each with a 1 ms busy timeout: opening
    // waits out those brief locks itself; only statements use the timeout.
    let dir = Dir::new("open-race");
    let path = dir.file("race.db");
    for round in 0..5 {
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("db-wal"));
        let _ = std::fs::remove_file(path.with_extension("db-shm"));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    run(async move {
                        let cfg = SqliteConfig::file(&path).busy_timeout(Duration::from_millis(1)).pool_size(1);
                        let db = SqliteDriver::with_config(cfg).await.unwrap_or_else(|e| panic!("round {round}, opener {i}: {e}"));
                        // Writes may lose the race (1 ms timeout); opening must not.
                        let _ = db.execute("CREATE TABLE IF NOT EXISTS t (n INTEGER)").await;
                        let _ = db.execute_with("INSERT INTO t VALUES (?)", params![i]).await;
                    })
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
    }
}

#[test]
fn values_are_typed_by_the_declared_column_type() {
    run(async {
        let db = SqliteDriver::open_in_memory().await.unwrap();
        db.execute(
            "CREATE TABLE v (b BOOLEAN, d DATE, dt DATETIME, ts TIMESTAMP, t TIME, j JSON, dec DECIMAL(10,2), num NUMERIC,
                             i INTEGER, r REAL, s TEXT, blob BLOB, any_col)",
        )
        .await
        .unwrap();
        db.execute_with(
            "INSERT INTO v VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                true.into(),
                "2026-11-27".into(),
                "2026-11-27 09:30:00".into(),
                "2026-11-27T09:30:00.250Z".into(),
                "23:59:59".into(),
                Param::Object(vec![("a".into(), Param::Int(1)), ("b".into(), Param::Array(vec!["x".into()]))]),
                "12.50".into(),
                "abc".into(),
                Param::Int(i64::MIN),
                Param::Float(-0.5),
                "Việt 😀".into(),
                Param::Bytes(vec![0, 255, 1]),
                Param::Null,
            ],
        )
        .await
        .unwrap();
        let r = db.query("SELECT * FROM v", &[]).await.unwrap();
        let row = &r.rows[0];
        assert_eq!(row[0], Value::Bool(true));
        assert_eq!(row[1], Value::Date("2026-11-27".into()));
        assert_eq!(row[2], Value::DateTime("2026-11-27 09:30:00".into()));
        assert_eq!(row[3], Value::DateTime("2026-11-27T09:30:00.250Z".into()));
        assert_eq!(row[4], Value::Time("23:59:59".into()));
        assert_eq!(row[5], Value::Json(r#"{"a":1,"b":["x"]}"#.into()));
        // DECIMAL has numeric affinity: SQLite keeps 12.5 as a REAL.
        assert_eq!(row[6], Value::Float(12.5));
        assert_eq!(row[7], Value::Decimal("abc".into()), "non-numeric text stays text in a NUMERIC column");
        assert_eq!(row[8], Value::Int(i64::MIN));
        assert_eq!(row[9], Value::Float(-0.5));
        assert_eq!(row[10], Value::Text("Việt 😀".into()));
        assert_eq!(row[11], Value::Bytes(vec![0, 255, 1]));
        assert_eq!(row[12], Value::Null);
        // An expression has no declared type: the stored type decides.
        let e = db.query("SELECT 1 + 1, 'x' || 'y', x'00', NULL, 1.5", &[]).await.unwrap();
        assert_eq!(e.rows[0], [Value::Int(2), Value::Text("xy".into()), Value::Bytes(vec![0]), Value::Null, Value::Float(1.5)]);
        // Invalid UTF-8 stored as text comes back as bytes, not mangled.
        let bad = db.query("SELECT CAST(x'fffe' AS TEXT)", &[]).await.unwrap();
        assert_eq!(bad.rows[0][0], Value::Bytes(vec![0xff, 0xfe]));
    });
}

#[test]
fn parameters_are_checked() {
    run(async {
        let db = SqliteDriver::open_in_memory().await.unwrap();
        let e = db.query("SELECT ?, ?", params![1]).await.unwrap_err();
        assert!(e.to_string().contains("2 parameter"), "{e}");
        let e = db.query("SELECT ?", &[Param::UInt(u64::MAX)]).await.unwrap_err();
        assert!(matches!(e, OrmError::TypeConversion(_)), "{e:?}");
        assert_eq!(db.query_scalar("SELECT ?", &[Param::UInt(i64::MAX as u64)]).await.unwrap(), Value::Int(i64::MAX));
        let e = db.query("SELEC 1", &[]).await.unwrap_err();
        assert!(matches!(e, OrmError::Query(m) if m.contains("syntax error")));
    });
}

#[test]
fn unique_violations_name_the_column() {
    run(async {
        let db = SqliteDriver::open_in_memory().await.unwrap();
        db.execute_script("CREATE TABLE u (id INTEGER PRIMARY KEY, email TEXT UNIQUE, a TEXT, b TEXT, UNIQUE (a, b));").await.unwrap();
        db.execute("INSERT INTO u VALUES (1, 'x@y', 'p', 'q')").await.unwrap();
        for (sql, field) in [
            ("INSERT INTO u VALUES (2, 'x@y', 'r', 's')", "email"),
            ("INSERT INTO u VALUES (1, 'z@y', 'r', 's')", "id"),
            ("INSERT INTO u VALUES (3, 'w@y', 'p', 'q')", "a"),
        ] {
            match db.execute(sql).await.unwrap_err() {
                OrmError::UniqueViolation { field: f } => assert_eq!(f, field, "{sql}"),
                other => panic!("{sql}: {other:?}"),
            }
        }
        // NOT NULL / CHECK are plain query errors.
        db.execute("CREATE TABLE c (n INTEGER NOT NULL CHECK (n > 0))").await.unwrap();
        assert!(matches!(db.execute("INSERT INTO c VALUES (NULL)").await.unwrap_err(), OrmError::Query(_)));
        assert!(matches!(db.execute("INSERT INTO c VALUES (-1)").await.unwrap_err(), OrmError::Query(_)));
    });
}

#[test]
fn full_text_search_and_json_functions_are_built_in() {
    run(async {
        let db = SqliteDriver::open_in_memory().await.unwrap();
        db.execute_script(
            "CREATE VIRTUAL TABLE docs USING fts5(title, body, tokenize = 'unicode61 remove_diacritics 2');
             INSERT INTO docs VALUES ('Nail salon', 'Sơn gel và đắp bột');
             INSERT INTO docs VALUES ('Hair', 'Cắt tóc nam nữ');",
        )
        .await
        .unwrap();
        // Diacritics folded: "son gel" finds "Sơn gel".
        let hits = db.query("SELECT title FROM docs WHERE docs MATCH ? ORDER BY rank", params!["son gel"]).await.unwrap();
        assert_eq!(hits.rows.len(), 1);
        assert_eq!(hits.rows[0][0].as_str(), Some("Nail salon"));
        let snippet = db.query_scalar("SELECT highlight(docs, 1, '[', ']') FROM docs WHERE docs MATCH 'bot'", &[]).await.unwrap();
        assert_eq!(snippet.as_str(), Some("Sơn gel và đắp [bột]"));

        let j = db
            .query_scalar("SELECT json_extract(?, '$.items[1].qty')", &[Param::Object(vec![(
                "items".into(),
                Param::Array(vec![
                    Param::Object(vec![("qty".into(), Param::Int(1))]),
                    Param::Object(vec![("qty".into(), Param::Int(5))]),
                ]),
            )])])
            .await
            .unwrap();
        assert_eq!(j, Value::Int(5));
        let n = db.query_scalar("SELECT COUNT(*) FROM json_each(?)", &[Param::Array(vec![1.into(), 2.into(), 3.into()])]).await.unwrap();
        assert_eq!(n.as_i64(), Some(3));
    });
}

#[test]
fn rust_functions_are_callable_from_sql() {
    run(async {
        let cfg = SqliteConfig::memory()
            .function("cents", 1, true, |args| match &args[0] {
                Value::Float(f) => Ok(Value::Int((f * 100.0).round() as i64)),
                Value::Int(i) => Ok(Value::Int(i * 100)),
                Value::Null => Ok(Value::Null),
                other => Err(format!("cents() wants a number, got {other:?}")),
            })
            .function("join_all", -1, true, |args| {
                Ok(Value::Text(args.iter().map(|a| a.as_str().map(str::to_string).unwrap_or_else(|| format!("{a:?}"))).collect::<Vec<_>>().join("+")))
            })
            .pool_size(2);
        let db = SqliteDriver::with_config(cfg).await.unwrap();
        assert_eq!(scalar(&db, "SELECT cents(12.345)").await, Value::Int(1235));
        assert_eq!(scalar(&db, "SELECT join_all('a', 'b', 'c')").await.as_str(), Some("a+b+c"));
        // Deterministic: usable in an index expression.
        db.execute_script("CREATE TABLE p (price REAL); CREATE INDEX p_cents ON p (cents(price)); INSERT INTO p VALUES (1.5);")
            .await
            .unwrap();
        assert_eq!(scalar(&db, "SELECT COUNT(*) FROM p WHERE cents(price) = 150").await.as_i64(), Some(1));
        // An error message reaches the caller.
        let e = db.query("SELECT cents('x')", &[]).await.unwrap_err();
        assert!(e.to_string().contains("cents() wants a number"), "{e}");
        // Through call_function too.
        let r = db.call_function("cents", OutKind::Number, &[CallArg::In(Param::Int(3))]).await.unwrap();
        assert_eq!(r.value, Value::Int(300));
        assert!(db.call_function("drop table p; --", OutKind::Number, &[]).await.is_err(), "a name is a name, not SQL");
        // Stored procedures do not exist in SQLite.
        assert!(matches!(db.call("p", &[]).await.unwrap_err(), OrmError::Unsupported(_)));
    });
}

#[test]
fn online_backup_copies_a_consistent_database() {
    let dir = Dir::new("backup");
    run(async {
        let db = SqliteDriver::open_in_memory().await.unwrap();
        db.execute("CREATE TABLE t (n INTEGER)").await.unwrap();
        let rows: Vec<Vec<Param>> = (0..1000).map(|i| vec![Param::Int(i)]).collect();
        db.execute_many("INSERT INTO t VALUES (?)", &rows).await.unwrap();
        db.backup_to(dir.file("copy.db")).await.unwrap();
        // The live database keeps working after the backup.
        db.execute("INSERT INTO t VALUES (-1)").await.unwrap();
    });
    run(async {
        let copy = SqliteDriver::with_config(SqliteConfig::file(dir.file("copy.db")).read_only()).await.unwrap();
        assert_eq!(scalar(&copy, "SELECT COUNT(*) FROM t").await.as_i64(), Some(1000));
        assert_eq!(scalar(&copy, "SELECT SUM(n) FROM t").await.as_i64(), Some(499_500));
    });
}

#[test]
fn cursors_and_streams_never_hold_the_whole_result() {
    run(async {
        let db = SqliteDriver::open_in_memory().await.unwrap();
        let big = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 200000) SELECT x FROM c";
        // A cursor read in batches.
        let mut cur = db.cursor(big, &[]).await.unwrap();
        let (mut n, mut last) = (0, 0);
        while let Some(b) = cur.next_batch(5000).await.unwrap() {
            n += b.rows.len();
            last = b.rows.last().unwrap()[0].as_i64().unwrap();
        }
        assert_eq!((n, last), (200_000, 200_000));
        // Streaming stops at the callback's error and the connection works on.
        let mut seen = 0;
        let e = db
            .query_each(big, &[], |_| {
                seen += 1;
                if seen == 1234 { Err(OrmError::Query("enough".into())) } else { Ok(()) }
            })
            .await
            .unwrap_err();
        assert!(e.to_string().contains("enough"));
        assert_eq!(seen, 1234);
        assert_eq!(scalar(&db, "SELECT 41 + 1").await.as_i64(), Some(42));
        // Two cursors open at once on one transaction, interleaved.
        let pairs: Vec<(i64, i64)> = db
            .transaction(|tx| {
                Box::pin(async move {
                    let mut a = tx.open_cursor("SELECT value FROM json_each('[1,2,3]')", &[]).await?;
                    let mut b = tx.open_cursor("SELECT value FROM json_each('[10,20,30]')", &[]).await?;
                    let mut out = Vec::new();
                    loop {
                        let (x, y) = (tx.fetch(&mut a, 1).await?, tx.fetch(&mut b, 1).await?);
                        match (x, y) {
                            (Some(x), Some(y)) => out.push((x.rows[0][0].as_i64().unwrap(), y.rows[0][0].as_i64().unwrap())),
                            _ => break,
                        }
                    }
                    Ok(out)
                })
            })
            .await
            .unwrap();
        assert_eq!(pairs, [(1, 10), (2, 20), (3, 30)]);
    });
}

#[test]
fn execute_script_runs_triggers_and_multi_statement_scripts() {
    run(async {
        let db = SqliteDriver::open_in_memory().await.unwrap();
        db.execute_script(
            "CREATE TABLE a (n INTEGER);
             CREATE TABLE log (msg TEXT);
             CREATE TRIGGER a_ins AFTER INSERT ON a BEGIN
               INSERT INTO log VALUES ('added ' || NEW.n);
             END;
             INSERT INTO a VALUES (1); INSERT INTO a VALUES (2);",
        )
        .await
        .unwrap();
        let r = db.query("SELECT msg FROM log ORDER BY rowid", &[]).await.unwrap();
        assert_eq!(r.rows.iter().map(|r| r[0].as_str().unwrap()).collect::<Vec<_>>(), ["added 1", "added 2"]);
        // An error stops the script; earlier statements stay applied.
        assert!(db.execute_script("INSERT INTO a VALUES (3); INSERT INTO nope VALUES (1); INSERT INTO a VALUES (4);").await.is_err());
        assert_eq!(scalar(&db, "SELECT MAX(n) FROM a").await.as_i64(), Some(3));
    });
}

#[test]
fn dropping_the_driver_closes_its_files() {
    let dir = Dir::new("close");
    let path = dir.file("closing.db");
    run(async {
        let db = SqliteDriver::open(&path).await.unwrap();
        db.execute("CREATE TABLE t (n INTEGER)").await.unwrap();
        db.execute("INSERT INTO t VALUES (1)").await.unwrap();
    });
    // Windows refuses to delete an open file: this proves every connection
    // thread closed its handle.
    let mut last = None;
    for _ in 0..50 {
        match std::fs::remove_file(&path) {
            Ok(()) => {
                last = None;
                break;
            }
            Err(e) => last = Some(e),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(last.is_none(), "database file still open: {last:?}");
}

#[test]
fn a_cancelled_query_is_interrupted_and_the_connection_is_free_at_once() {
    run(async {
        let db = SqliteDriver::with_config(SqliteConfig::memory().pool_size(1)).await.unwrap();
        let endless = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT COUNT(*) FROM c";
        // A query that would never end, given up after 100 ms.
        let r = rt_core::timeout(Duration::from_millis(100), db.query_scalar(endless, &[])).await;
        assert!(r.is_err(), "timed out");
        // The only connection is back and idle: the next query answers at
        // once instead of waiting for the endless one.
        let started = Instant::now();
        let v = rt_core::timeout(Duration::from_secs(2), db.query_scalar("SELECT 7", &[])).await.expect("connection freed");
        assert_eq!(v.unwrap().as_i64(), Some(7));
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
    });
}

#[test]
fn hand_written_sql_has_upsert_returning_and_window_functions() {
    run(async {
        let db = SqliteDriver::open_in_memory().await.unwrap();
        db.execute("CREATE TABLE stock (sku TEXT PRIMARY KEY, qty INTEGER NOT NULL)").await.unwrap();
        let upsert = "INSERT INTO stock VALUES (?, ?) ON CONFLICT (sku) DO UPDATE SET qty = qty + excluded.qty RETURNING qty";
        assert_eq!(db.query_scalar(upsert, params!["A", 5]).await.unwrap().as_i64(), Some(5));
        assert_eq!(db.query_scalar(upsert, params!["A", 3]).await.unwrap().as_i64(), Some(8));
        db.execute_with("INSERT INTO stock VALUES ('B', 1), ('C', 8)", &[]).await.unwrap();
        let ranked = db
            .query("SELECT sku, RANK() OVER (ORDER BY qty DESC) AS r, SUM(qty) OVER () AS total FROM stock ORDER BY sku", &[])
            .await
            .unwrap();
        let shape: Vec<(String, i64, i64)> =
            ranked.rows.iter().map(|r| (r[0].as_str().unwrap().into(), r[1].as_i64().unwrap(), r[2].as_i64().unwrap())).collect();
        assert_eq!(shape, [("A".into(), 1, 17), ("B".into(), 3, 17), ("C".into(), 1, 17)]);
    });
}

#[test]
fn the_bundled_engine_is_the_same_everywhere() {
    run(async {
        let db = SqliteDriver::open_in_memory().await.unwrap();
        let v = scalar(&db, "SELECT sqlite_version()").await;
        let v = v.as_str().unwrap().to_string();
        // The acceptance report reads this line.
        println!("SQLite engine: {v}");
        let parts: Vec<u32> = v.split('.').map(|p| p.parse().unwrap()).collect();
        assert!(parts[0] == 3 && parts[1] >= 38, "JSON built in, RETURNING, window functions: {v}");
        let fts = scalar(&db, "SELECT sqlite_compileoption_used('ENABLE_FTS5')").await;
        assert_eq!(fts.as_i64(), Some(1));
    });
}

#[test]
fn incremental_blob_io_reads_and_writes_pieces_in_place() {
    run(async {
        let db = SqliteDriver::open_in_memory().await.unwrap();
        db.execute("CREATE TABLE files (id INTEGER PRIMARY KEY, data BLOB)").await.unwrap();
        const SIZE: usize = 5 << 20;
        db.execute_with("INSERT INTO files VALUES (1, zeroblob(?))", params![SIZE as i64]).await.unwrap();
        assert_eq!(db.blob_len("files", "data", 1).await.unwrap(), SIZE);
        // Fill it 64 KiB at a time, as an upload would arrive.
        let chunk = |i: usize| -> Vec<u8> { (0..65536).map(|j| ((i * 31 + j) % 251) as u8).collect() };
        for i in 0..SIZE / 65536 {
            db.blob_write("files", "data", 1, i * 65536, &chunk(i)).await.unwrap();
        }
        // Read a piece in the middle, straddling two chunks.
        let got = db.blob_read("files", "data", 1, 7 * 65536 - 10, 20).await.unwrap();
        let mut want = chunk(6)[65536 - 10..].to_vec();
        want.extend_from_slice(&chunk(7)[..10]);
        assert_eq!(got, want);
        // SQL sees the same bytes.
        let tail = db.query_scalar("SELECT substr(data, ?, 4) FROM files WHERE id = 1", params![(SIZE - 3) as i64]).await.unwrap();
        assert_eq!(tail, Value::Bytes(chunk(SIZE / 65536 - 1)[65536 - 4..].to_vec()));
        // A BLOB never grows in place; missing rows are errors.
        assert!(db.blob_write("files", "data", 1, SIZE - 2, b"abcd").await.is_err());
        assert!(db.blob_read("files", "data", 1, SIZE - 2, 4).await.is_err());
        assert!(db.blob_len("files", "data", 99).await.is_err());
        assert!(db.blob_len("files; DROP TABLE files", "data", 1).await.is_err(), "names are not SQL");
        assert_eq!(scalar(&db, "SELECT COUNT(*) FROM files").await.as_i64(), Some(1));
    });
}

#[test]
fn change_hooks_report_every_row_and_transaction_end() {
    use kernway_orm_sqlite::{Change, ChangeKind};
    use std::sync::{Arc, Mutex};
    let changes: Arc<Mutex<Vec<Change>>> = Arc::default();
    let ends: Arc<Mutex<Vec<&str>>> = Arc::default();
    run({
        let (changes, ends) = (Arc::clone(&changes), Arc::clone(&ends));
        async move {
            let (c, e1, e2) = (Arc::clone(&changes), Arc::clone(&ends), Arc::clone(&ends));
            let cfg = SqliteConfig::memory()
                .pool_size(2)
                .on_change(move |ch| c.lock().unwrap().push(ch.clone()))
                .on_commit(move || e1.lock().unwrap().push("commit"))
                .on_rollback(move || e2.lock().unwrap().push("rollback"));
            let db = SqliteDriver::with_config(cfg).await.unwrap();
            db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n INTEGER)").await.unwrap();
            ends.lock().unwrap().clear();
            db.execute("INSERT INTO t VALUES (1, 10), (2, 20)").await.unwrap();
            db.execute("UPDATE t SET n = n + 1 WHERE id = 2").await.unwrap();
            db.execute("DELETE FROM t WHERE id = 1").await.unwrap();
            let r: Result<(), OrmError> = db
                .transaction(|tx| {
                    Box::pin(async move {
                        tx.execute_with("INSERT INTO t VALUES (3, 30)", &[]).await?;
                        Err(OrmError::Query("abort".into()))
                    })
                })
                .await;
            assert!(r.is_err());
        }
    });
    let got: Vec<(ChangeKind, String, i64)> = changes.lock().unwrap().iter().map(|c| (c.kind, c.table.clone(), c.rowid)).collect();
    assert_eq!(
        got,
        [
            (ChangeKind::Insert, "t".into(), 1),
            (ChangeKind::Insert, "t".into(), 2),
            (ChangeKind::Update, "t".into(), 2),
            (ChangeKind::Delete, "t".into(), 1),
            (ChangeKind::Insert, "t".into(), 3), // reported, then rolled back
        ]
    );
    assert!(changes.lock().unwrap().iter().all(|c| c.database == "main"));
    assert_eq!(*ends.lock().unwrap(), ["commit", "commit", "commit", "rollback"]);
}

#[test]
fn a_database_serializes_to_bytes_and_opens_from_them() {
    let dir = Dir::new("image");
    run(async {
        let db = SqliteDriver::open(dir.file("src.db")).await.unwrap();
        db.execute_script("CREATE TABLE t (n INTEGER, s TEXT); INSERT INTO t VALUES (1, 'một'), (2, 'hai');").await.unwrap();
        let image = db.serialize().await.unwrap();
        assert!(image.starts_with(b"SQLite format 3\0"), "the bytes of a database file");

        // A private in-memory copy, shared by its own pool.
        let copy = SqliteDriver::from_bytes(image.clone()).await.unwrap();
        assert_eq!(scalar(&copy, "SELECT group_concat(s, ',') FROM t").await.as_str(), Some("một,hai"));
        copy.execute("INSERT INTO t VALUES (3, 'ba')").await.unwrap();
        assert_eq!(scalar(&copy, "SELECT COUNT(*) FROM t").await.as_i64(), Some(3));
        assert_eq!(scalar(&db, "SELECT COUNT(*) FROM t").await.as_i64(), Some(2), "the source is untouched");

        // The same bytes written to disk are a database file.
        std::fs::write(dir.file("written.db"), &image).unwrap();
        let file = SqliteDriver::with_config(SqliteConfig::file(dir.file("written.db")).read_only()).await.unwrap();
        assert_eq!(scalar(&file, "SELECT SUM(n) FROM t").await.as_i64(), Some(3));

        assert!(SqliteDriver::from_bytes(b"not a database".to_vec()).await.is_err());
    });
}
