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
use crate::{col, params, Agg, Cmp, Param, SqlConnect, SqlDb, Value};
use kernway_orm_macro::entity;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::ops::Deref;

/// How the suite reaches one backend.
pub trait Harness: Clone + Send + Sync + 'static {
    /// The pool connector of the backend.
    type Connect: SqlConnect;
    /// The driver under test (a [`SqlDb`] wrapper).
    type Driver: Driver + TransactionalDriver<Tx = crate::SqlTx<Self::Connect>> + Deref<Target = SqlDb<Self::Connect>> + 'static;
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

/// A projection of hand-written SQL, mapped by column name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "conf_item_totals")]
pub struct ItemTotal {
    #[id(strategy = "manual")]
    pub active: bool,
    pub n: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "conf_wallets")]
pub struct Wallet {
    #[id(strategy = "auto")]
    pub id: i64,
    pub balance: i64,
}

/// Joined to [`Item`] by `item_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "conf_tags")]
pub struct Tag {
    #[id(strategy = "auto")]
    pub id: i64,
    pub item_id: i64,
    pub label: String,
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
        ("raw_sql", raw_sql::<H>),
        ("select_joins_and_aggregates", select_joins_and_aggregates::<H>),
        ("execute_many_failure_semantics", execute_many_failure_semantics::<H>),
        ("server_cursors", server_cursors::<H>),
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


fn raw_sql<H: Harness>(h: H) {
    block(async move {
        let db = h.connect().await;
        fresh_table::<Item, H>(&h, &db).await;
        let repo = db.repository::<Item>();
        for i in 1..=6i64 {
            repo.save(item(&format!("r{i}"), i, i as f64 / 4.0, i % 2 == 0, None)).await.unwrap();
        }
        let d = h.dialect();
        let q = |s: &str| d.quote_identifier(s);
        let (t, name, qty, price, active) = (q("conf_items"), q("name"), q("qty"), q("price"), q("active"));
        let p = |i: usize| d.placeholder(i);

        // Any query, bound parameters, typed values.
        let rows = db
            .query(&format!("SELECT {name}, {qty}, {price} FROM {t} WHERE {qty} > {} ORDER BY {qty} DESC", p(0)), params![4])
            .await
            .unwrap();
        assert_eq!(rows.rows.len(), 2);
        assert_eq!(rows.get(0, "name").and_then(Value::as_str), Some("r6"));
        assert_eq!(rows.get(0, "qty").and_then(Value::as_i64), Some(6));
        assert_eq!(rows.get(1, "price").and_then(Value::as_f64), Some(1.25));

        // Aggregates grouped, mapped onto an entity by column name.
        let mut totals: Vec<ItemTotal> = db
            .query_as(
                &format!("SELECT {active} AS {}, COUNT(*) AS {}, SUM({qty}) AS {} FROM {t} GROUP BY {active}", q("active"), q("n"), q("total")),
                &[],
            )
            .await
            .unwrap();
        totals.sort_by_key(|t| t.active);
        assert_eq!(
            totals,
            vec![ItemTotal { active: false, n: 3, total: 9 }, ItemTotal { active: true, n: 3, total: 12 }]
        );
        assert_eq!(
            db.query_scalar(&format!("SELECT COUNT(*) FROM {t} WHERE {name} LIKE {}", p(0)), params!["r%"]).await.unwrap().as_i64(),
            Some(6)
        );

        // Statements report rows affected; streaming visits every row.
        let n = db.execute_with(&format!("UPDATE {t} SET {qty} = {qty} * 10 WHERE {qty} <= {}", p(0)), params![3]).await.unwrap();
        assert_eq!(n, 3);
        let mut sum = 0;
        db.query_each(&format!("SELECT {qty} FROM {t}"), &[], |r| {
            sum += r[0].as_i64().unwrap();
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(sum, 10 + 20 + 30 + 4 + 5 + 6);

        // NULL and text parameters.
        let none: Option<String> = None;
        let n = db
            .execute_with(&format!("UPDATE {t} SET {} = {} WHERE {name} = {}", q("note"), p(0), p(1)), &[Param::from(none), "r1".into()])
            .await
            .unwrap();
        assert_eq!(n, 1);

        // Many rows of one statement (array DML where the backend has it).
        let rows: Vec<Vec<Param>> = (0..30i64).map(|i| vec![(100 + i).into(), format!("m{i}").into()]).collect();
        let n = db
            .execute_many(&format!("UPDATE {t} SET {qty} = {} WHERE {name} = {}", p(0), p(1)), &rows)
            .await
            .unwrap();
        assert_eq!(n, 0, "no row named m*");
        let rows: Vec<Vec<Param>> = (1..=6i64).map(|i| vec![(1000 + i).into(), format!("r{i}").into()]).collect();
        let n = db.execute_many(&format!("UPDATE {t} SET {qty} = {} WHERE {name} = {}", p(0), p(1)), &rows).await.unwrap();
        assert_eq!(n, 6);
        assert_eq!(
            db.query_scalar(&format!("SELECT SUM({qty}) FROM {t}"), &[]).await.unwrap().as_i64(),
            Some(6 * 1000 + 21)
        );

        // Hand-written SQL inside a transaction rolls back with it.
        let r: Result<(), OrmError> = db
            .transaction(move |tx| {
                let (t, qty) = (t.clone(), qty.clone());
                Box::pin(async move {
                    tx.execute_with(&format!("DELETE FROM {t} WHERE {qty} > {}", p(0)), params![0]).await?;
                    let left = tx.query(&format!("SELECT COUNT(*) FROM {t}"), &[]).await?;
                    assert_eq!(left.rows[0][0].as_i64(), Some(0));
                    Err(OrmError::Query("abort".into()))
                })
            })
            .await;
        assert!(r.is_err());
        assert_eq!(repo.count().await.unwrap(), 6, "raw DELETE rolled back");
    });
}

fn select_joins_and_aggregates<H: Harness>(h: H) {
    block(async move {
        let db = h.connect().await;
        fresh_table::<Item, H>(&h, &db).await;
        fresh_table::<Tag, H>(&h, &db).await;
        let items = db.repository::<Item>();
        let tags = db.repository::<Tag>();
        let mut ids = Vec::new();
        for i in 1..=6i64 {
            ids.push(items.save(item(&format!("s{i}"), i, i as f64 * 0.5, i % 2 == 0, None)).await.unwrap().id);
        }
        // s1: a, b · s2: a · s3: none · s4: b, c, d · s5: a · s6: none
        for (item_ix, label) in [(0, "a"), (0, "b"), (1, "a"), (3, "b"), (3, "c"), (3, "d"), (4, "a")] {
            tags.save(Tag { id: 0, item_id: ids[item_ix], label: label.into() }).await.unwrap();
        }

        // GROUP BY with aggregates, read into a plain struct.
        #[derive(Debug, Deserialize, PartialEq)]
        struct ByActive {
            active: bool,
            n: i64,
            total: i64,
            avg_price: f64,
            max_name: String,
        }
        let got: Vec<ByActive> = db
            .select::<Item>()
            .group_by(col::<Item>("active"))
            .aggregate("n", Agg::Count)
            .aggregate("total", Agg::Sum(col::<Item>("qty")))
            .aggregate("avg_price", Agg::Avg(col::<Item>("price")))
            .aggregate("max_name", Agg::Max(col::<Item>("name")))
            .order_by("active", true)
            .fetch_as()
            .await
            .unwrap();
        assert_eq!(
            got,
            [
                ByActive { active: false, n: 3, total: 9, avg_price: 1.5, max_name: "s5".into() },
                ByActive { active: true, n: 3, total: 12, avg_price: 2.0, max_name: "s6".into() },
            ]
        );

        // JOIN + WHERE on both sides + GROUP BY + HAVING + ORDER BY + LIMIT.
        #[derive(Debug, Deserialize, PartialEq)]
        struct TagCount {
            name: String,
            tags: i64,
            labels: i64,
        }
        let q = db
            .select::<Item>()
            .join::<Tag>(col::<Item>("id"), "item_id")
            .filter(Spec::gt("qty", "0"))
            .filter_on::<Tag>(Spec::ne("label", "zzz"))
            .group_by(col::<Item>("name"))
            .aggregate("tags", Agg::CountOf(col::<Tag>("id")))
            .aggregate("labels", Agg::CountDistinct(col::<Tag>("label")))
            .having("tags", Cmp::Gte, 2)
            .order_by("tags", false);
        assert_eq!(q.to_sql().1.len(), 3, "values are bound, never inlined");
        let busiest: Vec<TagCount> = q.fetch_as().await.unwrap();
        assert_eq!(
            busiest,
            [TagCount { name: "s4".into(), tags: 3, labels: 3 }, TagCount { name: "s1".into(), tags: 2, labels: 2 }]
        );
        let first: Vec<TagCount> = db
            .select::<Item>()
            .join::<Tag>(col::<Item>("id"), "item_id")
            .group_by(col::<Item>("name"))
            .aggregate("tags", Agg::Count)
            .aggregate("labels", Agg::CountDistinct(col::<Tag>("label")))
            .order_by("tags", false)
            .order_by("name", true)
            .limit(1)
            .fetch_as()
            .await
            .unwrap();
        assert_eq!(first, [TagCount { name: "s4".into(), tags: 3, labels: 3 }]);

        // Counting groups, and rows of a join.
        let groups = db
            .select::<Item>()
            .join::<Tag>(col::<Item>("id"), "item_id")
            .group_by(col::<Item>("id"))
            .aggregate("n", Agg::Count)
            .fetch_count()
            .await
            .unwrap();
        assert_eq!(groups, 4, "items with at least one tag");
        let tagged_a = db
            .select::<Item>()
            .join::<Tag>(col::<Item>("id"), "item_id")
            .filter_on::<Tag>(Spec::eq("label", "a"))
            .fetch_count()
            .await
            .unwrap();
        assert_eq!(tagged_a, 3);

        // Entities through a join: the root rows, and pairs with a LEFT JOIN.
        let mut with_b = db
            .select::<Item>()
            .join::<Tag>(col::<Item>("id"), "item_id")
            .filter_on::<Tag>(Spec::eq("label", "b"))
            .fetch_all()
            .await
            .unwrap();
        with_b.sort_by_key(|i| i.qty);
        assert_eq!(with_b.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["s1", "s4"]);
        let pairs = db
            .select::<Item>()
            .left_join::<Tag>(col::<Item>("id"), "item_id")
            .filter(Spec::lte("qty", "3"))
            .order_by_col(col::<Item>("qty"), true)
            .order_by_col(col::<Tag>("label"), true)
            .fetch_joined::<Tag>()
            .await
            .unwrap();
        let shape: Vec<(String, Option<String>)> =
            pairs.iter().map(|(i, t)| (i.name.clone(), t.as_ref().map(|t| t.label.clone()))).collect();
        assert_eq!(
            shape,
            [
                ("s1".to_string(), Some("a".to_string())),
                ("s1".to_string(), Some("b".to_string())),
                ("s2".to_string(), Some("a".to_string())),
                ("s3".to_string(), None),
            ]
        );
        assert!(pairs.iter().all(|(i, t)| t.as_ref().is_none_or(|t| t.item_id == i.id)));

        // Aggregates over the whole selection, and an empty one.
        #[derive(Debug, Deserialize, PartialEq)]
        struct Whole {
            n: i64,
            total: Option<i64>,
        }
        let whole: Vec<Whole> = db
            .select::<Item>()
            .aggregate("n", Agg::Count)
            .aggregate("total", Agg::Sum(col::<Item>("qty")))
            .fetch_as()
            .await
            .unwrap();
        assert_eq!(whole, [Whole { n: 6, total: Some(21) }]);
        let none: Vec<Whole> = db
            .select::<Item>()
            .filter(Spec::gt("qty", "100"))
            .aggregate("n", Agg::Count)
            .aggregate("total", Agg::Sum(col::<Item>("qty")))
            .fetch_as()
            .await
            .unwrap();
        assert_eq!(none, [Whole { n: 0, total: None }]);

        // Mistakes are errors, not wrong SQL.
        assert!(db.select::<Item>().filter_on::<Tag>(Spec::eq("label", "a")).fetch_count().await.is_err(), "not joined");
        assert!(db.select::<Item>().group_by(col::<Item>("nope")).aggregate("n", Agg::Count).fetch_rows().await.is_err());
        assert!(db.select::<Item>().aggregate("n", Agg::Count).having("m", Cmp::Gt, 1).fetch_rows().await.is_err());
    });
}

/// `execute_many` behaves as if the rows ran one by one: on a failure at row
/// k, rows before it stay applied and rows after it are not run.
fn execute_many_failure_semantics<H: Harness>(h: H) {
    block(async move {
        let db = h.connect().await;
        fresh_table::<Member, H>(&h, &db).await;
        let d = h.dialect();
        let q = |s: &str| d.quote_identifier(s);
        let insert = format!(
            "INSERT INTO {} ({}, {}, {}) VALUES ({}, {}, {})",
            q("conf_members"),
            q("code"),
            q("email"),
            q("level"),
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3)
        );
        let row = |c: &str, e: &str, l: i64| -> Vec<Param> { vec![c.into(), e.into(), l.into()] };
        let count = |db: &H::Driver| {
            let sql = format!("SELECT COUNT(*) FROM {}", q("conf_members"));
            let db = db.deref().clone();
            async move { db.query_scalar(&sql, &[]).await.unwrap().as_i64().unwrap() }
        };

        // Many rows in one call (more than one pipelined flight on MySQL).
        let many: Vec<Vec<Param>> = (0..1200).map(|i| row(&format!("m{i}"), &format!("m{i}@x"), i)).collect();
        assert_eq!(db.execute_many(&insert, &many).await.unwrap(), 1200);
        assert_eq!(count(&db).await, 1200);
        fresh_table::<Member, H>(&h, &db).await;

        // Outside a transaction: row 3 repeats row 1's e-mail.
        let rows = vec![row("a", "a@x", 1), row("b", "b@x", 2), row("c", "a@x", 3), row("d", "d@x", 4)];
        let err = db.execute_many(&insert, &rows).await.unwrap_err();
        assert!(matches!(err, OrmError::UniqueViolation { .. }), "{err:?}");
        assert_eq!(count(&db).await, 2, "rows before the failure applied, none after");
        let codes = db.query(&format!("SELECT {} FROM {} ORDER BY {}", q("code"), q("conf_members"), q("code")), &[]).await.unwrap();
        assert_eq!(codes.rows.iter().map(|r| r[0].as_str().unwrap().to_string()).collect::<Vec<_>>(), ["a", "b"]);

        // Inside a transaction: the same, and the transaction goes on.
        let insert2 = insert.clone();
        let r: Result<i64, OrmError> = db
            .transaction(move |tx| {
                let insert = insert2.clone();
                Box::pin(async move {
                    let rows = vec![row("e", "e@x", 5), row("f", "a@x", 6), row("g", "g@x", 7)];
                    assert!(tx.execute_many(&insert, &rows).await.is_err());
                    tx.execute_many(&insert, &[row("h", "h@x", 8), row("i", "i@x", 9)]).await?;
                    let n = tx.query(&format!("SELECT COUNT(*) FROM {}", d.quote_identifier("conf_members")), &[]).await?;
                    Ok(n.rows[0][0].as_i64().unwrap())
                })
            })
            .await;
        assert_eq!(r.unwrap(), 5, "a, b + e (before the failure) + h, i");
        assert_eq!(count(&db).await, 5, "committed");
    });
}

/// Server-side cursors: batches fetched on demand, other statements on the
/// same transaction in between. A backend without them reports
/// `Unsupported` (listed as a gap in its acceptance report).
fn server_cursors<H: Harness>(h: H) {
    block(async move {
        let db = h.connect().await;
        fresh_table::<Item, H>(&h, &db).await;
        let repo = db.repository::<Item>();
        for i in 1..=25i64 {
            repo.save(item(&format!("c{i:02}"), i, 0.0, false, None)).await.unwrap();
        }
        let d = h.dialect();
        let q = |s: &str| d.quote_identifier(s);
        let select = format!("SELECT {}, {} FROM {} WHERE {} > {} ORDER BY {}", q("id"), q("qty"), q("conf_items"), q("qty"), d.placeholder(1), q("qty"));
        let mut cur = match db.cursor(&select, params![0]).await {
            Err(OrmError::Unsupported(what)) => {
                eprintln!("server_cursors: unsupported by this backend ({what})");
                return;
            }
            other => other.unwrap(),
        };
        assert_eq!(cur.columns().iter().map(|c| c.to_ascii_lowercase()).collect::<Vec<_>>(), ["id", "qty"]);
        let mut seen = Vec::new();
        let mut batches = 0;
        while let Some(b) = cur.next_batch(10).await.unwrap() {
            assert!(b.rows.len() <= 10);
            batches += 1;
            seen.extend(b.rows.iter().map(|r| r[1].as_i64().unwrap()));
        }
        assert_eq!(seen, (1..=25).collect::<Vec<_>>());
        assert!(batches >= 3);
        assert!(cur.next_batch(10).await.unwrap().is_none(), "stays exhausted");

        // Closing early returns the connection cleanly; it keeps working.
        let mut early = db.cursor(&select, params![0]).await.unwrap();
        assert_eq!(early.next_batch(5).await.unwrap().unwrap().rows.len(), 5);
        early.close().await.unwrap();
        assert_eq!(repo.count().await.unwrap(), 25);

        // In a transaction: update each row while walking the cursor.
        let (select2, upd) = (
            select.clone(),
            format!("UPDATE {} SET {} = {} WHERE {} = {}", q("conf_items"), q("price"), d.placeholder(1), q("id"), d.placeholder(2)),
        );
        db.transaction(move |tx| {
            let (select, upd) = (select2.clone(), upd.clone());
            Box::pin(async move {
                let mut cur = tx.open_cursor(&select, params![0]).await?;
                while let Some(b) = tx.fetch(&mut cur, 7).await? {
                    for r in &b.rows {
                        let id = r[0].as_i64().unwrap();
                        tx.execute_with(&upd, params![r[1].as_i64().unwrap() as f64 * 2.0, id]).await?;
                    }
                }
                Ok(())
            })
        })
        .await
        .unwrap();
        let sum = db.query_scalar(&format!("SELECT SUM({}) FROM {}", q("price"), q("conf_items")), &[]).await.unwrap();
        assert_eq!(sum.as_f64(), Some(2.0 * 325.0));

        // A cursor over a statement without rows is refused.
        let bad = db.cursor(&format!("UPDATE {} SET {} = 0", q("conf_items"), q("qty")), &[]).await;
        assert!(bad.is_err());
    });
}
