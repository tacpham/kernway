//! Entity operations of the unified test format, run through the driver's
//! public API. Every argument an operation is given must be used: one the
//! driver has no way to honour makes the operation `Unsupported` (a gap, never
//! silently ignored).

use super::entities::{Entities, Entity};
use kernway_orm_mongodb::{
    AggregateOptions, Bson, BulkWriteOptions, ChangeStreamOptions, Collection, CountOptions, DeleteOptions, DistinctOptions, Document, Error, ErrorKind,
    EstimatedCountOptions, FindOneAndModifyOptions, FindOptions, IndexModel, InsertOptions, ReadConcern, ReadMode, ReadPreference, Session,
    TransactionOptions, UpdateOptions, WriteConcern, WriteModel,
};
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::Duration;

/// Why an operation produced no result.
pub enum OpError {
    /// The driver (or server) failed it: compared with `expectError`.
    Driver(Error),
    /// The driver cannot express what the test asks (an argument, an
    /// operation, an option): the test is a gap, not a pass or a failure.
    Unsupported(String),
    /// The test runner could not run it (a malformed test or a runner bug).
    Runner(String),
}

impl From<Error> for OpError {
    fn from(e: Error) -> Self {
        OpError::Driver(e)
    }
}

/// An operation's result, and whether documents in it are root-level
/// (results of iterables) for matching.
pub struct Outcome {
    pub value: Option<Bson>,
    pub iterable: bool,
}

fn value(v: impl Into<Bson>) -> Result<Outcome, OpError> {
    Ok(Outcome { value: Some(v.into()), iterable: false })
}

fn none() -> Result<Outcome, OpError> {
    Ok(Outcome { value: None, iterable: false })
}

fn docs(v: Vec<Document>) -> Result<Outcome, OpError> {
    Ok(Outcome { value: Some(Bson::Array(v.into_iter().map(Bson::Document).collect())), iterable: true })
}

/// The arguments, tracking which ones were used.
pub struct Args {
    d: Document,
    used: RefCell<BTreeSet<String>>,
    op: String,
}

impl Args {
    pub fn new(op: &str, d: Option<&Document>) -> Self {
        Args { d: d.cloned().unwrap_or_default(), used: RefCell::new(BTreeSet::new()), op: op.to_string() }
    }
    fn get(&self, k: &str) -> Option<&Bson> {
        self.used.borrow_mut().insert(k.to_string());
        self.d.get(k)
    }
    fn doc(&self, k: &str) -> Result<Document, OpError> {
        match self.get(k) {
            Some(Bson::Document(d)) => Ok(d.clone()),
            None => Ok(Document::new()),
            other => Err(OpError::Runner(format!("{}: {k} is {other:?}, not a document", self.op))),
        }
    }
    fn opt_doc(&self, k: &str) -> Option<Document> {
        self.get(k).and_then(Bson::as_document).cloned()
    }
    fn docs(&self, k: &str) -> Result<Vec<Document>, OpError> {
        match self.get(k) {
            Some(Bson::Array(a)) => a.iter().map(|x| x.as_document().cloned().ok_or_else(|| OpError::Runner(format!("{}: {k} holds a non-document", self.op)))).collect(),
            None => Ok(Vec::new()),
            other => Err(OpError::Runner(format!("{}: {k} is {other:?}", self.op))),
        }
    }
    fn str(&self, k: &str) -> Option<String> {
        self.get(k).and_then(Bson::as_str).map(String::from)
    }
    fn bool(&self, k: &str) -> Option<bool> {
        self.get(k).and_then(Bson::as_bool)
    }
    fn int(&self, k: &str) -> Option<i64> {
        self.get(k).and_then(Bson::as_i64)
    }
    fn bson(&self, k: &str) -> Option<Bson> {
        self.get(k).cloned()
    }
    /// Arguments that were given but not used.
    fn finish(&self) -> Result<(), OpError> {
        let used = self.used.borrow();
        let unused: Vec<&str> = self.d.keys().filter(|k| !used.contains(*k)).collect();
        if unused.is_empty() {
            Ok(())
        } else {
            Err(OpError::Unsupported(format!("{} argument(s) {}", self.op, unused.join(", "))))
        }
    }
}

fn session_of(args: &Args, e: &Entities) -> Result<Option<Rc<Session>>, OpError> {
    match args.str("session") {
        Some(id) => e.session(&id).map(Some).map_err(OpError::Runner),
        None => Ok(None),
    }
}

pub fn read_concern(d: &Document) -> Option<ReadConcern> {
    d.get_str("level").map(|l| ReadConcern(l.to_string()))
}

pub fn write_concern(d: &Document) -> WriteConcern {
    WriteConcern {
        w: d.get("w").cloned(),
        journal: d.get_bool("j").or(d.get_bool("journal")),
        timeout: d.get_i64("wtimeout").or(d.get_i64("wtimeoutMS")).map(|ms| Duration::from_millis(ms as u64)),
    }
}

pub fn read_preference(d: &Document) -> Result<ReadPreference, OpError> {
    let mode = match d.get_str("mode").unwrap_or("primary") {
        "primary" | "Primary" => ReadMode::Primary,
        "primaryPreferred" | "PrimaryPreferred" => ReadMode::PrimaryPreferred,
        "secondary" | "Secondary" => ReadMode::Secondary,
        "secondaryPreferred" | "SecondaryPreferred" => ReadMode::SecondaryPreferred,
        "nearest" | "Nearest" => ReadMode::Nearest,
        other => return Err(OpError::Runner(format!("read preference mode {other}"))),
    };
    let mut rp = ReadPreference::new(mode);
    if let Some(sets) = d.get_array("tagSets").or(d.get_array("tag_sets")) {
        rp.tag_sets = sets
            .iter()
            .filter_map(Bson::as_document)
            .map(|s| s.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.to_string(), v.to_string()))).collect())
            .collect();
    }
    if let Some(s) = d.get_i64("maxStalenessSeconds").filter(|s| *s > 0) {
        rp.max_staleness = Some(Duration::from_secs(s as u64));
    }
    if d.contains_key("hedge") {
        return Err(OpError::Unsupported("read preference hedge".into()));
    }
    Ok(rp)
}

pub fn transaction_options(d: &Document) -> Result<TransactionOptions, OpError> {
    let mut o = TransactionOptions::default();
    for (k, v) in d.iter() {
        match (k, v) {
            ("readConcern", Bson::Document(rc)) => o.read_concern = read_concern(rc),
            ("writeConcern", Bson::Document(wc)) => o.write_concern = Some(write_concern(wc)),
            ("readPreference", Bson::Document(rp)) => o.read_preference = Some(read_preference(rp)?),
            ("maxCommitTimeMS", v) => o.max_commit_time = v.as_i64().map(|ms| Duration::from_millis(ms as u64)),
            (k, _) => return Err(OpError::Unsupported(format!("transaction option {k}"))),
        }
    }
    Ok(o)
}

fn coll(c: &Collection<Document>, session: &Option<Rc<Session>>) -> Collection<Document> {
    match session {
        Some(s) => c.in_session(s),
        None => c.clone(),
    }
}

fn ms(args: &Args, k: &str) -> Option<Duration> {
    args.int(k).map(|ms| Duration::from_millis(ms as u64))
}

fn update_options(args: &Args) -> UpdateOptions {
    UpdateOptions {
        upsert: args.bool("upsert").unwrap_or(false),
        array_filters: args.get("arrayFilters").and_then(Bson::as_array).map(|a| a.iter().filter_map(Bson::as_document).cloned().collect()),
        collation: args.opt_doc("collation"),
        hint: args.bson("hint"),
        bypass_document_validation: args.bool("bypassDocumentValidation"),
        let_vars: args.opt_doc("let"),
        comment: args.bson("comment"),
        sort: args.opt_doc("sort"),
        raw_data: args.bool("rawData"),
    }
}

fn delete_options(args: &Args) -> DeleteOptions {
    DeleteOptions { collation: args.opt_doc("collation"), hint: args.bson("hint"), let_vars: args.opt_doc("let"), comment: args.bson("comment"), raw_data: args.bool("rawData") }
}

fn aggregate_options(args: &Args) -> AggregateOptions {
    AggregateOptions {
        allow_disk_use: args.bool("allowDiskUse"),
        batch_size: args.int("batchSize").map(|b| b as u32),
        bypass_document_validation: args.bool("bypassDocumentValidation"),
        collation: args.opt_doc("collation"),
        comment: args.bson("comment"),
        hint: args.bson("hint"),
        let_vars: args.opt_doc("let"),
        max_time: ms(args, "maxTimeMS"),
        raw_data: args.bool("rawData"),
    }
}

fn update_result(r: kernway_orm_mongodb::UpdateResult) -> Document {
    let mut d = Document::new().with("matchedCount", r.matched as i64).with("modifiedCount", r.modified as i64).with("upsertedCount", i64::from(r.upserted_id.is_some()));
    if let Some(id) = r.upserted_id {
        d.insert("upsertedId", id);
    }
    d
}

fn find_options(args: &Args) -> Result<FindOptions, OpError> {
    Ok(FindOptions {
        sort: args.opt_doc("sort"),
        projection: args.opt_doc("projection"),
        skip: args.int("skip").map(|s| s as u64),
        limit: args.int("limit"),
        batch_size: args.int("batchSize").map(|b| b as u32),
        hint: args.bson("hint"),
        collation: args.opt_doc("collation"),
        max_time: args.int("maxTimeMS").map(|ms| Duration::from_millis(ms as u64)),
        comment: args.bson("comment"),
        allow_disk_use: args.bool("allowDiskUse"),
        read_preference: None,
        let_vars: args.opt_doc("let"),
        allow_partial_results: args.bool("allowPartialResults"),
        max: args.opt_doc("max"),
        min: args.opt_doc("min"),
        return_key: args.bool("returnKey"),
        show_record_id: args.bool("showRecordId"),
        no_cursor_timeout: args.bool("noCursorTimeout"),
        raw_data: args.bool("rawData"),
    })
}

fn fam_options(args: &Args) -> FindOneAndModifyOptions {
    FindOneAndModifyOptions {
        sort: args.opt_doc("sort"),
        projection: args.opt_doc("projection"),
        upsert: args.bool("upsert").unwrap_or(false),
        return_after: args.str("returnDocument").is_some_and(|r| r.eq_ignore_ascii_case("after")),
        array_filters: args.get("arrayFilters").and_then(Bson::as_array).map(|a| a.iter().filter_map(Bson::as_document).cloned().collect()),
        collation: args.opt_doc("collation"),
        hint: args.bson("hint"),
        let_vars: args.opt_doc("let"),
        comment: args.bson("comment"),
        max_time: ms(args, "maxTimeMS"),
        bypass_document_validation: args.bool("bypassDocumentValidation"),
        raw_data: args.bool("rawData"),
    }
}

fn write_model(m: &Document) -> Result<WriteModel, OpError> {
    let (kind, body) = m.iter().next().ok_or_else(|| OpError::Runner("empty write model".into()))?;
    let body = body.as_document().ok_or_else(|| OpError::Runner("write model body".into()))?;
    let a = Args::new(kind, Some(body));
    let model = match kind {
        "insertOne" => WriteModel::InsertOne(a.doc("document")?),
        "updateOne" | "updateMany" => {
            let filter = a.doc("filter")?;
            let update = a.bson("update").ok_or_else(|| OpError::Runner("update model without update".into()))?;
            let options = update_options(&a);
            if kind == "updateOne" {
                WriteModel::UpdateOne { filter, update, options }
            } else {
                WriteModel::UpdateMany { filter, update, options }
            }
        }
        "replaceOne" => WriteModel::ReplaceOne { filter: a.doc("filter")?, replacement: a.doc("replacement")?, options: update_options(&a) },
        "deleteOne" => WriteModel::DeleteOne { filter: a.doc("filter")?, options: DeleteOptions { collation: a.opt_doc("collation"), hint: a.bson("hint"), ..Default::default() } },
        "deleteMany" => WriteModel::DeleteMany { filter: a.doc("filter")?, options: DeleteOptions { collation: a.opt_doc("collation"), hint: a.bson("hint"), ..Default::default() } },
        other => return Err(OpError::Runner(format!("write model {other}"))),
    };
    a.finish()?;
    Ok(model)
}

fn index_doc(keys: &Document, name: Option<&str>) -> Document {
    let mut d = Document::new().with("key", Bson::Document(keys.clone()));
    if let Some(n) = name {
        d.insert("name", n);
    }
    d
}

fn change_stream_options(args: &Args) -> ChangeStreamOptions {
    ChangeStreamOptions {
        full_document: args.str("fullDocument"),
        full_document_before_change: args.str("fullDocumentBeforeChange"),
        resume_after: args.bson("resumeAfter"),
        start_after: args.bson("startAfter"),
        start_at_operation_time: match args.get("startAtOperationTime") {
            Some(Bson::Timestamp(t)) => Some(*t),
            _ => None,
        },
        max_await: args.int("maxAwaitTimeMS").map(|ms| Duration::from_millis(ms as u64)),
        batch_size: args.int("batchSize").map(|b| b as u32),
        comment: args.bson("comment"),
        show_expanded_events: args.bool("showExpandedEvents"),
    }
}

/// Run one entity operation.
pub async fn run(object: &str, name: &str, arguments: Option<&Document>, e: &mut Entities) -> Result<Outcome, OpError> {
    let args = Args::new(name, arguments);
    let out = run_inner(object, name, &args, e).await;
    if matches!(out, Err(OpError::Unsupported(_)) | Err(OpError::Runner(_))) {
        return out;
    }
    // An unused argument outranks the outcome: the driver ignored part of the test.
    if let Err(u @ OpError::Unsupported(_)) = args.finish() {
        return Err(u);
    }
    out
}

async fn run_inner(object: &str, name: &str, args: &Args, e: &mut Entities) -> Result<Outcome, OpError> {
    let kind = match e.get(object) {
        Some(Entity::Client(_)) => "client",
        Some(Entity::Database(_)) => "database",
        Some(Entity::Collection(_)) => "collection",
        Some(Entity::Session(..)) => "session",
        Some(Entity::Bucket(_)) => "bucket",
        Some(Entity::Cursor(_)) => "cursor",
        Some(Entity::ChangeStream(_)) => "changeStream",
        Some(Entity::Value(_)) | None => return Err(OpError::Runner(format!("no entity {object}"))),
    };
    match kind {
        "collection" => collection_op(object, name, args, e).await,
        "database" => database_op(object, name, args, e).await,
        "client" => client_op(object, name, args, e).await,
        "session" => session_op(object, name, args, e).await,
        "bucket" => bucket_op(object, name, args, e).await,
        "cursor" | "changeStream" => cursor_op(object, name, args, e).await,
        _ => unreachable!(),
    }
}

async fn collection_op(object: &str, name: &str, args: &Args, e: &mut Entities) -> Result<Outcome, OpError> {
    let base = e.collection(object).map_err(OpError::Runner)?;
    let session = session_of(args, e)?;
    let c = coll(&base, &session);
    match name {
        "insertOne" => {
            let d = args.doc("document")?;
            let id = c.insert_one_with(&d, InsertOptions { bypass_document_validation: args.bool("bypassDocumentValidation"), comment: args.bson("comment"), raw_data: args.bool("rawData") }).await?;
            value(Document::new().with("insertedId", id))
        }
        "insertMany" => {
            let d = args.docs("documents")?;
            let io = InsertOptions { bypass_document_validation: args.bool("bypassDocumentValidation"), comment: args.bson("comment"), raw_data: args.bool("rawData") };
            let r = c.insert_many_documents_with(d, args.bool("ordered").unwrap_or(true), io).await?;
            let ids: Document = r.inserted_ids.into_iter().enumerate().map(|(i, id)| (i.to_string(), id)).collect();
            value(Document::new().with("insertedIds", ids))
        }
        "updateOne" | "updateMany" => {
            let filter = args.doc("filter")?;
            let update = args.bson("update").ok_or_else(|| OpError::Runner("no update".into()))?;
            let o = update_options(args);
            let r = if name == "updateOne" { c.update_one(filter, update, o).await? } else { c.update_many(filter, update, o).await? };
            value(update_result(r))
        }
        "replaceOne" => {
            let filter = args.doc("filter")?;
            let replacement = args.doc("replacement")?;
            let r = c.replace_one(filter, &replacement, update_options(args)).await?;
            value(update_result(r))
        }
        "deleteOne" | "deleteMany" => {
            let filter = args.doc("filter")?;
            let o = delete_options(args);
            let n = if name == "deleteOne" { c.delete_one_with(filter, o).await? } else { c.delete_many_with(filter, o).await? };
            value(Document::new().with("deletedCount", n as i64))
        }
        "find" | "findOne" => {
            let filter = args.doc("filter")?;
            let o = find_options(args)?;
            if name == "findOne" {
                return Ok(Outcome { value: Some(c.find_one(filter, o).await?.map_or(Bson::Null, Bson::Document)), iterable: false });
            }
            docs(c.find(filter, o).await?.collect().await?)
        }
        "createFindCursor" => {
            let filter = args.doc("filter")?;
            let cur = c.find(filter, find_options(args)?).await?;
            CURSOR.with(|slot| *slot.borrow_mut() = Some(cur));
            Ok(Outcome { value: Some(Bson::Null), iterable: false })
        }
        "countDocuments" => {
            let filter = args.doc("filter")?;
            let o = CountOptions {
                skip: args.int("skip").map(|n| n as u64),
                limit: args.int("limit").map(|n| n as u64),
                collation: args.opt_doc("collation"),
                hint: args.bson("hint"),
                comment: args.bson("comment"),
                max_time: ms(args, "maxTimeMS"),
                raw_data: args.bool("rawData"),
            };
            value(c.count_documents_with(filter, o).await? as i64)
        }
        "estimatedDocumentCount" => {
            let o = EstimatedCountOptions { max_time: ms(args, "maxTimeMS"), comment: args.bson("comment"), raw_data: args.bool("rawData") };
            value(c.estimated_document_count_with(o).await? as i64)
        }
        "distinct" => {
            let field = args.str("fieldName").ok_or_else(|| OpError::Runner("no fieldName".into()))?;
            let filter = args.doc("filter")?;
            let o = DistinctOptions { collation: args.opt_doc("collation"), comment: args.bson("comment"), max_time: ms(args, "maxTimeMS"), hint: args.bson("hint"), raw_data: args.bool("rawData") };
            value(Bson::Array(c.distinct_with(&field, filter, o).await?))
        }
        "aggregate" => {
            let pipeline = args.docs("pipeline")?;
            docs(c.aggregate_with(pipeline, aggregate_options(args)).await?.collect().await?)
        }
        "findOneAndUpdate" => {
            let filter = args.doc("filter")?;
            let update = args.bson("update").ok_or_else(|| OpError::Runner("no update".into()))?;
            let r = c.find_one_and_update(filter, update, fam_options(args)).await?;
            value(r.map_or(Bson::Null, Bson::Document))
        }
        "findOneAndReplace" => {
            let filter = args.doc("filter")?;
            let replacement = args.doc("replacement")?;
            let r = c.find_one_and_replace(filter, &replacement, fam_options(args)).await?;
            value(r.map_or(Bson::Null, Bson::Document))
        }
        "findOneAndDelete" => {
            let filter = args.doc("filter")?;
            let r = c.find_one_and_delete(filter, fam_options(args)).await?;
            value(r.map_or(Bson::Null, Bson::Document))
        }
        "bulkWrite" => {
            let models = args.docs("requests")?.iter().map(write_model).collect::<Result<Vec<_>, _>>()?;
            let bo = BulkWriteOptions {
                ordered: args.bool("ordered").unwrap_or(true),
                bypass_document_validation: args.bool("bypassDocumentValidation"),
                comment: args.bson("comment"),
                let_vars: args.opt_doc("let"),
                raw_data: args.bool("rawData"),
            };
            let r = c.bulk_write_with(models, bo).await?;
            value(bulk_result_doc(&r))
        }
        "count" => {
            let filter = args.doc("filter")?;
            let o = CountOptions {
                skip: args.int("skip").map(|n| n as u64),
                limit: args.int("limit").map(|n| n as u64),
                collation: args.opt_doc("collation"),
                hint: args.bson("hint"),
                comment: args.bson("comment"),
                max_time: ms(args, "maxTimeMS"),
                raw_data: args.bool("rawData"),
            };
            #[allow(deprecated)]
            let n = c.count(filter, o).await?;
            value(n as i64)
        }
        "createIndex" => {
            let keys = args.doc("keys")?;
            let mut m = IndexModel { keys, name: args.str("name"), unique: args.bool("unique").unwrap_or(false), ..Default::default() };
            if let Some(b) = args.bool("sparse") {
                m.sparse = b;
            }
            value(c.create_index(m).await?)
        }
        "listIndexes" => docs(c.list_indexes().await?),
        "listIndexNames" => {
            let names: Vec<Bson> = c.list_indexes().await?.iter().filter_map(|d| d.get_str("name").map(Bson::from)).collect();
            value(Bson::Array(names))
        }
        "dropIndexes" => {
            c.drop_indexes().await?;
            none()
        }
        "dropIndex" => {
            let n = args.str("name").ok_or_else(|| OpError::Runner("no index name".into()))?;
            c.drop_index(&n).await?;
            none()
        }
        "drop" => {
            c.drop().await?;
            none()
        }
        "createChangeStream" | "watch" => {
            let pipeline = args.docs("pipeline")?;
            let cs = c.watch(pipeline, change_stream_options(args)).await?;
            CHANGE_STREAM.with(|slot| *slot.borrow_mut() = Some(cs));
            Ok(Outcome { value: Some(Bson::Null), iterable: false })
        }
        "rename" => {
            let to = args.str("to").ok_or_else(|| OpError::Runner("no to".into()))?;
            c.rename(&to, args.bool("dropTarget").unwrap_or(false)).await?;
            none()
        }
        "createSearchIndex" | "createSearchIndexes" => {
            let model = |d: &Document| kernway_orm_mongodb::SearchIndexModel {
                definition: d.get_document("definition").cloned().unwrap_or_default(),
                name: d.get_str("name").map(String::from),
                kind: d.get_str("type").map(String::from),
            };
            if name == "createSearchIndex" {
                let m = args.doc("model")?;
                value(c.create_search_index(model(&m)).await?)
            } else {
                let ms: Vec<_> = args.docs("models")?.iter().map(model).collect();
                value(Bson::Array(c.create_search_indexes(ms).await?.into_iter().map(Bson::from).collect()))
            }
        }
        "dropSearchIndex" => {
            let n = args.str("name").unwrap_or_default();
            c.drop_search_index(&n).await?;
            none()
        }
        "updateSearchIndex" => {
            let n = args.str("name").unwrap_or_default();
            c.update_search_index(&n, args.doc("definition")?).await?;
            none()
        }
        "listSearchIndexes" => {
            let n = args.str("name");
            let o = args.opt_doc("aggregationOptions").map(|d| aggregate_options(&Args::new("aggregationOptions", Some(&d)))).unwrap_or_default();
            docs(c.list_search_indexes(n.as_deref(), o).await?.collect().await?)
        }
        other => Err(OpError::Unsupported(format!("collection operation {other}"))),
    }
}

thread_local! {
    /// A cursor / change stream just created, for `saveResultAsEntity`.
    pub static CURSOR: RefCell<Option<kernway_orm_mongodb::Cursor<Document>>> = const { RefCell::new(None) };
    pub static CHANGE_STREAM: RefCell<Option<kernway_orm_mongodb::ChangeStream>> = const { RefCell::new(None) };
}

async fn database_op(object: &str, name: &str, args: &Args, e: &mut Entities) -> Result<Outcome, OpError> {
    let db = e.database(object).map_err(OpError::Runner)?;
    let session = session_of(args, e)?;
    let db = match &session {
        Some(s) => db.in_session(s),
        None => db,
    };
    match name {
        "runCommand" => {
            let cmd = args.doc("command")?;
            let _ = args.str("commandName");
            let rp = args.opt_doc("readPreference").map(|d| read_preference(&d)).transpose()?;
            let reply = match rp {
                Some(rp) => db.run_command_with(cmd, &rp).await?,
                None => db.run_command(cmd).await?,
            };
            Ok(Outcome { value: Some(Bson::Document(reply)), iterable: false })
        }
        "runCursorCommand" | "createCommandCursor" => {
            let cmd = args.doc("command")?;
            let _ = args.str("commandName");
            let rp = args.opt_doc("readPreference").map(|d| read_preference(&d)).transpose()?;
            let cur = db.run_cursor_command(cmd, rp.as_ref(), args.int("batchSize").map(|b| b as u32), ms(args, "maxTimeMS"), args.bson("comment")).await?;
            if name == "runCursorCommand" {
                return docs(cur.collect().await?);
            }
            CURSOR.with(|slot| *slot.borrow_mut() = Some(cur));
            Ok(Outcome { value: Some(Bson::Null), iterable: false })
        }
        "listCollections" | "listCollectionObjects" => {
            let filter = args.doc("filter")?;
            docs(db.list_collections(filter).await?)
        }
        "listCollectionNames" => {
            let filter = args.doc("filter")?;
            value(Bson::Array(db.list_collection_names_with(filter).await?.into_iter().map(Bson::from).collect()))
        }
        "createCollection" => {
            let n = args.str("collection").ok_or_else(|| OpError::Runner("no collection".into()))?;
            let mut opts = Document::new();
            for k in ["capped", "size", "max", "validator", "validationLevel", "validationAction", "timeseries", "expireAfterSeconds", "clusteredIndex", "changeStreamPreAndPostImages", "collation", "indexOptionDefaults", "viewOn", "pipeline", "encryptedFields", "storageEngine"] {
                if let Some(v) = args.bson(k) {
                    opts.insert(k, v);
                }
            }
            db.create_collection(&n, opts).await?;
            none()
        }
        "modifyCollection" => {
            // collMod through the driver's command API.
            let n = args.str("collection").ok_or_else(|| OpError::Runner("no collection".into()))?;
            let mut cmd = Document::new().with("collMod", n);
            for k in ["validator", "validationLevel", "validationAction", "index", "expireAfterSeconds", "changeStreamPreAndPostImages", "timeseries"] {
                if let Some(v) = args.bson(k) {
                    cmd.insert(k, v);
                }
            }
            db.run_command(cmd).await?;
            none()
        }
        "dropCollection" => {
            let n = args.str("collection").ok_or_else(|| OpError::Runner("no collection".into()))?;
            db.collection::<Document>(&n).drop().await?;
            none()
        }
        "aggregate" => {
            let pipeline = args.docs("pipeline")?;
            docs(db.aggregate_with(pipeline, aggregate_options(args)).await?.collect().await?)
        }
        "createChangeStream" | "watch" => {
            let pipeline = args.docs("pipeline")?;
            let cs = db.watch(pipeline, change_stream_options(args)).await?;
            CHANGE_STREAM.with(|slot| *slot.borrow_mut() = Some(cs));
            Ok(Outcome { value: Some(Bson::Null), iterable: false })
        }
        other => Err(OpError::Unsupported(format!("database operation {other}"))),
    }
}

async fn client_op(object: &str, name: &str, args: &Args, e: &mut Entities) -> Result<Outcome, OpError> {
    let c = e.client(object).map_err(OpError::Runner)?;
    match name {
        "listDatabaseNames" => value(Bson::Array(c.client.list_database_names().await?.into_iter().map(Bson::from).collect())),
        "listDatabases" | "listDatabaseObjects" => {
            let filter = args.doc("filter")?;
            match session_of(args, e)? {
                Some(s) => docs(c.client.list_databases_in(filter, &s).await?),
                None => docs(c.client.list_databases(filter).await?),
            }
        }
        "createChangeStream" | "watch" => {
            let pipeline = args.docs("pipeline")?;
            let cs = c.client.watch(pipeline, change_stream_options(args)).await?;
            CHANGE_STREAM.with(|slot| *slot.borrow_mut() = Some(cs));
            Ok(Outcome { value: Some(Bson::Null), iterable: false })
        }
        "close" => {
            c.client.shutdown().await;
            none()
        }
        "dropDatabase" => {
            let n = args.str("database").ok_or_else(|| OpError::Runner("no database".into()))?;
            let db = c.client.database(&n);
            let db = match session_of(args, e)? {
                Some(s) => db.in_session(&s),
                None => db,
            };
            db.drop().await?;
            none()
        }
        "clientBulkWrite" => {
            let mut models = Vec::new();
            for m in args.docs("models")? {
                let (kind, body) = m.iter().next().ok_or_else(|| OpError::Runner("empty model".into()))?;
                let a = Args::new(kind, body.as_document());
                let namespace = a.str("namespace").unwrap_or_default();
                let model = match kind {
                    "insertOne" => kernway_orm_mongodb::ClientWriteModel::InsertOne { namespace, document: a.doc("document")? },
                    "updateOne" | "updateMany" => {
                        let filter = a.doc("filter")?;
                        let update = a.bson("update").ok_or_else(|| OpError::Runner("no update".into()))?;
                        let options = update_options(&a);
                        if kind == "updateOne" {
                            kernway_orm_mongodb::ClientWriteModel::UpdateOne { namespace, filter, update, options }
                        } else {
                            kernway_orm_mongodb::ClientWriteModel::UpdateMany { namespace, filter, update, options }
                        }
                    }
                    "replaceOne" => kernway_orm_mongodb::ClientWriteModel::ReplaceOne { namespace, filter: a.doc("filter")?, replacement: a.doc("replacement")?, options: update_options(&a) },
                    "deleteOne" | "deleteMany" => {
                        let filter = a.doc("filter")?;
                        let options = DeleteOptions { collation: a.opt_doc("collation"), hint: a.bson("hint"), ..Default::default() };
                        if kind == "deleteOne" {
                            kernway_orm_mongodb::ClientWriteModel::DeleteOne { namespace, filter, options }
                        } else {
                            kernway_orm_mongodb::ClientWriteModel::DeleteMany { namespace, filter, options }
                        }
                    }
                    other => return Err(OpError::Runner(format!("client write model {other}"))),
                };
                a.finish()?;
                models.push(model);
            }
            let o = kernway_orm_mongodb::ClientBulkWriteOptions {
                ordered: args.bool("ordered"),
                bypass_document_validation: args.bool("bypassDocumentValidation"),
                comment: args.bson("comment"),
                let_vars: args.opt_doc("let"),
                write_concern: args.opt_doc("writeConcern").map(|d| write_concern(&d)),
                verbose_results: args.bool("verboseResults"),
                raw_data: args.bool("rawData"),
            };
            let r = match session_of(args, e)? {
                Some(s) => c.client.bulk_write_in(models, o, Some(&s)).await?,
                None => c.client.bulk_write(models, o).await?,
            };
            value(client_bulk_doc(&r))
        }
        other => Err(OpError::Unsupported(format!("client operation {other}"))),
    }
}

async fn session_op(object: &str, name: &str, args: &Args, e: &mut Entities) -> Result<Outcome, OpError> {
    let s = e.session(object).map_err(OpError::Runner)?;
    match name {
        "startTransaction" => {
            let mut d = Document::new();
            for k in ["readConcern", "writeConcern", "readPreference", "maxCommitTimeMS"] {
                if let Some(v) = args.bson(k) {
                    d.insert(k, v);
                }
            }
            s.start_transaction(transaction_options(&d)?)?;
            none()
        }
        "commitTransaction" => {
            s.commit_transaction().await?;
            none()
        }
        "abortTransaction" => {
            s.abort_transaction().await?;
            none()
        }
        "getSnapshotTime" => match s.snapshot_time() {
            Some(t) => value(Bson::Timestamp(t)),
            None => Err(OpError::Runner("the session has no snapshot time yet".into())),
        },
        "endSession" => {
            e.record_lsids();
            (*s).clone().end().await;
            none()
        }
        other => Err(OpError::Unsupported(format!("session operation {other}"))),
    }
}

async fn bucket_op(object: &str, name: &str, args: &Args, e: &mut Entities) -> Result<Outcome, OpError> {
    let Some(Entity::Bucket(b)) = e.get(object) else { return Err(OpError::Runner(format!("no bucket {object}"))) };
    let b = (**b).clone();
    match name {
        "upload" => {
            let filename = args.str("filename").unwrap_or_default();
            // disableMD5 is deprecated: this driver never computes an MD5.
            let _ = args.bool("disableMD5");
            let source = match args.get("source") {
                Some(Bson::Binary(bin)) => bin.bytes.clone(),
                Some(Bson::Document(d)) if d.get_str("$$hexBytes").is_some() => {
                    super::hex(d.get_str("$$hexBytes").unwrap_or("")).ok_or_else(|| OpError::Runner("bad $$hexBytes".into()))?
                }
                other => return Err(OpError::Runner(format!("upload source {other:?}"))),
            };
            let b = match args.int("chunkSizeBytes") {
                Some(n) => b.with_chunk_size(n as usize),
                None => b,
            };
            let id = b.upload(&filename, &source, args.opt_doc("metadata")).await?;
            value(id)
        }
        "download" => {
            let id = args.bson("id").ok_or_else(|| OpError::Runner("no id".into()))?;
            value(Bson::Binary(kernway_orm_mongodb::Binary { subtype: 0, bytes: b.download(id).await? }))
        }
        "downloadByName" => {
            let filename = args.str("filename").unwrap_or_default();
            let revision = args.int("revision").unwrap_or(-1);
            value(Bson::Binary(kernway_orm_mongodb::Binary { subtype: 0, bytes: b.download_by_name(&filename, revision).await? }))
        }
        "delete" => {
            let id = args.bson("id").ok_or_else(|| OpError::Runner("no id".into()))?;
            b.delete(id).await?;
            none()
        }
        "rename" => {
            let id = args.bson("id").ok_or_else(|| OpError::Runner("no id".into()))?;
            let n = args.str("newFilename").unwrap_or_default();
            b.rename(id, &n).await?;
            none()
        }
        "renameByName" => {
            let f = args.str("filename").unwrap_or_default();
            let n = args.str("newFilename").unwrap_or_default();
            b.rename_by_name(&f, &n).await?;
            none()
        }
        "deleteByName" => {
            let f = args.str("filename").unwrap_or_default();
            b.delete_by_name(&f).await?;
            none()
        }
        "drop" => {
            b.drop().await?;
            none()
        }
        other => Err(OpError::Unsupported(format!("bucket operation {other}"))),
    }
}

async fn cursor_op(object: &str, name: &str, _args: &Args, e: &mut Entities) -> Result<Outcome, OpError> {
    // Taken out of the map while it is awaited, put back afterwards (closed: not).
    match e.remove(object) {
        Some(Entity::ChangeStream(mut cs)) => {
            let r = match name {
                "iterateUntilDocumentOrError" => match cs.next().await {
                    Some(Ok(d)) => value(d),
                    Some(Err(err)) => Err(OpError::Driver(err)),
                    None => Err(OpError::Runner("change stream ended".into())),
                },
                "iterateOnce" => match cs.try_next(Duration::ZERO).await {
                    Ok(Some(d)) => value(d),
                    Ok(None) => none(),
                    Err(err) => Err(OpError::Driver(err)),
                },
                "close" => return none(),
                other => Err(OpError::Unsupported(format!("change stream operation {other}"))),
            };
            e.insert(object, Entity::ChangeStream(cs)).map_err(OpError::Runner)?;
            r
        }
        Some(Entity::Cursor(mut c)) => {
            let r = match name {
                "iterateUntilDocumentOrError" | "iterateOnce" => match c.next_document().await {
                    Some(Ok(d)) => value(d),
                    Some(Err(err)) => Err(OpError::Driver(err)),
                    None if name == "iterateOnce" => none(),
                    None => Err(OpError::Runner("cursor exhausted".into())),
                },
                "close" => {
                    c.close().await?;
                    return none();
                }
                other => Err(OpError::Unsupported(format!("cursor operation {other}"))),
            };
            e.insert(object, Entity::Cursor(c)).map_err(OpError::Runner)?;
            r
        }
        Some(other) => {
            e.insert(object, other).map_err(OpError::Runner)?;
            Err(OpError::Runner(format!("{object} is not a cursor")))
        }
        None => Err(OpError::Runner(format!("no cursor {object}"))),
    }
}

/// A client BulkWriteResult as the bulk-write spec's tests write it.
pub fn client_bulk_doc(r: &kernway_orm_mongodb::ClientBulkWriteResult) -> Document {
    let mut d = Document::new()
        .with("insertedCount", r.inserted)
        .with("upsertedCount", r.upserted)
        .with("matchedCount", r.matched)
        .with("modifiedCount", r.modified)
        .with("deletedCount", r.deleted);
    if let Some(v) = &r.verbose {
        let ins: Document = v.insert_results.iter().map(|(i, id)| (i.to_string(), Bson::Document(Document::new().with("insertedId", id.clone())))).collect();
        let upd: Document = v
            .update_results
            .iter()
            .map(|(i, u)| {
                let mut x = Document::new().with("matchedCount", u.matched).with("modifiedCount", u.modified);
                if let Some(id) = &u.upserted_id {
                    x.insert("upsertedId", id.clone());
                }
                (i.to_string(), Bson::Document(x))
            })
            .collect();
        let del: Document = v.delete_results.iter().map(|(i, n)| (i.to_string(), Bson::Document(Document::new().with("deletedCount", *n)))).collect();
        d.insert("insertResults", ins);
        d.insert("updateResults", upd);
        d.insert("deleteResults", del);
    }
    d
}

/// A BulkWriteResult as the CRUD spec's tests write it.
pub fn bulk_result_doc(r: &kernway_orm_mongodb::BulkWriteResult) -> Document {
    let upserted: Document = r.upserted.iter().map(|(i, id)| (i.to_string(), id.clone())).collect();
    let inserted: Document = r.inserted_ids.iter().map(|(i, id)| (i.to_string(), id.clone())).collect();
    Document::new()
        .with("deletedCount", r.deleted as i64)
        .with("insertedCount", r.inserted as i64)
        .with("matchedCount", r.matched as i64)
        .with("modifiedCount", r.modified as i64)
        .with("upsertedCount", r.upserted.len() as i64)
        .with("insertedIds", inserted)
        .with("upsertedIds", upserted)
}

/// Whether `e` came from the server (a command or write error).
pub fn is_server_error(e: &Error) -> bool {
    matches!(e.kind, ErrorKind::Command { .. } | ErrorKind::Write { .. })
}
