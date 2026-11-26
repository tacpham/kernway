//! Throughput smoke-benchmark against a real MySQL server — the numbers the
//! driver's hot paths are tuned by (prepared CRUD round trips, large result sets).
//!
//! Skips unless `KERNWAY_MYSQL_URL` is set; run in release for meaningful numbers:
//!
//! ```bash
//! KERNWAY_MYSQL_URL='mysql://kernway:kernway_pw@127.0.0.1:3307/kernway_test' \
//!   cargo test -p kernway-orm-mysql --features mysql --release --test perf -- --nocapture
//! ```
#![cfg(feature = "mysql")]

use kernway_orm_core::{Driver, Repository};
use kernway_orm_macro::entity;
use kernway_orm_mysql::MySqlDriver;
use serde::{Deserialize, Serialize};
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "perf_items")]
struct Item {
    #[id(strategy = "auto")]
    id: u64,
    name: String,
    qty: i64,
}

const ROWS: u64 = 1_000;
const POINT_OPS: u32 = 2_000;
const SCANS: u32 = 100;

fn report(label: &str, ops: u32, started: Instant) {
    let elapsed = started.elapsed();
    let per_op = elapsed / ops;
    println!("{label:<28} {ops:>6} ops  {elapsed:>10.2?}  {per_op:>9.2?}/op");
}

#[test]
fn perf_smoke() {
    let Ok(url) = std::env::var("KERNWAY_MYSQL_URL") else {
        eprintln!("skip perf_smoke: KERNWAY_MYSQL_URL not set");
        return;
    };
    rt_core::Executor::new()
        .unwrap()
        .block_on(async move {
            let db = MySqlDriver::connect(&url).await.expect("connect to MySQL");
            db.execute("DROP TABLE IF EXISTS perf_items").await.unwrap();
            db.ensure_table::<Item>().await.unwrap();
            let repo = db.repository::<Item>();

            let batch: Vec<Item> = (0..ROWS)
                .map(|i| Item { id: 0, name: format!("item-{i:05}"), qty: i as i64 })
                .collect();
            let t = Instant::now();
            let saved = repo.save_all(batch).await.unwrap();
            report("save_all (1 batch)", 1, t);
            let first = saved[0].id;

            let t = Instant::now();
            for i in 0..POINT_OPS {
                let id = first + u64::from(i) % ROWS;
                assert!(repo.find_by_id(&id).await.unwrap().is_some());
            }
            report("find_by_id (prepared)", POINT_OPS, t);

            let t = Instant::now();
            for i in 0..POINT_OPS {
                let id = first + u64::from(i) % ROWS;
                repo.save(Item { id, name: format!("upd-{i}"), qty: i64::from(i) })
                    .await
                    .unwrap();
            }
            report("save update (prepared)", POINT_OPS, t);

            let t = Instant::now();
            for _ in 0..SCANS {
                assert_eq!(repo.find_all().await.unwrap().len() as u64, ROWS);
            }
            report("find_all 1000 rows (text)", SCANS, t);

            let t = Instant::now();
            for _ in 0..SCANS {
                let rows = repo.query().filter_gt("qty", "-1").fetch_all().await.unwrap();
                assert_eq!(rows.len() as u64, ROWS);
            }
            report("query 1000 rows (builder)", SCANS, t);

            db.execute("DROP TABLE perf_items").await.unwrap();
        })
        .unwrap();
}
