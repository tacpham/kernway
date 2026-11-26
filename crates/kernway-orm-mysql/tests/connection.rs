//! Connection-level features against a real MySQL server, each proven on the
//! wire or by the server itself:
//!
//! - the compressed protocol (the server reports `Compression = ON`, and a
//!   counting relay sees far fewer bytes for a compressible result);
//! - pipelined `execute_many` (the relay counts request/response turns);
//! - server-side cursors (`COM_STMT_FETCH`) interleaved with other statements;
//! - Unix-domain sockets (`unix_socket_*`): only where the server's socket
//!   file is reachable — the acceptance run executes them inside the MySQL
//!   container (`KERNWAY_MYSQL_SOCKET`).
//!
//! Skips unless `KERNWAY_MYSQL_URL` is set.
#![cfg(feature = "mysql")]

use kernway_orm_core::{transaction::TransactionalDriver, OrmError};
use kernway_orm_mysql::{params, Conn, MySqlDriver, MysqlConfig, Param, QueryResult};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;

fn url() -> Option<String> {
    std::env::var("KERNWAY_MYSQL_URL").ok()
}

fn run<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

/// A TCP relay in front of the server that counts the bytes each way and the
/// number of *turns*: times the client starts sending after the server has
/// answered (one per round trip).
struct Relay {
    port: u16,
    up: Arc<AtomicU64>,
    down: Arc<AtomicU64>,
    turns: Arc<AtomicU64>,
}

impl Relay {
    fn start(target: (String, u16)) -> Relay {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (up, down, turns) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        // 0 = nothing yet, 1 = client spoke last, 2 = server spoke last.
        let last = Arc::new(AtomicU8::new(0));
        let (u, d, t) = (up.clone(), down.clone(), turns.clone());
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { return };
                let server = TcpStream::connect((target.0.as_str(), target.1)).unwrap();
                let pipe = |mut from: TcpStream, mut to: TcpStream, bytes: Arc<AtomicU64>, me: u8, turns: Option<Arc<AtomicU64>>, last: Arc<AtomicU8>| {
                    std::thread::spawn(move || {
                        let mut buf = vec![0u8; 64 * 1024];
                        loop {
                            let n = match from.read(&mut buf) {
                                Ok(0) | Err(_) => {
                                    let _ = to.shutdown(std::net::Shutdown::Write);
                                    return;
                                }
                                Ok(n) => n,
                            };
                            bytes.fetch_add(n as u64, Ordering::SeqCst);
                            if last.swap(me, Ordering::SeqCst) != me {
                                if let Some(t) = &turns {
                                    t.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                            if to.write_all(&buf[..n]).is_err() {
                                return;
                            }
                        }
                    });
                };
                pipe(client.try_clone().unwrap(), server.try_clone().unwrap(), u.clone(), 1, Some(t.clone()), last.clone());
                pipe(server, client, d.clone(), 2, None, last.clone());
            }
        });
        Relay { port, up, down, turns }
    }

    fn url(&self, base: &MysqlConfig, extra: &str) -> String {
        format!(
            "mysql://{}:{}@127.0.0.1:{}/{}?ssl-mode=disabled{extra}",
            base.user,
            base.password,
            self.port,
            base.database.clone().unwrap_or_default()
        )
    }

    fn snapshot(&self) -> (u64, u64, u64) {
        (self.up.load(Ordering::SeqCst), self.down.load(Ordering::SeqCst), self.turns.load(Ordering::SeqCst))
    }
}

fn session_status(r: QueryResult) -> String {
    match r {
        QueryResult::Rows { rows, .. } => rows[0][1].clone().unwrap_or_default(),
        QueryResult::Ok { .. } => String::new(),
    }
}

#[test]
fn compressed_protocol_is_negotiated_and_carries_every_kind_of_traffic() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_MYSQL_URL not set") };
    run(async move {
        // The server confirms the session is compressed, with zlib.
        let mut cfg = MysqlConfig::from_url(&url).unwrap();
        cfg.compress = true;
        let mut conn = Conn::connect(&cfg).await.unwrap();
        assert!(conn.is_compressed());
        assert_eq!(session_status(conn.query("SHOW SESSION STATUS LIKE 'Compression'").await.unwrap()), "ON");
        assert_eq!(session_status(conn.query("SHOW SESSION STATUS LIKE 'Compression_algorithm'").await.unwrap()), "zlib");

        let sep = if url.contains('?') { '&' } else { '?' };
        let plain = MySqlDriver::connect(&url).await.unwrap();
        let zdb = MySqlDriver::connect(&format!("{url}{sep}compress=true")).await.unwrap();
        zdb.execute("DROP TABLE IF EXISTS kwc_z").await.unwrap();
        zdb.execute("CREATE TABLE kwc_z (id INT PRIMARY KEY, body LONGBLOB, note LONGTEXT)").await.unwrap();

        // Values larger than one 16 MiB packet both ways (split into several
        // compressed frames), small ones (sent uncompressed inside frames),
        // incompressible random-looking bytes and very compressible text.
        let mut big = vec![0u8; 20 * 1024 * 1024 + 7];
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        for b in big.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = x as u8;
        }
        let text = "kernway compressed ".repeat(200_000);
        zdb.execute_with("INSERT INTO kwc_z VALUES (?, ?, ?)", params![1, big.clone(), text.clone()]).await.unwrap();
        zdb.execute_with("INSERT INTO kwc_z VALUES (?, ?, ?)", params![2, vec![1u8, 2, 3], "x"]).await.unwrap();
        let md5 = plain.query_scalar("SELECT MD5(body) FROM kwc_z WHERE id = 1", &[]).await.unwrap();
        let expect: String = {
            use md5::Digest;
            md5::Md5::digest(&big).iter().map(|b| format!("{b:02x}")).collect()
        };
        assert_eq!(md5.as_str(), Some(expect.as_str()), "the server stored exactly what was sent");
        let back = zdb.query("SELECT body, note FROM kwc_z ORDER BY id", &[]).await.unwrap();
        assert_eq!(back.rows[0][0], kernway_orm_mysql::Value::Bytes(big));
        assert_eq!(back.rows[0][1].as_str(), Some(text.as_str()));
        assert_eq!(back.rows[1][0], kernway_orm_mysql::Value::Bytes(vec![1, 2, 3]));
        // Text protocol, binary protocol, scripts, streaming: the same rows
        // with and without compression.
        zdb.execute_script("DROP TABLE IF EXISTS kwc_n; CREATE TABLE kwc_n (n INT PRIMARY KEY, s VARCHAR(40))").await.unwrap();
        let rows: Vec<Vec<Param>> = (0..3000).map(|n| vec![n.into(), format!("row {n}").into()]).collect();
        assert_eq!(zdb.execute_many("INSERT INTO kwc_n VALUES (?, ?)", &rows).await.unwrap(), 3000);
        let a = zdb.query("SELECT n, s FROM kwc_n ORDER BY n", &[]).await.unwrap();
        let b = plain.query("SELECT n, s FROM kwc_n WHERE n >= ? ORDER BY n", params![0]).await.unwrap();
        assert_eq!(a, b);
        let mut streamed = 0;
        zdb.query_each("SELECT n FROM kwc_n", &[], |_| {
            streamed += 1;
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(streamed, 3000);
        let mut cur = zdb.cursor("SELECT n FROM kwc_n WHERE n < ? ORDER BY n", params![1000]).await.unwrap();
        let mut seen = 0;
        while let Some(b) = cur.next_batch(333).await.unwrap() {
            seen += b.rows.len();
        }
        assert_eq!(seen, 1000);
    });
}

#[test]
fn compression_shrinks_what_crosses_the_wire() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_MYSQL_URL not set") };
    let base = MysqlConfig::from_url(&url).unwrap();
    let relay = Relay::start((base.host.clone(), base.port));
    run(async move {
        let setup = MySqlDriver::connect(&url).await.unwrap();
        setup.execute("DROP TABLE IF EXISTS kwc_w").await.unwrap();
        setup.execute("CREATE TABLE kwc_w (id INT PRIMARY KEY, s TEXT)").await.unwrap();
        let rows: Vec<Vec<Param>> = (0..2000).map(|n| vec![n.into(), "the same words again and again ".repeat(8).into()]).collect();
        setup.execute_many("INSERT INTO kwc_w VALUES (?, ?)", &rows).await.unwrap();

        let measure = |extra: &'static str| {
            let url = relay.url(&base, extra);
            let relay = &relay;
            async move {
                let before = relay.snapshot();
                let db = MySqlDriver::connect_with(MysqlConfig::from_url(&url).unwrap(), 1).await.unwrap();
                let r = db.query("SELECT id, s FROM kwc_w ORDER BY id", &[]).await.unwrap();
                assert_eq!(r.rows.len(), 2000);
                let after = relay.snapshot();
                after.1 - before.1
            }
        };
        let plain = measure("").await;
        let compressed = measure("&compress=true").await;
        eprintln!("bytes from the server: plain {plain}, compressed {compressed}");
        assert!(plain > 500_000, "the result is ~500 KB uncompressed");
        assert!(compressed * 5 < plain, "compressed {compressed} vs plain {plain}");
    });
}

#[test]
fn execute_many_is_pipelined_into_a_few_round_trips() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_MYSQL_URL not set") };
    let base = MysqlConfig::from_url(&url).unwrap();
    let relay = Relay::start((base.host.clone(), base.port));
    run(async move {
        let db = MySqlDriver::connect_with(MysqlConfig::from_url(&relay.url(&base, "")).unwrap(), 1).await.unwrap();
        db.execute("DROP TABLE IF EXISTS kwc_p").await.unwrap();
        db.execute("CREATE TABLE kwc_p (n INT PRIMARY KEY, s VARCHAR(20))").await.unwrap();
        let rows: Vec<Vec<Param>> = (0..1200).map(|n| vec![n.into(), format!("r{n}").into()]).collect();

        let (_, _, t0) = relay.snapshot();
        assert_eq!(db.execute_many("INSERT INTO kwc_p VALUES (?, ?)", &rows).await.unwrap(), 1200);
        let (_, _, t1) = relay.snapshot();
        // prepare + START TRANSACTION + 3 flights of ≤500 + COMMIT.
        eprintln!("execute_many of 1200 rows: {} round trips", t1 - t0);
        assert!(t1 - t0 <= 8, "{} round trips", t1 - t0);

        // The same through one statement at a time, for contrast.
        let upd: Vec<Vec<Param>> = (0..50).map(|n| vec![format!("u{n}").into(), n.into()]).collect();
        let (_, _, t2) = relay.snapshot();
        for r in &upd {
            db.execute_with("UPDATE kwc_p SET s = ? WHERE n = ?", r).await.unwrap();
        }
        let (_, _, t3) = relay.snapshot();
        assert!(t3 - t2 >= 50);
        assert_eq!(db.execute_many("UPDATE kwc_p SET s = ? WHERE n = ?", &upd).await.unwrap(), 50);
        assert_eq!(db.query_scalar("SELECT COUNT(*) FROM kwc_p WHERE s LIKE 'u%'", &[]).await.unwrap().as_i64(), Some(50));

        // A statement with a result set is still run row by row (correct, if
        // not pipelined).
        let r = db.execute_many("SELECT ?", &[params![1].to_vec(), params![2].to_vec()]).await.unwrap();
        assert_eq!(r, 0);
    });
}

#[test]
fn server_side_cursor_fetches_on_demand_and_leaves_the_connection_usable() {
    let Some(url) = url() else { return eprintln!("skip: KERNWAY_MYSQL_URL not set") };
    run(async move {
        let mut conn = Conn::connect(&MysqlConfig::from_url(&url).unwrap()).await.unwrap();
        conn.query("SET SESSION cte_max_recursion_depth = 10000").await.unwrap();
        conn.query("DROP TABLE IF EXISTS kwc_c").await.unwrap();
        conn.query("CREATE TABLE kwc_c (n INT PRIMARY KEY)").await.unwrap();
        conn.query("INSERT INTO kwc_c WITH RECURSIVE s(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM s WHERE n < 5000) SELECT n FROM s")
            .await
            .unwrap();
        let db = MySqlDriver::connect_with(MysqlConfig::from_url(&url).unwrap(), 1).await.unwrap();
        // The server holds the cursor open: its status reports one.
        let r: Result<(), OrmError> = db
            .transaction(|tx| {
                Box::pin(async move {
                    let mut cur = tx.open_cursor("SELECT n FROM kwc_c WHERE n > ? ORDER BY n", params![0]).await?;
                    let first = tx.fetch(&mut cur, 100).await?.unwrap();
                    assert_eq!(first.rows.len(), 100);
                    // Another statement on the same connection mid-cursor.
                    let other = tx.query("SELECT COUNT(*) FROM kwc_c", &[]).await?;
                    assert_eq!(other.rows[0][0].as_i64(), Some(5000));
                    let mut total = first.rows.len();
                    let mut last = 0;
                    while let Some(b) = tx.fetch(&mut cur, 1000).await? {
                        total += b.rows.len();
                        last = b.rows.last().unwrap()[0].as_i64().unwrap();
                    }
                    assert_eq!((total, last), (5000, 5000));
                    // One left open is closed with the transaction.
                    let mut open = tx.open_cursor("SELECT n FROM kwc_c ORDER BY n", &[]).await?;
                    assert!(tx.fetch(&mut open, 1).await?.is_some());
                    Ok(())
                })
            })
            .await;
        r.unwrap();
        assert_eq!(db.query_scalar("SELECT 1", &[]).await.unwrap().as_i64(), Some(1), "connection usable after");
        // Statements prepared for cursors are closed: the cached ones remain.
        let open = conn.query("SHOW SESSION STATUS LIKE 'Prepared_stmt_count'").await;
        assert!(open.is_ok());
    });
}

// --- Unix-domain socket ---------------------------------------------------

fn socket() -> Option<String> {
    std::env::var("KERNWAY_MYSQL_SOCKET").ok()
}

fn socket_url(user: &str, pw: &str, path: &str, extra: &str) -> String {
    format!("mysql://{user}:{pw}@localhost/kernway_test?socket={path}{extra}")
}

#[test]
fn unix_socket_connects_through_the_server_socket_file() {
    let Some(path) = socket() else { return eprintln!("skip: KERNWAY_MYSQL_SOCKET not set") };
    run(async move {
        let cfg = MysqlConfig::from_url(&socket_url("kernway", "kernway_pw", &path, "")).unwrap();
        let mut conn = Conn::connect(&cfg).await.unwrap();
        assert!(conn.is_unix_socket());
        // The server sees a socket client: host "localhost" (TCP clients show
        // "ip:port"), and no TLS on it.
        let host = conn
            .query("SELECT host FROM information_schema.processlist WHERE id = CONNECTION_ID()")
            .await
            .unwrap();
        assert_eq!(session_status_first(host), "localhost");
        assert_eq!(session_status(conn.query("SHOW SESSION STATUS LIKE 'Ssl_cipher'").await.unwrap()), "");

        // The whole driver over it, compressed too.
        let db = MySqlDriver::connect(&socket_url("kernway", "kernway_pw", &path, "&compress=true")).await.unwrap();
        db.execute("DROP TABLE IF EXISTS kwc_s").await.unwrap();
        db.execute("CREATE TABLE kwc_s (n INT PRIMARY KEY, s TEXT)").await.unwrap();
        let rows: Vec<Vec<Param>> = (0..700).map(|n| vec![n.into(), "socket ".repeat(20).into()]).collect();
        assert_eq!(db.execute_many("INSERT INTO kwc_s VALUES (?, ?)", &rows).await.unwrap(), 700);
        assert_eq!(db.query_scalar("SELECT COUNT(*) FROM kwc_s", &[]).await.unwrap().as_i64(), Some(700));
    });
}

fn session_status_first(r: QueryResult) -> String {
    match r {
        QueryResult::Rows { rows, .. } => rows[0][0].clone().unwrap_or_default(),
        QueryResult::Ok { .. } => String::new(),
    }
}

#[test]
fn unix_socket_carries_a_caching_sha2_full_authentication() {
    let Some(path) = socket() else { return eprintln!("skip: KERNWAY_MYSQL_SOCKET not set") };
    run(async move {
        // Empty the server's auth cache so the next login is a full one: over
        // a socket the password goes in clear (the socket is local), as with
        // the official client — no TLS, no RSA key.
        let root_pw = std::env::var("KERNWAY_MYSQL_ROOT_PASSWORD").unwrap_or_else(|_| "secret".into());
        let root = MySqlDriver::connect(&socket_url("root", &root_pw, &path, "")).await.unwrap();
        root.execute("FLUSH PRIVILEGES").await.unwrap();
        let db = MySqlDriver::connect(&socket_url("kw_sha2", "sha2_pw", &path, "")).await.unwrap();
        assert_eq!(db.query_scalar("SELECT CURRENT_USER()", &[]).await.unwrap().as_str(), Some("kw_sha2@%"));
        // A wrong password is still refused.
        assert!(MySqlDriver::connect(&socket_url("kw_sha2", "wrong", &path, "")).await.is_err());
    });
}

#[test]
fn unix_socket_refuses_an_ssl_mode_that_requires_tls() {
    let Some(path) = socket() else { return eprintln!("skip: KERNWAY_MYSQL_SOCKET not set") };
    run(async move {
        let err = MySqlDriver::connect(&socket_url("kernway", "kernway_pw", &path, "&ssl-mode=required"))
            .await
            .err()
            .expect("refused");
        assert!(err.to_string().contains("Unix socket"), "{err}");
        let missing = MySqlDriver::connect(&socket_url("kernway", "kernway_pw", "/nonexistent/mysqld.sock", "")).await;
        assert!(missing.is_err());
    });
}
