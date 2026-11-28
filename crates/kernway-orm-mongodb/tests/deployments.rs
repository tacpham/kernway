//! Replica sets and sharded clusters: discovery, read preferences and tags,
//! transactions, causal consistency, retryable writes and reads (fail points),
//! failover, change streams with resume, snapshot reads, mongos pinning.

mod common;
use common::{block, env, unique};
use kernway_orm_mongodb::topology::{ServerType, TopologyType};
use kernway_orm_mongodb::*;
use std::time::{Duration, Instant};

async fn wait_for<F: Fn(&Client) -> bool>(c: &Client, what: &str, f: F) {
    let started = Instant::now();
    while !f(c) {
        assert!(started.elapsed() < Duration::from_secs(30), "timed out waiting for {what}: {}", c.topology().summary());
        rt_core::sleep(Duration::from_millis(50)).await;
    }
}

/// The member a command ran on (`hello.me` of the reply's server).
async fn served_by(db: &Database, rp: &ReadPreference) -> String {
    db.run_command_with(doc! { "hello" => 1 }, rp).await.unwrap().get_str("me").unwrap_or("").to_string()
}

async fn fail_point(c: &Client, spec: Document) {
    c.run_command("admin", doc! { "configureFailPoint" => "failCommand", "mode" => doc! { "times" => 1 }, "data" => spec }).await.unwrap();
}

#[test]
fn replica_set_discovery_and_read_preferences() {
    let Some(url) = env("KERNWAY_MONGO_RS_URL") else { return };
    block(async move {
        // Two seeds: the third member is found from the primary's host list.
        let c = Client::connect(&url).await.unwrap();
        c.ping().await.unwrap();
        wait_for(&c, "three members", |c| c.topology().servers.values().filter(|s| s.kind != ServerType::Unknown).count() == 3).await;
        let t = c.topology();
        assert_eq!(t.kind, TopologyType::ReplicaSetWithPrimary);
        assert_eq!(t.set_name.as_deref(), Some("rs0"));
        let primary = t.servers.values().find(|s| s.kind == ServerType::RsPrimary).unwrap().address.to_string();
        let db = c.database("admin");
        assert_eq!(served_by(&db, &ReadPreference::primary()).await, primary);
        for _ in 0..5 {
            assert_ne!(served_by(&db, &ReadPreference::new(ReadMode::Secondary)).await, primary);
        }
        // Tags pick the member.
        let mut rp = ReadPreference::new(ReadMode::Secondary);
        rp.tag_sets = vec![vec![("dc".into(), "dc2".into())]];
        assert_eq!(served_by(&db, &rp).await, "127.0.0.1:27113");
        rp.tag_sets = vec![vec![("dc".into(), "nowhere".into())], vec![]];
        assert_ne!(served_by(&db, &rp).await, primary, "the empty tag set matches any secondary");
        // From the URL.
        let c2 = Client::connect(&format!("{url}&readPreference=secondary&readPreferenceTags=dc:dc1")).await.unwrap();
        let coll = c2.database("kernway_test").collection::<Document>(&unique("rp"));
        assert!(coll.find(doc! {}, FindOptions::default()).await.unwrap().collect().await.unwrap().is_empty());
        // directConnection to a secondary works for reads only.
        let direct = Client::connect("mongodb://127.0.0.1:27113/?directConnection=true").await.unwrap();
        direct.ping().await.unwrap();
        assert_eq!(direct.topology().kind, TopologyType::Single);
        let err = direct.database("kernway_test").collection::<Document>("x").insert_one(&doc! { "a" => 1 }).await.unwrap_err();
        assert!(err.to_string().contains("NotWritablePrimary") || err.to_string().contains("not primary"), "{err}");
        // A wrong set name finds no server.
        let wrong = Client::connect("mongodb://127.0.0.1:27111/?replicaSet=other&serverSelectionTimeoutMS=1500").await.unwrap();
        assert_eq!(wrong.ping().await.unwrap_err().kind, ErrorKind::ServerSelection);
    });
}

#[test]
fn transactions_and_causal_consistency() {
    let Some(url) = env("KERNWAY_MONGO_RS_URL") else { return };
    block(async move {
        let c = Client::connect(&format!("{url}&w=majority")).await.unwrap();
        let name = unique("txn");
        let coll = c.database("kernway_test").collection::<Document>(&name);
        c.database("kernway_test").create_collection(&name, Document::new()).await.unwrap();

        // Commit: both writes appear together.
        let s = c.start_session();
        s.start_transaction(TransactionOptions::default()).unwrap();
        let tc = coll.in_session(&s);
        tc.insert_one(&doc! { "_id" => 1, "v" => "a" }).await.unwrap();
        tc.insert_one(&doc! { "_id" => 2, "v" => "b" }).await.unwrap();
        assert_eq!(coll.count_documents(doc! {}).await.unwrap(), 0, "not visible outside before commit");
        assert_eq!(tc.count_documents(doc! {}).await.unwrap(), 2, "visible inside");
        s.commit_transaction().await.unwrap();
        assert_eq!(coll.count_documents(doc! {}).await.unwrap(), 2);
        // Commit again is allowed (idempotent retry); abort after commit is not.
        s.commit_transaction().await.unwrap();
        assert!(s.abort_transaction().await.is_err());

        // Abort: nothing remains.
        s.start_transaction(TransactionOptions::default()).unwrap();
        tc.insert_one(&doc! { "_id" => 3 }).await.unwrap();
        tc.delete_one(doc! { "_id" => 1 }).await.unwrap();
        s.abort_transaction().await.unwrap();
        assert_eq!(coll.count_documents(doc! {}).await.unwrap(), 2);
        assert!(s.start_transaction(TransactionOptions::default()).is_ok());
        assert!(s.start_transaction(TransactionOptions::default()).is_err(), "already in progress");
        s.abort_transaction().await.unwrap();

        // with_transaction retries a write conflict with another transaction.
        let other = c.start_session();
        other.start_transaction(TransactionOptions::default()).unwrap();
        coll.in_session(&other).update_one(doc! { "_id" => 2 }, doc! { "$set" => doc! { "by" => "other" } }, Default::default()).await.unwrap();
        let attempts = std::cell::Cell::new(0);
        let committer = rt_core::spawn({
            let other = other.clone();
            async move {
                rt_core::sleep(Duration::from_millis(300)).await;
                other.commit_transaction().await.unwrap();
            }
        });
        let _ = committer;
        let s2 = c.start_session();
        let r = s2
            .with_transaction(TransactionOptions::default(), async |s| {
                attempts.set(attempts.get() + 1);
                coll.in_session(s).update_one(doc! { "_id" => 2 }, doc! { "$inc" => doc! { "n" => 1 } }, Default::default()).await?;
                Ok(())
            })
            .await;
        r.unwrap();
        assert!(attempts.get() >= 2, "the conflicting attempt was retried ({} attempts)", attempts.get());
        let d = coll.find_one(doc! { "_id" => 2 }, FindOptions::default()).await.unwrap().unwrap();
        assert_eq!((d.get_str("by"), d.get_i64("n")), (Some("other"), Some(1)), "no lost update");

        // Causal consistency: a secondary read after a write sees it.
        let cs = c.start_session();
        let cc = coll.in_session(&cs);
        for i in 0..20 {
            cc.update_one(doc! { "_id" => 1 }, doc! { "$set" => doc! { "i" => i } }, Default::default()).await.unwrap();
            let sec = cc.with_read_preference(ReadPreference::new(ReadMode::Secondary));
            let got = sec.find_one(doc! { "_id" => 1 }, FindOptions::default()).await.unwrap().unwrap();
            assert_eq!(got.get_i64("i"), Some(i), "read your writes on a secondary");
        }
        assert!(cs.operation_time().is_some());

        // Snapshot reads see one point in time.
        let snap = c.start_snapshot_session();
        let sc = coll.in_session(&snap);
        let before = sc.count_documents(doc! {}).await.unwrap();
        coll.insert_one(&doc! { "_id" => 99 }).await.unwrap();
        assert_eq!(sc.count_documents(doc! {}).await.unwrap(), before, "the snapshot does not move");
        assert!(snap.start_transaction(TransactionOptions::default()).is_err());
        coll.drop().await.unwrap();
    });
}

#[test]
fn retryable_writes_and_reads_survive_one_failure() {
    let Some(url) = env("KERNWAY_MONGO_RS_URL") else { return };
    block(async move {
        let c = Client::connect(&url).await.unwrap();
        let coll = c.database("kernway_test").collection::<Document>(&unique("retry"));
        coll.insert_one(&doc! { "_id" => 0 }).await.unwrap();
        // The connection drops after the insert ran: the retry must not insert twice.
        fail_point(&c, doc! { "failCommands" => vec![Bson::from("insert")], "closeConnection" => true }).await;
        coll.insert_one(&doc! { "_id" => 1, "x" => 1 }).await.unwrap();
        // A retryable error code before the write.
        fail_point(&c, doc! { "failCommands" => vec![Bson::from("update")], "errorCode" => 91, "errorLabels" => vec![Bson::from("RetryableWriteError")] }).await;
        coll.update_one(doc! { "_id" => 1 }, doc! { "$inc" => doc! { "x" => 1 } }, Default::default()).await.unwrap();
        let d = coll.find_one(doc! { "_id" => 1 }, FindOptions::default()).await.unwrap().unwrap();
        assert_eq!(d.get_i64("x"), Some(2), "applied exactly once");
        // Reads retry too.
        fail_point(&c, doc! { "failCommands" => vec![Bson::from("find")], "closeConnection" => true }).await;
        assert_eq!(coll.find(doc! {}, FindOptions::default()).await.unwrap().collect().await.unwrap().len(), 2);
        // Without retries the error surfaces.
        let no = Client::connect(&format!("{url}&retryWrites=false")).await.unwrap();
        fail_point(&no, doc! { "failCommands" => vec![Bson::from("insert")], "closeConnection" => true }).await;
        let err = no.database("kernway_test").collection::<Document>(coll.name()).insert_one(&doc! { "_id" => 5 }).await.unwrap_err();
        assert!(err.is_network(), "{err}");
        // A non-retryable error is not retried.
        fail_point(&c, doc! { "failCommands" => vec![Bson::from("insert")], "errorCode" => 2 }).await;
        assert_eq!(coll.insert_one(&doc! { "_id" => 6 }).await.unwrap_err().code(), Some(2));
        coll.drop().await.unwrap();
    });
}

#[test]
fn failover_to_a_new_primary() {
    let Some(url) = env("KERNWAY_MONGO_RS_URL") else { return };
    block(async move {
        let c = Client::connect(&format!("{url}&heartbeatFrequencyMS=500")).await.unwrap();
        let coll = c.database("kernway_test").collection::<Document>(&unique("failover"));
        coll.insert_one(&doc! { "_id" => 0 }).await.unwrap();
        let old = c.topology().servers.values().find(|s| s.kind == ServerType::RsPrimary).unwrap().address.clone();
        // Step the primary down; writes continue on the new one.
        // The server refuses while no secondary has caught up: retry until
        // it steps down (its connections close: a network error).
        let started = Instant::now();
        loop {
            match c.run_command("admin", doc! { "replSetStepDown" => 30, "secondaryCatchUpPeriodSecs" => 10 }).await {
                Ok(_) => break,
                Err(e) if e.is_network() || e.is_state_change() => break,
                Err(e) => {
                    assert!(started.elapsed() < Duration::from_secs(60), "the primary would not step down: {e}");
                    rt_core::sleep(Duration::from_millis(500)).await;
                }
            }
        }
        let mut written = 0;
        let started = Instant::now();
        while written < 20 && started.elapsed() < Duration::from_secs(60) {
            match coll.insert_one(&doc! { "_id" => written + 1 }).await {
                Ok(_) => written += 1,
                Err(e) => eprintln!("during the election: {e}"),
            }
        }
        assert_eq!(written, 20);
        wait_for(&c, "a new primary", |c| c.topology().servers.values().any(|s| s.kind == ServerType::RsPrimary && s.address != old)).await;
        assert_eq!(coll.count_documents(doc! {}).await.unwrap(), 21);
        // Give the preferred member its role back (priority 2) for later tests.
        let started = Instant::now();
        while c.topology().servers.values().find(|s| s.kind == ServerType::RsPrimary).map(|s| s.address.clone()) != Some(old.clone()) {
            if started.elapsed() > Duration::from_secs(90) {
                break;
            }
            rt_core::sleep(Duration::from_millis(500)).await;
        }
        coll.drop().await.unwrap();
    });
}

#[test]
fn change_streams_resume_after_errors() {
    let Some(url) = env("KERNWAY_MONGO_RS_URL") else { return };
    block(async move {
        let c = Client::connect(&url).await.unwrap();
        let name = unique("cs");
        let coll = c.database("kernway_test").collection::<Document>(&name);
        c.database("kernway_test").create_collection(&name, Document::new()).await.unwrap();
        let mut cs = coll
            .watch(vec![doc! { "$match" => doc! { "operationType" => doc! { "$in" => vec![Bson::from("insert"), Bson::from("update")] } } }],
                ChangeStreamOptions { full_document: Some("updateLookup".into()), max_await: Some(Duration::from_millis(300)), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(cs.try_next(Duration::from_millis(500)).await.unwrap(), None, "nothing yet");
        coll.insert_one(&doc! { "_id" => 1, "v" => "a" }).await.unwrap();
        let ev = cs.next().await.unwrap().unwrap();
        assert_eq!(ev.get_str("operationType"), Some("insert"));
        assert_eq!(ev.get_document("fullDocument").and_then(|d| d.get_str("v")), Some("a"));
        // The getMore's connection drops: the stream resumes, losing nothing.
        fail_point(&c, doc! { "failCommands" => vec![Bson::from("getMore")], "closeConnection" => true }).await;
        coll.update_one(doc! { "_id" => 1 }, doc! { "$set" => doc! { "v" => "b" } }, Default::default()).await.unwrap();
        let ev = cs.next().await.unwrap().unwrap();
        assert_eq!(ev.get_str("operationType"), Some("update"));
        assert_eq!(ev.get_document("fullDocument").and_then(|d| d.get_str("v")), Some("b"));
        // Resume from a saved token in a new stream.
        let token = cs.resume_token().cloned().unwrap();
        coll.insert_one(&doc! { "_id" => 2 }).await.unwrap();
        let mut again = coll.watch(vec![], ChangeStreamOptions { resume_after: Some(token), ..Default::default() }).await.unwrap();
        let ev = again.next().await.unwrap().unwrap();
        assert_eq!(ev.get_document("documentKey").and_then(|d| d.get_i64("_id")), Some(2));
        // A database-wide stream.
        let mut dbs = c.database("kernway_test").watch(vec![], ChangeStreamOptions::default()).await.unwrap();
        coll.insert_one(&doc! { "_id" => 3 }).await.unwrap();
        let ev = dbs.next().await.unwrap().unwrap();
        assert_eq!(ev.get_document("ns").and_then(|d| d.get_str("coll")), Some(name.as_str()));
        // Dropping the collection invalidates its stream.
        coll.drop().await.unwrap();
        let mut seen_invalidate = false;
        let mut stream = coll.watch(vec![], ChangeStreamOptions { resume_after: again.resume_token().cloned(), ..Default::default() }).await.unwrap();
        while let Some(ev) = stream.next().await {
            if ev.unwrap().get_str("operationType") == Some("invalidate") {
                seen_invalidate = true;
            }
        }
        assert!(seen_invalidate);
    });
}

#[test]
fn sharded_cluster_through_mongos() {
    let Some(url) = env("KERNWAY_MONGO_SHARDED_URL") else { return };
    block(async move {
        let c = Client::connect(&url).await.unwrap();
        c.ping().await.unwrap();
        wait_for(&c, "two routers", |c| c.topology().servers.values().filter(|s| s.kind == ServerType::Mongos).count() == 2).await;
        assert_eq!(c.topology().kind, TopologyType::Sharded);
        let name = unique("sharded");
        let db = c.database("kernway_test");
        db.create_collection(&name, Document::new()).await.unwrap();
        c.run_command("admin", doc! { "shardCollection" => format!("kernway_test.{name}"), "key" => doc! { "_id" => "hashed" } }).await.unwrap();
        let coll = db.collection::<Document>(&name);
        let docs: Vec<Document> = (0..500).map(|i| doc! { "_id" => i, "g" => i % 5 }).collect();
        coll.insert_many_documents(docs, false).await.unwrap();
        assert_eq!(coll.count_documents(doc! {}).await.unwrap(), 500);
        // Reads with a read preference go through mongos ($readPreference).
        let sec = coll.with_read_preference(ReadPreference::new(ReadMode::SecondaryPreferred));
        assert_eq!(sec.count_documents(doc! { "g" => 1 }).await.unwrap(), 100);
        // A transaction pins one mongos for all its operations.
        let s = c.start_session();
        s.start_transaction(TransactionOptions::default()).unwrap();
        let tc = coll.in_session(&s);
        for i in 0..10 {
            tc.update_one(doc! { "_id" => i }, doc! { "$set" => doc! { "t" => true } }, Default::default()).await.unwrap();
        }
        s.commit_transaction().await.unwrap();
        assert_eq!(coll.count_documents(doc! { "t" => true }).await.unwrap(), 10);
        // Retryable writes through mongos. The fail point lives on one
        // router, so arm it and write through a client of that router only —
        // otherwise the write may go to the other one and leave it armed.
        let routers: Vec<String> = c.topology().servers.keys().map(|a| a.to_string()).collect();
        let one = Client::connect(&format!("mongodb://{}/kernway_test", routers[0])).await.unwrap();
        fail_point(&one, doc! { "failCommands" => vec![Bson::from("insert")], "closeConnection" => true }).await;
        one.database("kernway_test").collection::<Document>(&name).insert_one(&doc! { "_id" => 1000 }).await.unwrap();
        assert_eq!(coll.count_documents(doc! {}).await.unwrap(), 501);
        // Leave no fail point armed for the tests that follow.
        for r in &routers {
            let d = Client::connect(&format!("mongodb://{r}/?directConnection=true")).await.unwrap();
            d.run_command("admin", doc! { "configureFailPoint" => "failCommand", "mode" => "off" }).await.unwrap();
        }
        coll.drop().await.unwrap();
    });
}
