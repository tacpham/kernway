//! Large text and binary values (MySQL LONGTEXT / LONGBLOB) through the ORM,
//! over TLS and plaintext. Skips unless `KERNWAY_MYSQL_URL` is set.
#![cfg(feature = "mysql")]

use kernway_orm_core::Driver;
use kernway_orm_macro::entity;
use kernway_orm_mysql::MySqlDriver;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "it_lobs")]
struct Doc {
    #[id(strategy = "auto")]
    id: u64,
    body: String,
    data: Vec<u8>,
    thumb: Option<Vec<u8>>,
}

fn run<F: std::future::Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

#[test]
fn large_text_and_binary_round_trip_over_tls_and_plaintext() {
    let Ok(url) = std::env::var("KERNWAY_MYSQL_URL") else {
        return eprintln!("skip: KERNWAY_MYSQL_URL not set");
    };
    let base = url.split('?').next().unwrap().to_string();
    run(async move {
        for mode in ["preferred", "disabled"] {
            let db = MySqlDriver::connect(&format!("{base}?ssl-mode={mode}")).await.unwrap();
            db.execute("DROP TABLE IF EXISTS it_lobs").await.unwrap();
            db.ensure_table::<Doc>().await.unwrap();
            let repo = db.repository::<Doc>();

            // 70 KB of multi-byte text (over TEXT's 64 KB), every byte value,
            // and a 20 MiB blob: over the 16 MiB packet limit, so the write is
            // split into continuation packets (and, over TLS, fed to rustls in
            // pieces).
            let body = "Việt ✓ ".repeat(7_000);
            let data: Vec<u8> = (0..=255u8).cycle().take(5_000).collect();
            let huge: Vec<u8> = (0..20 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();

            let a = repo
                .save(Doc { id: 0, body: body.clone(), data: data.clone(), thumb: None })
                .await
                .unwrap_or_else(|e| panic!("{mode}: save small: {e}"));
            let b = repo
                .save(Doc { id: 0, body: "big".into(), data: huge.clone(), thumb: Some(vec![0, 255, 1]) })
                .await
                .unwrap_or_else(|e| panic!("{mode}: save 20 MiB: {e}"));

            // Binary protocol (prepared statement).
            let got = repo.find_by_id(&a.id).await.unwrap().unwrap();
            assert_eq!(got.body, body, "{mode}");
            assert_eq!(got.data, data, "{mode}");
            assert_eq!(got.thumb, None, "{mode}");
            let got = repo.find_by_id(&b.id).await.unwrap().unwrap();
            assert!(got.data == huge, "{mode}: 20 MiB blob intact");
            assert_eq!(got.thumb, Some(vec![0, 255, 1]), "{mode}");

            // Text protocol.
            let all = repo.find_all().await.unwrap();
            assert_eq!(all.len(), 2, "{mode}");
            assert!(all.iter().any(|d| d.data == huge), "{mode}: text protocol reads the blob too");
            db.execute("DROP TABLE it_lobs").await.unwrap();
        }
    });
}

/// Writes of every size class, each checked by the server's own LENGTH / MD5
/// of what it received. One bound parameter makes the COM_STMT_EXECUTE payload
/// exactly `18 + n` bytes (`SET @x = ?`) for 251 <= n < 2^24, so `n = 0xFFFFFF - 18` fills one
/// maximum-size packet and must be followed by an empty one.
#[test]
fn writes_small_medium_and_large_values_byte_exact() {
    use kernway_orm_mysql::{BindValue, Conn, MysqlConfig, QueryResult};
    use md5::Digest;
    let Ok(url) = std::env::var("KERNWAY_MYSQL_URL") else {
        return eprintln!("skip: KERNWAY_MYSQL_URL not set");
    };
    let base = url.split('?').next().unwrap().to_string();
    const MAX: usize = 0xFF_FFFF;
    let exact = MAX - 18;
    let sizes = [
        0, 1, 250, 251, 252, 253, // lenenc 1-byte / 3-byte boundary
        16_383, 16_384, 16_385, // one TLS record
        65_535, 65_536, 65_537, // rustls plaintext buffer, MySQL TEXT limit
        1 << 20,
        exact - 1, exact, exact + 1, // payload = 16 MiB - 2 / - 1 / exactly 16 MiB
        MAX, 20 << 20,
    ];
    run(async move {
        for mode in ["preferred", "disabled"] {
            let cfg = MysqlConfig::from_url(&format!("{base}?ssl-mode={mode}")).unwrap();
            let mut c = Conn::connect(&cfg).await.unwrap();
            for &n in &sizes {
                let data: Vec<u8> = (0..n as u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
                let want_md5: String = md5::Md5::digest(&data).iter().map(|b| format!("{b:02x}")).collect();
                // `SET @x = ?` (not a `SELECT ? ...` derived table, whose column
                // type MySQL fixes at a cached statement's first execution).
                c.exec_prepared("SET @x = ?", &[BindValue::Bytes(data)])
                    .await
                    .unwrap_or_else(|e| panic!("{mode}: {n} bytes: {e}"));
                let r = c.query("SELECT LENGTH(@x), MD5(@x)").await.unwrap();
                let QueryResult::Rows { rows, .. } = r else { panic!("{mode}: {n}: expected a row") };
                let (len, md5) = (rows[0][0].clone(), rows[0][1].clone());
                if n == 0 {
                    // An empty binary value reads back as NULL or "".
                    assert!(len.as_deref().is_none_or(|l| l == "0"), "{mode}: 0 bytes -> {len:?}");
                    continue;
                }
                assert_eq!(len.as_deref(), Some(n.to_string().as_str()), "{mode}: length of {n}");
                assert_eq!(md5.as_deref(), Some(want_md5.as_str()), "{mode}: content of {n}");
            }
            // The connection is still in step after the largest writes.
            let r = c.query("SELECT 1").await.unwrap();
            assert!(matches!(r, QueryResult::Rows { .. }), "{mode}");
        }
    });
}
