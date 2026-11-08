//! Integration tests against a real MySQL server, exercising the **native async**
//! driver (per-core reactor, connection pool, async transactions).
//!
//! Run only when `KERNWAY_MYSQL_URL` is set and built with `--features mysql`;
//! otherwise each test skips so the normal `cargo test` stays green.
//!
//! Each test runs entirely inside one `Executor::block_on` — an `AsyncTcpStream`
//! is bound to the reactor that created it, so every op on a connection must run
//! on the same executor. Each test uses its own table to stay parallel-safe.
//!
//! ```bash
//! docker run -d --name kernway-mysql -e MYSQL_ROOT_PASSWORD=secret \
//!   -e MYSQL_DATABASE=kernway_test -p 3307:3306 \
//!   mysql:8.0 --default-authentication-plugin=mysql_native_password
//! KERNWAY_MYSQL_URL='mysql://kernway:kernway_pw@127.0.0.1:3307/kernway_test' \
//!   cargo test -p kernway-orm-mysql --features mysql --test integration -- --nocapture
//! ```
#![cfg(feature = "mysql")]

use kernway_orm_core::{entity::Entity, Driver, OrmError, Repository, Transaction, TransactionalDriver};
use kernway_orm_macro::entity;
use kernway_orm_mysql::MySqlDriver;
use serde::{Deserialize, Serialize};
use std::future::Future;

/// Run an async test body to completion on a fresh per-core executor.
fn run<F: Future<Output = ()>>(f: F) {
    rt_core::Executor::new().unwrap().block_on(f).unwrap();
}

fn url() -> Option<String> {
    std::env::var("KERNWAY_MYSQL_URL").ok()
}

async fn clear<T: Entity>(repo: &dyn Repository<T>) {
    for row in repo.find_all().await.unwrap() {
        repo.delete_by_id(&row.id()).await.unwrap();
    }
}

// --- crud_roundtrip --------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "it_accounts")]
struct Account {
    #[id(strategy = "auto")]
    id: u64,
    name: String,
    balance: i64,
}

#[test]
fn crud_roundtrip() {
    let Some(url) = url() else {
        eprintln!("skip crud_roundtrip: KERNWAY_MYSQL_URL not set");
        return;
    };
    run(async move {
        let db = MySqlDriver::connect(&url).await.expect("connect to MySQL");
        db.ensure_table::<Account>().await.unwrap();
        let repo = db.repository::<Account>();
        clear(&*repo).await;

        let a = repo.save(Account { id: 0, name: "alpha".into(), balance: 100 }).await.unwrap();
        assert!(a.id > 0, "auto-increment id should be assigned");

        assert_eq!(repo.find_by_id(&a.id).await.unwrap().unwrap(), a);

        let updated = repo
            .save(Account { id: a.id, name: "alpha2".into(), balance: 250 })
            .await
            .unwrap();
        assert_eq!(updated.name, "alpha2");
        assert_eq!(repo.find_by_id(&a.id).await.unwrap().unwrap().balance, 250);

        repo.save(Account { id: 0, name: "beta".into(), balance: 5 }).await.unwrap();
        let only = repo.query().filter_eq("name", "beta").fetch_all().await.unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(repo.count().await.unwrap(), 2);

        repo.delete_by_id(&a.id).await.unwrap();
        assert!(repo.find_by_id(&a.id).await.unwrap().is_none());
    });
}

// --- transaction commit (row lock) -----------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "it_wallet_commit")]
struct WalletCommit {
    #[id(strategy = "auto")]
    id: u64,
    balance: i64,
}

#[test]
fn transaction_commit_with_row_lock() {
    let Some(url) = url() else {
        eprintln!("skip transaction_commit_with_row_lock: KERNWAY_MYSQL_URL not set");
        return;
    };
    run(async move {
        let db = MySqlDriver::connect(&url).await.expect("connect");
        db.ensure_table::<WalletCommit>().await.unwrap();
        let repo = db.repository::<WalletCommit>();
        clear(&*repo).await;
        let id = repo.save(WalletCommit { id: 0, balance: 100 }).await.unwrap().id;

        let after = db
            .transaction(move |tx| {
                Box::pin(async move {
                    let wallets = tx.repository::<WalletCommit>();
                    let mut w = wallets.find_by_id_for_update(&id).await?.ok_or(OrmError::NotFound)?;
                    w.balance -= 30;
                    let balance = w.balance;
                    wallets.save(w).await?;
                    Ok(balance)
                })
            })
            .await
            .unwrap();

        assert_eq!(after, 70);
        assert_eq!(repo.find_by_id(&id).await.unwrap().unwrap().balance, 70);
    });
}

// --- transaction rollback --------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "it_wallet_rollback")]
struct WalletRollback {
    #[id(strategy = "auto")]
    id: u64,
    balance: i64,
}

#[test]
fn transaction_rolls_back_on_error() {
    let Some(url) = url() else {
        eprintln!("skip transaction_rolls_back_on_error: KERNWAY_MYSQL_URL not set");
        return;
    };
    run(async move {
        let db = MySqlDriver::connect(&url).await.expect("connect");
        db.ensure_table::<WalletRollback>().await.unwrap();
        let repo = db.repository::<WalletRollback>();
        clear(&*repo).await;
        let id = repo.save(WalletRollback { id: 0, balance: 100 }).await.unwrap().id;

        let result: Result<(), OrmError> = db
            .transaction(move |tx| {
                Box::pin(async move {
                    let wallets = tx.repository::<WalletRollback>();
                    let mut w = wallets.find_by_id_for_update(&id).await?.ok_or(OrmError::NotFound)?;
                    w.balance = 0;
                    wallets.save(w).await?;
                    Err(OrmError::Query("simulated failure after write".into()))
                })
            })
            .await;
        assert!(result.is_err());

        assert_eq!(repo.find_by_id(&id).await.unwrap().unwrap().balance, 100);
    });
}

// --- concurrency: FOR UPDATE prevents a lost update ------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "it_wallet_concurrent")]
struct WalletConcurrent {
    #[id(strategy = "auto")]
    id: u64,
    balance: i64,
}

/// Two concurrent transactions on two connections (two threads, two reactors)
/// each credit the same row by 10. `find_by_id_for_update` must make the second
/// wait for the first to commit, so both credits land (120, not a lost-update
/// 110).
#[test]
fn concurrent_for_update_no_lost_update() {
    let Some(url) = url() else {
        eprintln!("skip concurrent_for_update_no_lost_update: KERNWAY_MYSQL_URL not set");
        return;
    };

    // Setup row.
    let id = {
        let url = url.clone();
        let mut captured = 0u64;
        run(async {
            let db = MySqlDriver::connect(&url).await.expect("connect");
            db.ensure_table::<WalletConcurrent>().await.unwrap();
            let repo = db.repository::<WalletConcurrent>();
            clear(&*repo).await;
            captured = repo.save(WalletConcurrent { id: 0, balance: 100 }).await.unwrap().id;
        });
        captured
    };

    let credit = |hold_ms: u64| {
        let url = url.clone();
        std::thread::spawn(move || {
            run(async move {
                let db = MySqlDriver::connect(&url).await.expect("connect");
                db.transaction(move |tx| {
                    Box::pin(async move {
                        let wallets = tx.repository::<WalletConcurrent>();
                        let mut w =
                            wallets.find_by_id_for_update(&id).await?.ok_or(OrmError::NotFound)?;
                        // Hold the lock so the other txn is forced to wait.
                        rt_core::sleep(std::time::Duration::from_millis(hold_ms)).await;
                        w.balance += 10;
                        wallets.save(w).await?;
                        Ok(())
                    })
                })
                .await
                .unwrap();
            });
        })
    };

    let a = credit(400);
    std::thread::sleep(std::time::Duration::from_millis(100));
    let b = credit(0);
    a.join().unwrap();
    b.join().unwrap();

    run(async move {
        let db = MySqlDriver::connect(&url).await.expect("connect");
        let repo = db.repository::<WalletConcurrent>();
        assert_eq!(
            repo.find_by_id(&id).await.unwrap().unwrap().balance,
            120,
            "both credits must apply — a lost update would show 110"
        );
    });
}

// --- batch -----------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "it_batch")]
struct BatchRow {
    #[id(strategy = "auto")]
    id: u64,
    name: String,
    n: i64,
}

#[test]
fn batch_insert_select_delete() {
    let Some(url) = url() else {
        eprintln!("skip batch_insert_select_delete: KERNWAY_MYSQL_URL not set");
        return;
    };
    run(async move {
        let db = MySqlDriver::connect(&url).await.expect("connect");
        db.ensure_table::<BatchRow>().await.unwrap();
        let repo = db.repository::<BatchRow>();
        clear(&*repo).await;

        let input: Vec<BatchRow> = (1..=5)
            .map(|i| BatchRow { id: 0, name: format!("r{i}"), n: i })
            .collect();
        let saved = repo.save_all(input).await.unwrap();
        assert_eq!(saved.len(), 5);
        assert!(saved.iter().all(|r| r.id > 0));
        for pair in saved.windows(2) {
            assert_eq!(pair[1].id, pair[0].id + 1);
        }
        assert_eq!(repo.count().await.unwrap(), 5);

        let ids = vec![saved[0].id, saved[2].id, saved[4].id];
        let mut got = repo.find_all_by_ids(&ids).await.unwrap();
        got.sort_by_key(|r| r.id);
        assert_eq!(got.iter().map(|r| r.n).collect::<Vec<_>>(), vec![1, 3, 5]);

        repo.delete_all_by_ids(&ids).await.unwrap();
        assert_eq!(repo.count().await.unwrap(), 2);
    });
}

// --- prepared binds handle special characters & big ints -------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "it_binds")]
struct Tricky {
    #[id(strategy = "auto")]
    id: u64,
    label: String,
    amount: i64,
}

/// Binary parameter binding must round-trip quotes, backslashes, unicode, and
/// large integers without any text-escaping step.
#[test]
fn prepared_binds_special_values() {
    let Some(url) = url() else {
        eprintln!("skip prepared_binds_special_values: KERNWAY_MYSQL_URL not set");
        return;
    };
    run(async move {
        let db = MySqlDriver::connect(&url).await.expect("connect");
        db.ensure_table::<Tricky>().await.unwrap();
        let repo = db.repository::<Tricky>();
        clear(&*repo).await;

        let nasty = r#"O'Brien "quote" \ backslash; DROP TABLE x; -- 文字 🌿"#;
        let saved = repo
            .save(Tricky { id: 0, label: nasty.to_string(), amount: 9_223_372_036_854_775_123 })
            .await
            .unwrap();

        // Round-trips byte-for-byte through the binary protocol.
        let got = repo.find_by_id(&saved.id).await.unwrap().unwrap();
        assert_eq!(got.label, nasty);
        assert_eq!(got.amount, 9_223_372_036_854_775_123);

        // And it is a real bound value, not SQL: filtering by it finds exactly it.
        let found = repo.query().filter_eq("id", &saved.id.to_string()).fetch_one().await.unwrap();
        assert_eq!(found.unwrap().label, nasty);
    });
}
