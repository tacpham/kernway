//! The ORM backend: the behaviour the SQL conformance suite asks of every
//! relational backend, for documents — CRUD with generated keys, value
//! round trips, batches, the query builder and specs, binary and long text,
//! unique violations, transactions, and no lost update under contention.

mod common;
use common::{block, env, unique};
use kernway_orm_core::{
    error::OrmError,
    spec::Spec,
    transaction::{Transaction, TransactionalDriver},
    Driver,
};
use kernway_orm_macro::entity;
use kernway_orm_mongodb::{doc, MongoDriver};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "orm_items")]
struct Item {
    #[id(strategy = "auto")]
    id: i64,
    name: String,
    qty: i64,
    price: f64,
    active: bool,
    note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "orm_files")]
struct File {
    #[id(strategy = "auto")]
    id: String,
    body: String,
    data: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "orm_members")]
struct Member {
    #[id(strategy = "manual")]
    code: String,
    #[column(unique)]
    email: String,
    level: i32,
}

fn item(name: &str, qty: i64, price: f64, active: bool, note: Option<&str>) -> Item {
    Item { id: 0, name: name.into(), qty, price, active, note: note.map(String::from) }
}

/// A driver on a fresh database (dropped at the end).
async fn driver(url: &str) -> MongoDriver {
    let base = url.split('?').next().unwrap().rsplit_once('/').unwrap().0.to_string();
    let query = url.split_once('?').map(|(_, q)| format!("?{q}")).unwrap_or_default();
    MongoDriver::connect(&format!("{base}/{}{query}", unique("orm"))).await.unwrap()
}

#[test]
fn crud_values_batches_and_queries() {
    let Some(url) = env("KERNWAY_MONGO_URL") else { return };
    block(async move {
        let db = driver(&url).await;
        let repo = db.repository::<Item>();
        // CRUD with generated keys.
        let a = repo.save(item("alpha", 3, 9.5, true, Some("first"))).await.unwrap();
        assert!(a.id > 0);
        let b = repo.save(item("beta", 5, 1.25, false, None)).await.unwrap();
        assert_eq!(b.id, a.id + 1, "keys from the counter");
        assert_eq!(repo.find_by_id(&a.id).await.unwrap(), Some(a.clone()));
        let updated = repo.save(Item { qty: 30, note: None, ..a.clone() }).await.unwrap();
        assert_eq!(repo.find_by_id(&a.id).await.unwrap(), Some(updated));
        assert_eq!(repo.count().await.unwrap(), 2);
        repo.delete_by_id(&b.id).await.unwrap();
        assert!(!repo.exists_by_id(&b.id).await.unwrap());
        assert!(matches!(repo.save(Item { id: 999, ..b.clone() }).await, Err(OrmError::NotFound)), "a generated key must exist");
        // Values.
        for c in [
            item("i64 max", i64::MAX, 0.0, true, None),
            item("i64 min", i64::MIN, -0.0, false, None),
            item("tiny", 0, 1e-300, true, Some("x")),
            item("big float", 1, 1.797_693_134_862_315_7e308, true, None),
            item("Việt Nam ✓ 😀", 7, 3.5, true, Some("ghi chú tiếng Việt 😀")),
            item("quote ' and \\ backslash $ dollar", 1, 0.5, false, Some("a'b\"c.d")),
            item("", 2, 0.0, true, Some("")),
        ] {
            let saved = repo.save(c.clone()).await.unwrap();
            assert_eq!(repo.find_by_id(&saved.id).await.unwrap().unwrap(), Item { id: saved.id, ..c }, "value round trip");
        }
        // Batches: one counter step, input order kept.
        let rows: Vec<Item> = (0..50).map(|i| item(&format!("n{i:02}"), i, f64::from(i as i32) / 2.0, i % 2 == 0, None)).collect();
        let saved = repo.save_all(rows.clone()).await.unwrap();
        let mut ids: Vec<i64> = saved.iter().map(|s| s.id).collect();
        ids.dedup();
        assert_eq!(ids.len(), 50);
        for (s, r) in saved.iter().zip(&rows) {
            assert_eq!(Item { id: s.id, ..r.clone() }, *s);
        }
        let some: Vec<i64> = saved.iter().step_by(10).map(|s| s.id).collect();
        assert_eq!(repo.find_all_by_ids(&some).await.unwrap().len(), 5);
        repo.delete_all_by_ids(&some).await.unwrap();
        assert_eq!(repo.find_all_by_ids(&some).await.unwrap().len(), 0);
        // The query builder against an oracle: the same specs evaluated in
        // memory over every row (numeric comparisons numeric, NULL never
        // equal / unequal).
        let all = repo.find_all().await.unwrap();
        let get = |i: &Item, f: &str| -> Option<String> {
            match f {
                "name" => Some(i.name.clone()),
                "qty" => Some(i.qty.to_string()),
                "active" => Some(i.active.to_string()),
                // The in-memory oracle counts '' as NULL; SQL (and this
                // backend) do not.
                "note" => i.note.clone().map(|n| if n.is_empty() { "\u{1}".into() } else { n }),
                _ => None,
            }
        };
        let specs = [
            Spec::gt("qty", "9").and(Spec::lt("qty", "20")),
            Spec::eq("active", "true").and(Spec::between("qty", "0", "9")),
            Spec::in_("name", vec!["n01".to_string(), "n03".into(), "zz".into()]),
            Spec::eq("active", "false").and(Spec::gte("qty", "40")).or(Spec::eq("name", "alpha")),
            Spec::ne("note", "x"),
            Spec::lte("qty", "-1").or(Spec::gte("qty", "1000")),
            Spec::is_not_null("note").and(Spec::ne("name", "alpha")),
        ];
        for spec in specs {
            let want = all.iter().filter(|i| spec.matches(&|f: &str| get(i, f))).count() as u64;
            assert!(want > 0, "{spec:?} selects something");
            assert_eq!(repo.query().filter_spec(spec.clone()).fetch_count().await.unwrap(), want, "{spec:?}");
        }
        // LIKE patterns, NULL tests, NOT (counted directly).
        let count = |p: &dyn Fn(&Item) -> bool| all.iter().filter(|i| p(i)).count() as u64;
        assert_eq!(repo.query().filter_like("name", "n0%").fetch_count().await.unwrap(), count(&|i| i.name.starts_with("n0")));
        assert_eq!(repo.query().filter_like("name", "n_5").fetch_count().await.unwrap(), count(&|i| i.name.len() == 3 && i.name.starts_with('n') && i.name.ends_with('5')));
        assert_eq!(repo.query().filter_like("name", "%$%").fetch_count().await.unwrap(), 1, "regex characters are literal");
        assert_eq!(repo.query().filter_is_null("note").fetch_count().await.unwrap(), count(&|i| i.note.is_none()));
        assert_eq!(repo.query().filter_is_not_null("note").fetch_count().await.unwrap(), count(&|i| i.note.is_some()));
        assert_eq!(repo.query().filter_spec(Spec::like("name", "n%").not()).fetch_count().await.unwrap(), count(&|i| !i.name.starts_with('n')));
        assert_eq!(repo.query().filter_eq("qty", "not a number").fetch_count().await.unwrap(), 0);
        // Ordering and pages.
        let page = repo.query().filter_like("name", "n%").order_by_desc("qty").fetch_page(1, 10).await.unwrap();
        assert_eq!(page.total, count(&|i| i.name.starts_with('n')));
        assert_eq!(page.items.len(), 10);
        assert!(page.items.windows(2).all(|w| w[0].qty > w[1].qty));
        assert_eq!(repo.query().order_by_asc("qty").limit(3).offset(2).fetch_all().await.unwrap().len(), 3);
        assert_eq!(repo.query().order_by_asc("qty").fetch_one().await.unwrap().unwrap().name, "i64 min");
        db.database().drop().await.unwrap();
    });
}

#[test]
fn binary_text_ids_and_unique_columns() {
    let Some(url) = env("KERNWAY_MONGO_URL") else { return };
    block(async move {
        let db = driver(&url).await;
        let files = db.repository::<File>();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 256) as u8).collect();
        let f = files.save(File { id: String::new(), body: "ü".repeat(2_000_000), data: data.clone() }).await.unwrap();
        assert_eq!(f.id.len(), 24, "a text key is a fresh ObjectId");
        let got = files.find_by_id(&f.id).await.unwrap().unwrap();
        assert_eq!(got.data, data);
        assert_eq!(got.body.chars().count(), 2_000_000);
        let raw = db.database().collection::<kernway_orm_mongodb::Document>("orm_files").find_one(doc! {}, Default::default()).await.unwrap().unwrap();
        assert!(matches!(raw.get("_id"), Some(kernway_orm_mongodb::Bson::ObjectId(_))), "stored as an ObjectId");
        assert!(matches!(raw.get("data"), Some(kernway_orm_mongodb::Bson::Binary(_))), "bytes as binary");

        let members = db.repository::<Member>();
        members.save(Member { code: "a".into(), email: "a@x".into(), level: 1 }).await.unwrap();
        // A manual key that exists is updated; a new one inserted.
        members.save(Member { code: "a".into(), email: "a@x".into(), level: 2 }).await.unwrap();
        assert_eq!(members.find_by_id(&"a".into()).await.unwrap().unwrap().level, 2);
        let err = members.save(Member { code: "b".into(), email: "a@x".into(), level: 3 }).await.unwrap_err();
        assert!(matches!(&err, OrmError::UniqueViolation { field } if field == "email"), "{err:?}");
        assert_eq!(members.count().await.unwrap(), 1);
        db.database().drop().await.unwrap();
    });
}

#[test]
fn transactions_and_no_lost_update() {
    let Some(url) = env("KERNWAY_MONGO_RS_URL") else { return };
    block(async move {
        let db = driver(&url).await;
        let repo = db.repository::<Item>();
        let w = repo.save(item("wallet", 100, 0.0, true, None)).await.unwrap();
        let id = w.id;
        // Commit on Ok.
        db.transaction(move |tx| {
            Box::pin(async move {
                let r = tx.repository::<Item>();
                let mut x = r.find_by_id(&id).await?.unwrap();
                x.qty -= 30;
                r.save(x).await?;
                r.save(item("audit", 30, 0.0, true, None)).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
        assert_eq!(repo.find_by_id(&id).await.unwrap().unwrap().qty, 70);
        // Roll back on Err.
        let r: Result<(), OrmError> = db
            .transaction(move |tx| {
                Box::pin(async move {
                    let r = tx.repository::<Item>();
                    let mut x = r.find_by_id(&id).await?.unwrap();
                    x.qty = 0;
                    r.save(x).await?;
                    r.delete_by_id(&id).await?;
                    Err(OrmError::Query("changed my mind".into()))
                })
            })
            .await;
        assert!(r.is_err());
        assert_eq!(repo.find_by_id(&id).await.unwrap().unwrap().qty, 70);
        assert_eq!(repo.count().await.unwrap(), 2);
        // Two transactions take the same document for update: the second
        // conflicts instead of silently overwriting.
        let tx1 = db.client().start_session();
        let _ = tx1;
        let first = db.transaction(move |tx| {
            Box::pin(async move {
                let r = tx.repository::<Item>();
                let mut x = r.find_by_id_for_update(&id).await?.unwrap();
                rt_core::sleep(std::time::Duration::from_millis(400)).await;
                x.qty -= 1;
                r.save(x).await?;
                Ok(())
            })
        });
        let second = async {
            rt_core::sleep(std::time::Duration::from_millis(100)).await;
            db.transaction(move |tx| {
                Box::pin(async move {
                    let r = tx.repository::<Item>();
                    let mut x = r.find_by_id_for_update(&id).await?.unwrap();
                    x.qty -= 1;
                    r.save(x).await?;
                    Ok(())
                })
            })
            .await
        };
        let (a, b) = futures_join(first, second).await;
        a.unwrap();
        assert!(matches!(b, Err(OrmError::Transaction(_))), "{b:?}");
        assert_eq!(repo.find_by_id(&id).await.unwrap().unwrap().qty, 69);
        // With retries, many concurrent decrements all land.
        let tasks = (0..8).map(|_| {
            let db = &db;
            async move {
                db.transaction_retrying(async |tx| {
                    let r = tx.repository::<Item>();
                    let mut x = r.find_by_id_for_update(&id).await?.unwrap();
                    x.qty -= 1;
                    r.save(x).await?;
                    Ok(())
                })
                .await
            }
        });
        for r in join_all(tasks.collect()).await {
            r.unwrap();
        }
        assert_eq!(repo.find_by_id(&id).await.unwrap().unwrap().qty, 61, "no lost update");
        db.database().drop().await.unwrap();
    });
}

async fn futures_join<A: std::future::Future, B: std::future::Future>(a: A, b: B) -> (A::Output, B::Output) {
    let mut a = std::pin::pin!(a);
    let mut b = std::pin::pin!(b);
    let (mut ra, mut rb) = (None, None);
    std::future::poll_fn(|cx| {
        if ra.is_none() {
            if let std::task::Poll::Ready(v) = a.as_mut().poll(cx) {
                ra = Some(v);
            }
        }
        if rb.is_none() {
            if let std::task::Poll::Ready(v) = b.as_mut().poll(cx) {
                rb = Some(v);
            }
        }
        if ra.is_some() && rb.is_some() { std::task::Poll::Ready(()) } else { std::task::Poll::Pending }
    })
    .await;
    (ra.unwrap(), rb.unwrap())
}

async fn join_all<F: std::future::Future>(futs: Vec<F>) -> Vec<F::Output> {
    let mut futs: Vec<std::pin::Pin<Box<F>>> = futs.into_iter().map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = futs.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        for (f, o) in futs.iter_mut().zip(out.iter_mut()) {
            if o.is_none() {
                if let std::task::Poll::Ready(v) = f.as_mut().poll(cx) {
                    *o = Some(v);
                }
            }
        }
        if out.iter().all(Option::is_some) { std::task::Poll::Ready(()) } else { std::task::Poll::Pending }
    })
    .await;
    out.into_iter().map(Option::unwrap).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Detail {
    zeta: i64,
    alpha: String,
    middle: Vec<Line>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct Line {
    sku: String,
    cents: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[entity(table = "orm_tickets")]
struct Ticket {
    #[id(strategy = "auto")]
    id: i64,
    zone: String,
    detail: Detail,
    amount: i64,
}

#[test]
fn documents_keep_field_order() {
    let Some(url) = env("KERNWAY_MONGO_URL") else { return };
    block(async move {
        let db = driver(&url).await;
        let repo = db.repository::<Ticket>();
        let t = Ticket {
            id: 0,
            zone: "A".into(),
            detail: Detail { zeta: 1, alpha: "x".into(), middle: vec![Line { sku: "gel".into(), cents: 3500 }] },
            amount: 42,
        };
        let saved = repo.save(t.clone()).await.unwrap();
        // The stored document, read raw: _id first, then the declaration
        // order — nested objects too (sorted keys would be alpha, middle, zeta).
        let raw = db.database().collection::<kernway_orm_mongodb::Document>("orm_tickets").find_one(doc! {}, Default::default()).await.unwrap().unwrap();
        let keys = |d: &kernway_orm_mongodb::Document| d.iter().map(|(k, _)| k.to_string()).collect::<Vec<_>>();
        assert_eq!(keys(&raw), ["_id", "zone", "detail", "amount"]);
        let Some(kernway_orm_mongodb::Bson::Document(detail)) = raw.get("detail") else { panic!("detail: {raw:?}") };
        assert_eq!(keys(detail), ["zeta", "alpha", "middle"]);
        let Some(kernway_orm_mongodb::Bson::Array(lines)) = detail.get("middle") else { panic!() };
        let Some(kernway_orm_mongodb::Bson::Document(line)) = lines.first() else { panic!() };
        assert_eq!(keys(line), ["sku", "cents"]);
        // And it reads back equal.
        assert_eq!(repo.find_by_id(&saved.id).await.unwrap(), Some(Ticket { id: saved.id, ..t }));
        // A typed collection insert keeps struct order too.
        let coll = db.database().collection::<Detail>("orm_details");
        coll.insert_one(&Detail { zeta: 9, alpha: "y".into(), middle: vec![] }).await.unwrap();
        let raw = db.database().collection::<kernway_orm_mongodb::Document>("orm_details").find_one(doc! {}, Default::default()).await.unwrap().unwrap();
        assert_eq!(keys(&raw)[1..], ["zeta", "alpha", "middle"], "after the generated _id");
        db.database().drop().await.unwrap();
    });
}
