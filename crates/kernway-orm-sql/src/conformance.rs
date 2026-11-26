//! The conformance suite: the behaviour every SQL backend must show against a
//! live database. A driver crate provides a [`Harness`] and calls
//! [`run_all`] from an integration test; a backend that fails any scenario
//! does not conform.
//!
//! Each scenario drops and recreates its own `conf_*` table, so the suite can
//! run repeatedly against a scratch database. Where databases legitimately
//! differ, the suite asks the dialect (e.g. Oracle stores `''` as NULL).

#![allow(missing_docs)]

use kernway_orm_core::{
    entity::Entity, error::OrmError, spec::Spec, transaction::{Transaction, TransactionalDriver},
    Driver, SqlDialect,
};
use kernway_orm_macro::entity;
use serde::{Deserialize, Serialize};
use std::future::Future;

/// How the suite reaches one backend.
pub trait Harness: Clone + Send + Sync + 'static {
    /// The driver under test.
    type Driver: Driver + TransactionalDriver + 'static;
    /// Open a driver (on the current executor).
    fn connect(&self) -> impl Future<Output = Self::Driver>;
    /// Run one raw statement (DDL).
    fn exec<'a>(&'a self, db: &'a Self::Driver, sql: &'a str) -> impl Future<Output = Result<(), OrmError>> + 'a;
    /// The backend's dialect.
    fn dialect(&self) -> &'static dyn SqlDialect;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "conf_items")]
pub struct Item {
    #[id(strategy = "auto")]
    pub id: i64,
    pub name: String,
    pub qty: i64,
    pub price: f64,
    pub active: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "conf_files")]
pub struct File {
    #[id(strategy = "auto")]
    pub id: i64,
    pub body: String,
    pub data: Vec<u8>,
    pub thumb: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "conf_members")]
pub struct Member {
    #[id(strategy = "manual")]
    pub code: String,
    #[column(unique)]
    pub email: String,
    pub level: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "conf_wallets")]
pub struct Wallet {
    #[id(strategy = "auto")]
    pub id: i64,
    pub balance: i64,
}

fn block<F: Future<Output = ()>>(f: F) {
    rt_core::Executor::new().expect("executor").block_on(f).expect("executor run");
}

async fn fresh_table<T: Entity, H: Harness>(h: &H, db: &H::Driver) {
    let d = h.dialect();
    let _ = h.exec(db, &format!("DROP TABLE {}", d.quote_identifier(T::table_name()))).await;
    h.exec(db, &d.create_table_sql(T::table_name(), T::columns()))
        .await
        .unwrap_or_else(|e| panic!("create {}: {e}", T::table_name()));
}

fn item(name: &str, qty: i64, price: f64, active: bool, note: Option<&str>) -> Item {
    Item { id: 0, name: name.into(), qty, price, active, note: note.map(String::from) }
}

/// Run every scenario; panics with the scenario name on the first failure.
pub fn run_all<H: Harness>(h: H) {
    for (name, scenario) in [
        ("crud", crud::<H> as fn(H)),
        ("values", values::<H>),
        ("batch", batch::<H>),
        ("query_builder", query_builder::<H>),
        ("binary_and_long_text", binary_and_long_text::<H>),
        ("unique_violation", unique_violation::<H>),
        ("transactions", transactions::<H>),
        ("row_lock_no_lost_update", row_lock_no_lost_update::<H>),
    ] {
        eprintln!("conformance: {name}");
        scenario(h.clone());
    }
}

fn crud<H: Harness>(h: H) {
    block(async move {
        let db = h.connect().await;
        fresh_table::<Item, H>(&h, &db).await;
        let repo = db.repository::<Item>();
        let a = repo.save(item("alpha", 3, 9.5, true, Some("first"))).await.unwrap();
        assert!(a.id > 0, "a generated key is assigned");
        assert_eq!(repo.find_by_id(&a.id).await.unwrap().as_ref(), Some(&a));
        let b = repo.save(item("beta", 5, 1.25, false, None)).await.unwrap();
        assert_ne!(a.id, b.id);
        let updated = repo.save(Item { qty: 30, note: None, ..a.clone() }).await.unwrap();
        assert_eq!(repo.find_by_id(&a.id).await.unwrap(), Some(updated));
        assert_eq!(repo.count().await.unwrap(), 2);
        assert!(repo.exists_by_id(&b.id).await.unwrap());
        repo.delete_by_id(&b.id).await.unwrap();
        assert!(!repo.exists_by_id(&b.id).await.unwrap());
        assert_eq!(repo.find_by_id(&b.id).await.unwrap(), None);
        assert_eq!(repo.find_all().await.unwrap().len(), 1);
    });
}

fn values<H: Harness>(h: H) {
    block(async move {
        let db = h.connect().await;
        fresh_table::<Item, H>(&h, &db).await;
        let repo = db.repository::<Item>();
        let cases = [
            item("i64 max", i64::MAX, 0.0, true, None),
            item("i64 min", i64::MIN, -0.0, false, None),
            item("zero", 0, 1e-300, true, Some("x")),
            item("float", -42, -2.25, false, Some("y")),
            item("big float", 1, 1.7976931348623157e308, true, None),
            item("Việt Nam ✓ 😀", 7, 3.5, true, Some("ghi chú tiếng Việt 😀")),
            item("quote ' and \\ backslash", 1, 0.5, false, Some("a'b\"c")),
            item("", 2, 0.0, true, Some("")),
        ];
        for c in cases {
            let saved = repo.save(c.clone()).await.unwrap_or_else(|e| panic!("save {:?}: {e}", c.name));
            let got = repo.find_by_id(&saved.id).await.unwrap().unwrap();
            let mut want = Item { id: saved.id, ..c };
            if h.dialect().empty_string_is_null() && want.note.as_deref() == Some("") {
                want.note = None; // Oracle: '' is NULL
            }
            assert_eq!(got, want, "value round trip");
        }
    });
}

fn batch<H: Harness>(h: H) {
    block(async move {
        let db = h.connect().await;
        fresh_table::<Item, H>(&h, &db).await;
        let repo = db.repository::<Item>();
        let rows: Vec<Item> = (0..50).map(|i| item(&format!("n{i:02}"), i, f64::from(i as i32) / 2.0, i % 2 == 0, None)).collect();
        let saved = repo.save_all(rows.clone()).await.unwrap();
        assert_eq!(saved.len(), 50);
        let mut ids: Vec<i64> = saved.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 50, "distinct generated keys");
        for (s, r) in saved.iter().zip(&rows) {
            assert_eq!(Item { id: s.id, ..r.clone() }, *s, "returned in input order with data");
        }
        let some: Vec<i64> = saved.iter().step_by(10).map(|s| s.id).collect();
        let found = repo.find_all_by_ids(&some).await.unwrap();
        assert_eq!(found.len(), some.len());
        repo.delete_all_by_ids(&some).await.unwrap();
        assert_eq!(repo.count().await.unwrap(), 45);
        assert!(repo.find_all_by_ids(&[]).await.unwrap().is_empty());
    });
}

fn query_builder<H: Harness>(h: H) {
    block(async move {
        let db = h.connect().await;
        fresh_table::<Item, H>(&h, &db).await;
        let repo = db.repository::<Item>();
        for i in 1..=20i64 {
            let note = if i % 4 == 0 { None } else { Some(format!("note {i}")) };
            repo.save(Item { id: 0, name: format!("item-{i:02}"), qty: i, price: i as f64 * 1.5, active: i % 2 == 0, note }).await.unwrap();
        }
        let names = |v: Vec<Item>| v.into_iter().map(|i| i.qty).collect::<Vec<_>>();
        assert_eq!(names(repo.query().filter_eq("qty", "7").fetch_all().await.unwrap()), [7]);
        assert_eq!(repo.query().filter_ne("qty", "7").fetch_count().await.unwrap(), 19);
        assert_eq!(repo.query().filter_gt("qty", "17").order_by_asc("qty").fetch_all().await.map(names).unwrap(), [18, 19, 20]);
        assert_eq!(repo.query().filter_lt("qty", "3").order_by_asc("qty").fetch_all().await.map(names).unwrap(), [1, 2]);
        assert_eq!(repo.query().filter_gte("qty", "19").fetch_count().await.unwrap(), 2);
        assert_eq!(repo.query().filter_lte("qty", "2").fetch_count().await.unwrap(), 2);
        assert_eq!(repo.query().filter_like("name", "item-1%").fetch_count().await.unwrap(), 10);
        assert_eq!(
            repo.query().filter_in("qty", vec!["2".into(), "4".into(), "99".into()]).order_by_desc("qty").fetch_all().await.map(names).unwrap(),
            [4, 2]
        );
        assert_eq!(repo.query().filter_in("qty", vec![]).fetch_count().await.unwrap(), 0, "empty IN matches nothing");
        assert_eq!(repo.query().filter_between("qty", "5", "8").fetch_count().await.unwrap(), 4);
        assert_eq!(repo.query().filter_is_null("note").fetch_count().await.unwrap(), 5);
        assert_eq!(repo.query().filter_is_not_null("note").fetch_count().await.unwrap(), 15);
        assert_eq!(repo.query().filter_eq("active", "true").fetch_count().await.unwrap(), 10);
        assert_eq!(repo.query().filter_gt("price", "28").fetch_count().await.unwrap(), 2, "float comparison");
        let spec = Spec::Or(
            Box::new(Spec::Eq("qty", "1".into())),
            Box::new(Spec::And(Box::new(Spec::Gte("qty", "15".into())), Box::new(Spec::Not(Box::new(Spec::IsNull("note")))))),
        );
        assert_eq!(repo.query().filter_spec(spec).order_by_asc("qty").fetch_all().await.map(names).unwrap(), [1, 15, 17, 18, 19]);
        assert_eq!(repo.query().order_by_desc("qty").limit(3).fetch_all().await.map(names).unwrap(), [20, 19, 18]);
        assert_eq!(repo.query().order_by_asc("qty").offset(17).fetch_all().await.map(names).unwrap(), [18, 19, 20], "offset without limit");
        assert_eq!(repo.query().order_by_asc("qty").limit(2).offset(5).fetch_all().await.map(names).unwrap(), [6, 7]);
        let page = repo.query().filter_gt("qty", "10").order_by_asc("qty").fetch_page(1, 4).await.unwrap();
        assert_eq!(page.total, 10);
        assert_eq!(page.items.iter().map(|i| i.qty).collect::<Vec<_>>(), [15, 16, 17, 18]);
        assert_eq!(repo.query().filter_eq("qty", "3").fetch_one().await.unwrap().map(|i| i.qty), Some(3));
        assert_eq!(repo.query().filter_eq("qty", "999").fetch_one().await.unwrap(), None);
        assert!(repo.query().filter_eq("no_such_field", "1").fetch_all().await.is_err(), "unknown field is an error");
        // A filter value is a bound parameter, never SQL.
        assert_eq!(repo.query().filter_eq("name", "x' OR '1'='1").fetch_count().await.unwrap(), 0);
    });
}

fn binary_and_long_text<H: Harness>(h: H) {
    block(async move {
        let db = h.connect().await;
        fresh_table::<File, H>(&h, &db).await;
        let repo = db.repository::<File>();
        let every_byte: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let big: Vec<u8> = (0..1_000_000u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
        let long_text = "kernway ".repeat(375); // 3000 bytes: within every backend's String limit
        for f in [
            File { id: 0, body: long_text.clone(), data: every_byte.clone(), thumb: None },
            File { id: 0, body: "Việt ✓".into(), data: big.clone(), thumb: Some(vec![0, 255, 1]) },
            File { id: 0, body: String::new(), data: vec![], thumb: Some(vec![9]) },
        ] {
            let saved = repo.save(f.clone()).await.unwrap_or_else(|e| panic!("save {} bytes: {e}", f.data.len()));
            let got = repo.find_by_id(&saved.id).await.unwrap().unwrap();
            assert_eq!(got.body, f.body);
            assert!(got.data == f.data, "{} bytes intact", f.data.len());
            assert_eq!(got.thumb, f.thumb);
        }
        assert_eq!(repo.find_all().await.unwrap().len(), 3);
    });
}

fn unique_violation<H: Harness>(h: H) {
    block(async move {
        let db = h.connect().await;
        fresh_table::<Member, H>(&h, &db).await;
        let repo = db.repository::<Member>();
        repo.save(Member { code: "A1".into(), email: "a@x.io".into(), level: 1 }).await.unwrap();
        let e = repo.save(Member { code: "B2".into(), email: "a@x.io".into(), level: 2 }).await.unwrap_err();
        assert!(matches!(e, OrmError::UniqueViolation { .. }), "duplicate unique value: {e}");
        // A manual key that exists is updated, not inserted.
        repo.save(Member { code: "A1".into(), email: "a@x.io".into(), level: 9 }).await.unwrap();
        assert_eq!(repo.find_by_id(&"A1".to_string()).await.unwrap().unwrap().level, 9);
        assert_eq!(repo.count().await.unwrap(), 1);
    });
}

fn transactions<H: Harness>(h: H) {
    block(async move {
        let db = h.connect().await;
        fresh_table::<Wallet, H>(&h, &db).await;
        let repo = db.repository::<Wallet>();
        let w = repo.save(Wallet { id: 0, balance: 100 }).await.unwrap();
        let id = w.id;
        db.transaction(move |tx| {
            Box::pin(async move {
                let r = tx.repository::<Wallet>();
                let mut w = r.find_by_id_for_update(&id).await?.ok_or(OrmError::NotFound)?;
                w.balance -= 30;
                r.save(w).await?;
                r.save(Wallet { id: 0, balance: 1 }).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        assert_eq!(repo.find_by_id(&id).await.unwrap().unwrap().balance, 70, "committed");
        assert_eq!(repo.count().await.unwrap(), 2, "insert inside the transaction committed");
        let r: Result<(), OrmError> = db
            .transaction(move |tx| {
                Box::pin(async move {
                    let r = tx.repository::<Wallet>();
                    let mut w = r.find_by_id(&id).await?.ok_or(OrmError::NotFound)?;
                    w.balance = 0;
                    r.save(w).await?;
                    r.delete_by_id(&id).await?;
                    Err(OrmError::Query("abort".into()))
                })
            })
            .await;
        assert!(r.is_err());
        assert_eq!(repo.find_by_id(&id).await.unwrap().unwrap().balance, 70, "rolled back");
        // Outside a transaction every write is committed on its own: a fresh
        // connection sees it.
        repo.save(Wallet { id, balance: 71 }).await.unwrap();
        let other = h.connect().await;
        assert_eq!(other.repository::<Wallet>().find_by_id(&id).await.unwrap().unwrap().balance, 71);
    });
}

fn row_lock_no_lost_update<H: Harness>(h: H) {
    let id = {
        let h = h.clone();
        let mut id = 0;
        block(async {
            let db = h.connect().await;
            fresh_table::<Wallet, H>(&h, &db).await;
            id = db.repository::<Wallet>().save(Wallet { id: 0, balance: 100 }).await.unwrap().id;
        });
        id
    };
    let credit = |hold_ms: u64| {
        let h = h.clone();
        std::thread::spawn(move || {
            block(async move {
                let db = h.connect().await;
                db.transaction(move |tx| {
                    Box::pin(async move {
                        let r = tx.repository::<Wallet>();
                        let mut w = r.find_by_id_for_update(&id).await?.ok_or(OrmError::NotFound)?;
                        rt_core::sleep(std::time::Duration::from_millis(hold_ms)).await;
                        w.balance += 10;
                        r.save(w).await?;
                        Ok(())
                    })
                })
                .await
                .unwrap();
            })
        })
    };
    let a = credit(400);
    std::thread::sleep(std::time::Duration::from_millis(100));
    let b = credit(0);
    a.join().unwrap();
    b.join().unwrap();
    block(async move {
        let db = h.connect().await;
        assert_eq!(db.repository::<Wallet>().find_by_id(&id).await.unwrap().unwrap().balance, 120, "no lost update");
    });
}

