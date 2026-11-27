//! The collection API against a live server: every BSON type round trip,
//! typed collections, finds with every option, cursors over many batches
//! (and killed early), batch splitting, size limits, updates, finds-and-
//! modifies, bulk writes, aggregation, indexes, GridFS.

mod common;
use common::{block, env, unique};
use kernway_orm_mongodb::*;

async fn fresh(url: &str, name: &str) -> (Client, Collection<Document>) {
    let c = Client::connect(url).await.unwrap();
    let coll = c.database("kernway_test").collection::<Document>(&unique(name));
    (c, coll)
}

#[test]
fn every_bson_type_round_trips() {
    let Some(url) = env("KERNWAY_MONGO_URL") else { return };
    block(async move {
        let (_c, coll) = fresh(&url, "types").await;
        let d = doc! {
            "_id" => ObjectId::new(),
            "double" => 1.5, "nan" => f64::NAN, "inf" => f64::NEG_INFINITY, "negzero" => -0.0,
            "string" => "Việt Nam 😀 \u{0}inside", "doc" => doc! { "a" => doc! { "b" => 1 } },
            "array" => vec![Bson::Int32(1), Bson::from("two"), Bson::Null, Bson::Array(vec![])],
            "binary" => Binary { subtype: 0, bytes: vec![0, 255, 1] }, "uuid" => Binary { subtype: 4, bytes: vec![7; 16] },
            "oid" => ObjectId::new(), "bool" => true, "date" => DateTime(1_700_000_000_123), "before1970" => DateTime(-86_400_000),
            "null" => Bson::Null, "regex" => Regex::new("^a.*z$", "mi"), "code" => Bson::JavaScriptCode("function() { return 1; }".into()),
            "int32" => i32::MIN, "ts" => Timestamp { time: 1_700_000_000, increment: 7 }, "int64" => i64::MAX,
            "decimal" => Decimal128::parse("-1234567890.0987654321").unwrap(), "min" => Bson::MinKey, "max" => Bson::MaxKey,
        };
        coll.insert_one(&d).await.unwrap();
        let got = coll.find_one(doc! { "_id" => d.get("_id").cloned().unwrap() }, FindOptions::default()).await.unwrap().unwrap();
        for (k, v) in d.iter() {
            let g = got.get(k).unwrap_or_else(|| panic!("{k} missing"));
            match (v, g) {
                (Bson::Double(a), Bson::Double(b)) if a.is_nan() => assert!(b.is_nan()),
                (Bson::Double(a), Bson::Double(b)) => assert_eq!(a.to_bits(), b.to_bits(), "{k}"),
                _ => assert_eq!(v, g, "{k}"),
            }
        }
        // Typed collections through serde.
        #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Clone)]
        struct Item {
            _id: ObjectId,
            name: String,
            qty: i64,
            tags: Vec<String>,
            at: DateTime,
            price: Decimal128,
            note: Option<String>,
        }
        let items: Collection<Item> = coll.clone_with_type();
        let it = Item { _id: ObjectId::new(), name: "a".into(), qty: 3, tags: vec!["x".into()], at: DateTime::now(), price: Decimal128::parse("9.99").unwrap(), note: None };
        items.insert_one(&it).await.unwrap();
        assert_eq!(items.find_one(doc! { "_id" => it._id }, FindOptions::default()).await.unwrap(), Some(it));
        coll.drop().await.unwrap();
    });
}

#[test]
fn finds_cursors_and_batches() {
    let Some(url) = env("KERNWAY_MONGO_URL") else { return };
    block(async move {
        let (c, coll) = fresh(&url, "find").await;
        // 120 000 documents: more than one maxWriteBatchSize (100 000) batch.
        let docs: Vec<Document> = (0..120_000).map(|i| doc! { "_id" => i, "g" => i % 10, "s" => format!("s{i:06}") }).collect();
        assert_eq!(coll.insert_many_documents(docs, true).await.unwrap().inserted_ids.len(), 120_000);
        assert_eq!(coll.estimated_document_count().await.unwrap(), 120_000);
        let o = FindOptions {
            sort: Some(doc! { "_id" => -1 }),
            projection: Some(doc! { "s" => 1 }),
            skip: Some(10),
            limit: Some(25),
            batch_size: Some(7),
            ..Default::default()
        };
        let got = coll.find(doc! { "g" => 3 }, o).await.unwrap().collect().await.unwrap();
        assert_eq!(got.len(), 25);
        assert_eq!(got[0].get_i64("_id"), Some(119_993 - 100));
        assert!(got.iter().all(|d| d.get("g").is_none() && d.get("s").is_some()), "projection");
        // A cursor over many getMore batches.
        let mut cur = coll.find(doc! {}, FindOptions { batch_size: Some(1000), ..Default::default() }).await.unwrap();
        let mut n = 0;
        while let Some(d) = cur.next().await {
            d.unwrap();
            n += 1;
        }
        assert_eq!(n, 120_000);
        // A cursor dropped early is killed on the server.
        let open_before = c.run_command("admin", doc! { "serverStatus" => 1 }).await.unwrap().get_document("metrics").and_then(|m| m.get_document("cursor")).and_then(|c| c.get_document("open")).and_then(|o| o.get_i64("total")).unwrap_or(0);
        {
            let mut cur = coll.find(doc! {}, FindOptions { batch_size: Some(10), ..Default::default() }).await.unwrap();
            cur.next().await.unwrap().unwrap();
            assert!(!cur.is_exhausted());
        }
        rt_core::sleep(std::time::Duration::from_millis(300)).await;
        let open_after = c.run_command("admin", doc! { "serverStatus" => 1 }).await.unwrap().get_document("metrics").and_then(|m| m.get_document("cursor")).and_then(|c| c.get_document("open")).and_then(|o| o.get_i64("total")).unwrap_or(0);
        assert!(open_after <= open_before, "killed: {open_before} → {open_after}");
        let cur = coll.find(doc! {}, FindOptions { batch_size: Some(10), ..Default::default() }).await.unwrap();
        cur.close().await.unwrap();
        // distinct, counts, hint, collation, comment, maxTimeMS.
        assert_eq!(coll.distinct("g", doc! { "_id" => doc! { "$lt" => 100 } }).await.unwrap().len(), 10);
        assert_eq!(coll.count_documents(doc! { "g" => 0 }).await.unwrap(), 12_000);
        let o = FindOptions { hint: Some(Bson::from(doc! { "_id" => 1 })), collation: Some(doc! { "locale" => "en", "strength" => 2 }), comment: Some(Bson::from("kw")), max_time: Some(std::time::Duration::from_secs(5)), limit: Some(1), ..Default::default() };
        assert_eq!(coll.find(doc! { "s" => "S000001" }, o).await.unwrap().collect().await.unwrap().len(), 1, "case-insensitive collation");
        let err = coll.find(doc! { "$where" => "sleep(100) || true" }, FindOptions { max_time: Some(std::time::Duration::from_millis(1)), ..Default::default() }).await;
        assert!(err.is_err(), "maxTimeMS exceeded");
        coll.drop().await.unwrap();
    });
}

#[test]
fn writes_updates_and_limits() {
    let Some(url) = env("KERNWAY_MONGO_URL") else { return };
    block(async move {
        let (_c, coll) = fresh(&url, "writes").await;
        coll.insert_many_documents((0..10).map(|i| doc! { "_id" => i, "n" => i, "arr" => vec![Bson::Int32(1), Bson::Int32(5), Bson::Int32(9)] }).collect(), true).await.unwrap();
        let r = coll.update_many(doc! { "n" => doc! { "$gte" => 5 } }, doc! { "$inc" => doc! { "n" => 100 } }, UpdateOptions::default()).await.unwrap();
        assert_eq!((r.matched, r.modified), (5, 5));
        // Array filters.
        let o = UpdateOptions { array_filters: Some(vec![doc! { "x" => doc! { "$gt" => 4 } }]), ..Default::default() };
        coll.update_one(doc! { "_id" => 0 }, doc! { "$set" => doc! { "arr.$[x]" => 0 } }, o).await.unwrap();
        let d = coll.find_one(doc! { "_id" => 0 }, FindOptions::default()).await.unwrap().unwrap();
        assert_eq!(d.get_array("arr").unwrap(), &vec![Bson::Int32(1), Bson::Int32(0), Bson::Int32(0)]);
        // Pipeline updates.
        coll.update_one(doc! { "_id" => 1 }, vec![Bson::from(doc! { "$set" => doc! { "twice" => doc! { "$multiply" => vec![Bson::from("$n"), Bson::Int32(2)] } } })], UpdateOptions::default()).await.unwrap();
        assert_eq!(coll.find_one(doc! { "_id" => 1 }, FindOptions::default()).await.unwrap().unwrap().get_i64("twice"), Some(2));
        // Upsert and replace.
        let r = coll.update_one(doc! { "_id" => 50 }, doc! { "$set" => doc! { "n" => 50 } }, UpdateOptions { upsert: true, ..Default::default() }).await.unwrap();
        assert_eq!(r.upserted_id, Some(Bson::Int32(50)));
        let r = coll.replace_one(doc! { "_id" => 50 }, &doc! { "_id" => 50, "replaced" => true }, UpdateOptions::default()).await.unwrap();
        assert_eq!(r.modified, 1);
        assert!(coll.replace_one(doc! { "_id" => 50 }, &doc! { "$set" => doc! { "a" => 1 } }, UpdateOptions::default()).await.is_err());
        // find-and-modify.
        let o = FindOneAndModifyOptions { sort: Some(doc! { "n" => -1 }), return_after: true, ..Default::default() };
        let top = coll.find_one_and_update(doc! {}, doc! { "$set" => doc! { "top" => true } }, o).await.unwrap().unwrap();
        assert_eq!(top.get_i64("n"), Some(109));
        let gone = coll.find_one_and_delete(doc! { "_id" => 2 }, Default::default()).await.unwrap().unwrap();
        assert_eq!(gone.get_i64("n"), Some(2));
        assert_eq!(coll.find_one_and_delete(doc! { "_id" => 2 }, Default::default()).await.unwrap(), None);
        let before = coll.find_one_and_replace(doc! { "_id" => 3 }, &doc! { "_id" => 3, "r" => 1 }, Default::default()).await.unwrap().unwrap();
        assert_eq!(before.get_i64("n"), Some(3), "the document before");
        assert_eq!(coll.delete_one(doc! { "_id" => 3 }).await.unwrap(), 1);
        assert_eq!(coll.delete_many(doc! { "n" => doc! { "$gte" => 100 } }).await.unwrap(), 5);
        // Documents over 16 MiB are refused before sending; under it, accepted.
        let big = "x".repeat(16 * 1024 * 1024);
        let err = coll.insert_one(&doc! { "big" => big }).await.unwrap_err();
        assert_eq!(err.code(), Some(10334), "{err}");
        coll.insert_one(&doc! { "almost" => "y".repeat(15 * 1024 * 1024) }).await.unwrap();
        // Unordered inserts keep going past failures and report each.
        let docs = vec![doc! { "_id" => 100 }, doc! { "_id" => 0 }, doc! { "_id" => 101 }, doc! { "_id" => 1 }, doc! { "_id" => 102 }];
        let err = coll.insert_many_documents(docs, false).await.unwrap_err();
        let ErrorKind::Write { write_errors, .. } = &err.kind else { panic!("{err}") };
        assert_eq!(write_errors.iter().map(|w| w.0).collect::<Vec<_>>(), [1, 3]);
        assert_eq!(coll.count_documents(doc! { "_id" => doc! { "$gte" => 100, "$lte" => 102 } }).await.unwrap(), 3);
        coll.drop().await.unwrap();
        coll.drop().await.unwrap();
    });
}

#[test]
fn bulk_aggregate_and_indexes() {
    let Some(url) = env("KERNWAY_MONGO_URL") else { return };
    block(async move {
        let (c, coll) = fresh(&url, "bulk").await;
        let r = coll
            .bulk_write(
                vec![
                    WriteModel::InsertOne(doc! { "_id" => 1, "k" => "a" }),
                    WriteModel::InsertOne(doc! { "_id" => 2, "k" => "b" }),
                    WriteModel::UpdateOne { filter: doc! { "_id" => 1 }, update: Bson::from(doc! { "$set" => doc! { "v" => 1 } }), options: Default::default() },
                    WriteModel::UpdateMany { filter: doc! {}, update: Bson::from(doc! { "$inc" => doc! { "c" => 1 } }), options: Default::default() },
                    WriteModel::ReplaceOne { filter: doc! { "_id" => 9 }, replacement: doc! { "k" => "z" }, options: UpdateOptions { upsert: true, ..Default::default() } },
                    WriteModel::DeleteOne { filter: doc! { "_id" => 2 } },
                    WriteModel::InsertOne(doc! { "_id" => 3 }),
                ],
                true,
            )
            .await
            .unwrap();
        assert_eq!((r.inserted, r.matched, r.modified, r.deleted), (3, 3, 3, 1));
        assert_eq!(r.upserted, vec![(4, Bson::Int32(9))]);
        // Ordered bulk stops at the first error; its index is the model's.
        let err = coll.bulk_write(vec![WriteModel::InsertOne(doc! { "_id" => 10 }), WriteModel::InsertOne(doc! { "_id" => 1 }), WriteModel::InsertOne(doc! { "_id" => 11 })], true).await.unwrap_err();
        let ErrorKind::Write { write_errors, .. } = &err.kind else { panic!("{err}") };
        assert_eq!(write_errors[0].0, 1);
        assert_eq!(coll.count_documents(doc! { "_id" => 11 }).await.unwrap(), 0);
        // Aggregation, including $out to another collection.
        let out = unique("bulk_out");
        let mut cur = coll.aggregate(vec![doc! { "$group" => doc! { "_id" => Bson::Null, "n" => doc! { "$sum" => 1 } } }]).await.unwrap();
        assert_eq!(cur.next().await.unwrap().unwrap().get_i64("n"), Some(4));
        coll.aggregate(vec![doc! { "$match" => doc! { "c" => 1 } }, doc! { "$out" => out.clone() }]).await.unwrap();
        assert_eq!(c.database("kernway_test").collection::<Document>(&out).count_documents(doc! {}).await.unwrap(), 1, "_id 1 (2 was deleted)");
        c.database("kernway_test").collection::<Document>(&out).drop().await.unwrap();
        let names = c.database("kernway_test").list_collection_names().await.unwrap();
        assert!(names.contains(&coll.name().to_string()));
        // Indexes: unique, TTL, partial, text.
        let names = coll
            .create_indexes(vec![
                IndexModel { keys: doc! { "k" => 1 }, unique: true, partial_filter: Some(doc! { "k" => doc! { "$exists" => true } }), ..Default::default() },
                IndexModel { keys: doc! { "at" => 1 }, expire_after: Some(std::time::Duration::from_secs(3600)), ..Default::default() },
                IndexModel { keys: doc! { "body" => "text" }, name: Some("body_text".into()), ..Default::default() },
            ])
            .await
            .unwrap();
        assert_eq!(names, ["k_1", "at_1", "body_text"]);
        let listed = coll.list_indexes().await.unwrap();
        assert_eq!(listed.len(), 4, "with _id");
        assert!(coll.insert_one(&doc! { "k" => "a" }).await.unwrap_err().is_duplicate_key());
        coll.insert_one(&doc! { "body" => "the quick brown fox" }).await.unwrap();
        let hits = coll.find(doc! { "$text" => doc! { "$search" => "fox" } }, FindOptions::default()).await.unwrap().collect().await.unwrap();
        assert_eq!(hits.len(), 1);
        coll.drop_index("k_1").await.unwrap();
        coll.insert_one(&doc! { "k" => "a" }).await.unwrap();
        coll.drop().await.unwrap();
    });
}

#[test]
fn gridfs_files() {
    let Some(url) = env("KERNWAY_MONGO_URL") else { return };
    block(async move {
        let c = Client::connect(&url).await.unwrap();
        let bucket_name = unique("fs");
        let fs = c.database("kernway_test").gridfs(&bucket_name).with_chunk_size(64 * 1024);
        let data: Vec<u8> = (0..1_000_003u32).map(|i| (i * 31 % 251) as u8).collect();
        let id = fs.upload("a.bin", &data, Some(doc! { "owner" => "kw" })).await.unwrap();
        assert_eq!(fs.download(id).await.unwrap(), data);
        let v2 = fs.upload("a.bin", b"second", None).await.unwrap();
        assert_eq!(fs.download_by_name("a.bin", -1).await.unwrap(), b"second");
        assert_eq!(fs.download_by_name("a.bin", 0).await.unwrap(), data);
        let empty = fs.upload("empty", b"", None).await.unwrap();
        assert!(fs.download(empty).await.unwrap().is_empty());
        assert_eq!(fs.find(doc! { "metadata.owner" => "kw" }).await.unwrap().len(), 1);
        fs.rename(v2, "b.bin").await.unwrap();
        assert_eq!(fs.download_by_name("b.bin", -1).await.unwrap(), b"second");
        fs.delete(id).await.unwrap();
        assert!(fs.download(id).await.is_err());
        assert!(fs.delete(id).await.is_err());
        // A missing chunk is detected, not silently truncated.
        let id = fs.upload("c.bin", &data, None).await.unwrap();
        c.database("kernway_test").collection::<Document>(&format!("{bucket_name}.chunks")).delete_one(doc! { "files_id" => id, "n" => 3 }).await.unwrap();
        assert!(fs.download(id).await.is_err());
        for n in ["files", "chunks"] {
            c.database("kernway_test").collection::<Document>(&format!("{bucket_name}.{n}")).drop().await.unwrap();
        }
    });
}
