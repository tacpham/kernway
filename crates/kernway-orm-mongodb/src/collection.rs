//! Databases and collections: CRUD, cursors, aggregation, bulk writes,
//! indexes and change streams. Writes are split into batches the server
//! accepts (`maxWriteBatchSize`, message size) and sent as `OP_MSG`
//! document sequences.

use crate::bson::{to_document, Bson, Document};
use crate::client::{Client, Cursor, Op, Ran, Retry, Session};
use crate::doc;
use crate::error::{Error, ErrorKind, Result};
use crate::options::ReadPreference;
use crate::topology::Selection;
use crate::wire::Sequence;
use serde::{de::DeserializeOwned, Serialize};
use std::marker::PhantomData;
use std::time::Duration;

/// A database.
#[derive(Clone)]
pub struct Database {
    pub(crate) client: Client,
    pub(crate) name: String,
    pub(crate) session: Option<Session>,
}

/// A collection of `T` (default: raw [`Document`]s).
pub struct Collection<T = Document> {
    pub(crate) client: Client,
    pub(crate) db: String,
    pub(crate) name: String,
    pub(crate) session: Option<Session>,
    pub(crate) read_preference: Option<ReadPreference>,
    _t: PhantomData<T>,
}

impl<T> Clone for Collection<T> {
    fn clone(&self) -> Self {
        Collection {
            client: self.client.clone(),
            db: self.db.clone(),
            name: self.name.clone(),
            session: self.session.clone(),
            read_preference: self.read_preference.clone(),
            _t: PhantomData,
        }
    }
}

impl Client {
    /// A database.
    pub fn database(&self, name: &str) -> Database {
        Database { client: self.clone(), name: name.to_string(), session: None }
    }

    /// The default database of the connection string.
    pub fn default_database(&self) -> Option<Database> {
        self.options().database().map(|d| self.database(d))
    }
}

impl Session {
    /// A database whose operations run in this session.
    pub fn database(&self, name: &str) -> Database {
        Database { client: self.client.clone(), name: name.to_string(), session: Some(self.clone()) }
    }
}

impl Database {
    /// Its name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A collection of `T`.
    pub fn collection<T>(&self, name: &str) -> Collection<T> {
        Collection { client: self.client.clone(), db: self.name.clone(), name: name.to_string(), session: self.session.clone(), read_preference: None, _t: PhantomData }
    }

    async fn run(&self, command: Document, selection: Selection, retry: Retry, rc: bool, wc: bool) -> Result<Ran> {
        let op = Op { db: &self.name, command, seqs: &[], selection, retry, read_concern: rc, write_concern: wc, session: self.session.as_ref() };
        self.client.execute(op).await
    }

    /// Run any command on the primary.
    pub async fn run_command(&self, command: Document) -> Result<Document> {
        Ok(self.run(command, Selection::Write, Retry::No, false, false).await?.reply)
    }

    /// Run a command on a server picked by `read`.
    pub async fn run_command_with(&self, command: Document, read: &ReadPreference) -> Result<Document> {
        Ok(self.run(command, Selection::Read(read.clone()), Retry::No, false, false).await?.reply)
    }

    /// Collection names (views included).
    pub async fn list_collection_names(&self) -> Result<Vec<String>> {
        let ran = self.run(doc! { "listCollections" => 1, "nameOnly" => true, "authorizedCollections" => true }, Selection::Write, Retry::Read, false, false).await?;
        let cur: Cursor<Document> = Cursor::from_reply(&self.client, ran, self.session.as_ref(), None)?;
        Ok(cur.collect().await?.iter().filter_map(|d| d.get_str("name").map(String::from)).collect())
    }

    /// Collection descriptions (`listCollections`), optionally filtered.
    pub async fn list_collections(&self, filter: Document) -> Result<Vec<Document>> {
        let ran = self.run(doc! { "listCollections" => 1, "filter" => filter }, Selection::Write, Retry::Read, false, false).await?;
        Cursor::<Document>::from_reply(&self.client, ran, self.session.as_ref(), None)?.collect().await
    }

    /// Create a collection; `options` as the `create` command takes them
    /// (`capped`, `size`, `validator`, `timeseries`, `clusteredIndex`, …).
    pub async fn create_collection(&self, name: &str, options: Document) -> Result<()> {
        let mut cmd = doc! { "create" => name };
        for (k, v) in options {
            cmd.insert(k, v);
        }
        self.run(cmd, Selection::Write, Retry::No, false, true).await.map(|_| ())
    }

    /// Drop the database.
    pub async fn drop(&self) -> Result<()> {
        self.run(doc! { "dropDatabase" => 1 }, Selection::Write, Retry::No, false, true).await.map(|_| ())
    }

    /// Run an aggregation on the database (`$currentOp`, `$documents`, …).
    pub async fn aggregate(&self, pipeline: Vec<Document>) -> Result<Cursor<Document>> {
        let cmd = doc! { "aggregate" => 1, "pipeline" => pipeline.into_iter().map(Bson::Document).collect::<Vec<_>>(), "cursor" => Document::new() };
        let ran = self.run(cmd, Selection::Write, Retry::Read, true, false).await?;
        Cursor::from_reply(&self.client, ran, self.session.as_ref(), None)
    }
}

/// `find` options.
#[derive(Debug, Clone, Default)]
pub struct FindOptions {
    /// Sort document.
    pub sort: Option<Document>,
    /// Projection.
    pub projection: Option<Document>,
    /// Documents to skip.
    pub skip: Option<u64>,
    /// At most this many (negative: one batch only).
    pub limit: Option<i64>,
    /// Batch size.
    pub batch_size: Option<u32>,
    /// Index hint (name or key document).
    pub hint: Option<Bson>,
    /// Collation.
    pub collation: Option<Document>,
    /// `maxTimeMS`.
    pub max_time: Option<Duration>,
    /// Comment (shows in the profiler / logs).
    pub comment: Option<Bson>,
    /// Allow spilling to disk.
    pub allow_disk_use: Option<bool>,
    /// Read preference for this find.
    pub read_preference: Option<ReadPreference>,
}

/// Update options.
#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    /// Insert when nothing matches.
    pub upsert: bool,
    /// `arrayFilters`.
    pub array_filters: Option<Vec<Document>>,
    /// Collation.
    pub collation: Option<Document>,
    /// Index hint.
    pub hint: Option<Bson>,
}

/// `findAndModify` options.
#[derive(Debug, Clone, Default)]
pub struct FindOneAndModifyOptions {
    /// Which matching document (first in this order).
    pub sort: Option<Document>,
    /// Projection of the returned document.
    pub projection: Option<Document>,
    /// Insert when nothing matches (update / replace).
    pub upsert: bool,
    /// Return the document after the change (default: before).
    pub return_after: bool,
    /// `arrayFilters`.
    pub array_filters: Option<Vec<Document>>,
    /// Collation.
    pub collation: Option<Document>,
}

/// Result of an insert.
#[derive(Debug, Clone, PartialEq)]
pub struct InsertManyResult {
    /// The `_id`s, by input position.
    pub inserted_ids: Vec<Bson>,
}

/// Result of an update / replace.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UpdateResult {
    /// Documents matched.
    pub matched: u64,
    /// Documents changed.
    pub modified: u64,
    /// The `_id` inserted by an upsert.
    pub upserted_id: Option<Bson>,
}

/// One write of a bulk write.
#[derive(Debug, Clone)]
pub enum WriteModel {
    /// Insert a document (an `_id` is added when missing).
    InsertOne(Document),
    /// Update the first match.
    UpdateOne {
        /// Which documents.
        filter: Document,
        /// Update operators, or a pipeline array.
        update: Bson,
        /// Upsert, array filters, collation, hint.
        options: UpdateOptions,
    },
    /// Update every match.
    UpdateMany {
        /// Which documents.
        filter: Document,
        /// Update operators, or a pipeline array.
        update: Bson,
        /// Upsert, array filters, collation, hint.
        options: UpdateOptions,
    },
    /// Replace the first match.
    ReplaceOne {
        /// Which document.
        filter: Document,
        /// The new document (no operators).
        replacement: Document,
        /// Upsert, collation, hint.
        options: UpdateOptions,
    },
    /// Delete the first match.
    DeleteOne {
        /// Which document.
        filter: Document,
    },
    /// Delete every match.
    DeleteMany {
        /// Which documents.
        filter: Document,
    },
}

/// Result of a bulk write.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BulkWriteResult {
    /// Documents inserted.
    pub inserted: u64,
    /// Documents matched by updates.
    pub matched: u64,
    /// Documents changed by updates.
    pub modified: u64,
    /// Documents deleted.
    pub deleted: u64,
    /// Upserts: (model index, `_id`).
    pub upserted: Vec<(usize, Bson)>,
    /// Inserted `_id`s: (model index, `_id`).
    pub inserted_ids: Vec<(usize, Bson)>,
}

/// An index to create.
#[derive(Debug, Clone, Default)]
pub struct IndexModel {
    /// Key document (`{email: 1}`, `{loc: "2dsphere"}`, `{"$**": "text"}`).
    pub keys: Document,
    /// Name (default: derived from the keys).
    pub name: Option<String>,
    /// Unique.
    pub unique: bool,
    /// Sparse.
    pub sparse: bool,
    /// TTL.
    pub expire_after: Option<Duration>,
    /// Partial index filter.
    pub partial_filter: Option<Document>,
    /// Any other index option.
    pub options: Document,
}

/// Change stream options.
#[derive(Debug, Clone, Default)]
pub struct ChangeStreamOptions {
    /// `fullDocument`: `updateLookup`, `whenAvailable`, `required`.
    pub full_document: Option<String>,
    /// `fullDocumentBeforeChange`.
    pub full_document_before_change: Option<String>,
    /// Resume after this token.
    pub resume_after: Option<Bson>,
    /// Start after this token (also after an invalidate).
    pub start_after: Option<Bson>,
    /// Start at this cluster time.
    pub start_at_operation_time: Option<crate::bson::Timestamp>,
    /// How long a `getMore` waits for changes (default: the server's 1 s).
    pub max_await: Option<Duration>,
    /// Batch size.
    pub batch_size: Option<u32>,
}

/// Documents larger than this many bytes get their own batch.
const BATCH_BYTES: usize = 47_000_000;

impl<T> Collection<T> {
    /// Its name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `db.collection`.
    pub fn namespace(&self) -> String {
        format!("{}.{}", self.db, self.name)
    }

    /// The same collection, read with `rp`.
    pub fn with_read_preference(&self, rp: ReadPreference) -> Self {
        let mut c = self.clone();
        c.read_preference = Some(rp);
        c
    }

    /// The same collection with another document type.
    pub fn clone_with_type<U>(&self) -> Collection<U> {
        Collection {
            client: self.client.clone(),
            db: self.db.clone(),
            name: self.name.clone(),
            session: self.session.clone(),
            read_preference: self.read_preference.clone(),
            _t: PhantomData,
        }
    }

    /// The same collection, its operations in `session`.
    pub fn in_session(&self, session: &Session) -> Self {
        let mut c = self.clone();
        c.session = Some(session.clone());
        c
    }

    fn read_selection(&self, rp: Option<&ReadPreference>) -> Selection {
        let in_txn = self.session.as_ref().is_some_and(Session::in_transaction);
        match rp.or(self.read_preference.as_ref()) {
            Some(rp) if !in_txn => Selection::Read(rp.clone()),
            _ if in_txn => Selection::Write,
            _ => Selection::Read(self.client.options().read_preference.clone()),
        }
    }

    async fn exec(&self, command: Document, seqs: &[Sequence<'_>], selection: Selection, retry: Retry, rc: bool, wc: bool) -> Result<Ran> {
        let op = Op { db: &self.db, command, seqs, selection, retry, read_concern: rc, write_concern: wc, session: self.session.as_ref() };
        self.client.execute(op).await
    }

    async fn write(&self, command: Document, seqs: &[Sequence<'_>], retryable: bool) -> Result<Document> {
        Ok(self.exec(command, seqs, Selection::Write, if retryable { Retry::Write } else { Retry::No }, false, true).await?.reply)
    }

    /// Insert raw documents (an `_id` is added where missing), ordered or not.
    pub async fn insert_many_documents(&self, mut docs: Vec<Document>, ordered: bool) -> Result<InsertManyResult> {
        let mut ids = Vec::with_capacity(docs.len());
        for d in &mut docs {
            if !d.contains_key("_id") {
                // _id first, as the server stores it.
                let mut with_id = Document::new().with("_id", crate::bson::ObjectId::new());
                for (k, v) in std::mem::take(d) {
                    with_id.insert(k, v);
                }
                *d = with_id;
            }
            ids.push(d.get("_id").cloned().unwrap_or(Bson::Null));
        }
        let (max_count, max_bson) = self.limits().await?;
        let mut start = 0;
        let mut failures: Vec<(usize, i32, String)> = Vec::new();
        while start < docs.len() {
            let mut end = start;
            let mut bytes = 0;
            while end < docs.len() && end - start < max_count {
                let size = docs[end].to_bytes()?.len();
                if size > max_bson {
                    return Err(Error::new(ErrorKind::Command { code: 10334, code_name: "BSONObjectTooLarge".into() }, format!("document {end} is {size} bytes, above {max_bson}")));
                }
                if end > start && bytes + size > BATCH_BYTES {
                    break;
                }
                bytes += size;
                end += 1;
            }
            let cmd = doc! { "insert" => self.name.clone(), "ordered" => ordered };
            let r = self.write(cmd, &[Sequence { identifier: "documents", documents: &docs[start..end] }], true).await;
            match r {
                Ok(_) => {}
                Err(Error { kind: ErrorKind::Write { write_errors, write_concern }, message, labels }) => {
                    let shifted: Vec<(usize, i32, String)> = write_errors.into_iter().map(|(i, c, m)| (i + start, c, m)).collect();
                    if ordered || write_concern.is_some() {
                        let mut all = failures;
                        all.extend(shifted);
                        return Err(Error { kind: ErrorKind::Write { write_errors: all, write_concern }, message, labels });
                    }
                    failures.extend(shifted);
                }
                Err(e) => return Err(e),
            }
            start = end;
        }
        if !failures.is_empty() {
            let message = failures[0].2.clone();
            return Err(Error::new(ErrorKind::Write { write_errors: failures, write_concern: None }, message));
        }
        Ok(InsertManyResult { inserted_ids: ids })
    }

    async fn limits(&self) -> Result<(usize, usize)> {
        // From any connection's hello (servers of one deployment agree).
        let addr = self.client.select(&Selection::Write).await?;
        let c = self.client.connection(&addr).await?;
        Ok((c.hello().max_write_batch(), c.hello().max_bson_size()))
    }

    /// Update with raw filter / update documents.
    async fn update_raw(&self, filter: Document, update: Bson, multi: bool, o: &UpdateOptions) -> Result<UpdateResult> {
        let mut u = doc! { "q" => filter, "u" => update, "multi" => multi, "upsert" => o.upsert };
        if let Some(af) = &o.array_filters {
            u.insert("arrayFilters", af.iter().cloned().map(Bson::Document).collect::<Vec<_>>());
        }
        if let Some(c) = &o.collation {
            u.insert("collation", c.clone());
        }
        if let Some(h) = &o.hint {
            u.insert("hint", h.clone());
        }
        let r = self.write(doc! { "update" => self.name.clone(), "ordered" => true }, &[Sequence { identifier: "updates", documents: &[u] }], !multi).await?;
        Ok(UpdateResult {
            matched: r.get_i64("n").unwrap_or(0) as u64 - r.get_array("upserted").map_or(0, |a| a.len() as u64),
            modified: r.get_i64("nModified").unwrap_or(0) as u64,
            upserted_id: r.get_array("upserted").and_then(|a| a.first()).and_then(Bson::as_document).and_then(|d| d.get("_id").cloned()),
        })
    }

    /// Update the first document matching `filter` (`update`: an update
    /// document with operators, or a pipeline array).
    pub async fn update_one(&self, filter: Document, update: impl Into<Bson>, options: UpdateOptions) -> Result<UpdateResult> {
        self.update_raw(filter, update.into(), false, &options).await
    }

    /// Update every matching document.
    pub async fn update_many(&self, filter: Document, update: impl Into<Bson>, options: UpdateOptions) -> Result<UpdateResult> {
        self.update_raw(filter, update.into(), true, &options).await
    }

    /// Delete the first match (`limit` 1) or every match (0).
    async fn delete_raw(&self, filter: Document, limit: i32) -> Result<u64> {
        let d = doc! { "q" => filter, "limit" => limit };
        let r = self.write(doc! { "delete" => self.name.clone(), "ordered" => true }, &[Sequence { identifier: "deletes", documents: &[d] }], limit == 1).await?;
        Ok(r.get_i64("n").unwrap_or(0) as u64)
    }

    /// Delete the first document matching `filter`; returns how many (0 or 1).
    pub async fn delete_one(&self, filter: Document) -> Result<u64> {
        self.delete_raw(filter, 1).await
    }

    /// Delete every matching document.
    pub async fn delete_many(&self, filter: Document) -> Result<u64> {
        self.delete_raw(filter, 0).await
    }

    /// Count matching documents (an aggregation: exact, honours a transaction).
    pub async fn count_documents(&self, filter: Document) -> Result<u64> {
        let pipeline = vec![doc! { "$match" => filter }, doc! { "$group" => doc! { "_id" => 1, "n" => doc! { "$sum" => 1 } } }];
        let mut cur = self.aggregate(pipeline).await?;
        Ok(match cur.next_document().await.transpose()? {
            Some(d) => d.get_i64("n").unwrap_or(0) as u64,
            None => 0,
        })
    }

    /// The count from collection metadata (fast, approximate after crashes).
    pub async fn estimated_document_count(&self) -> Result<u64> {
        let ran = self.exec(doc! { "count" => self.name.clone() }, &[], self.read_selection(None), Retry::Read, true, false).await?;
        Ok(ran.reply.get_i64("n").unwrap_or(0) as u64)
    }

    /// Distinct values of `field` among matching documents.
    pub async fn distinct(&self, field: &str, filter: Document) -> Result<Vec<Bson>> {
        let ran = self.exec(doc! { "distinct" => self.name.clone(), "key" => field, "query" => filter }, &[], self.read_selection(None), Retry::Read, true, false).await?;
        Ok(ran.reply.get_array("values").cloned().unwrap_or_default())
    }

    /// Run an aggregation pipeline (`$out` / `$merge` run on the primary).
    pub async fn aggregate(&self, pipeline: Vec<Document>) -> Result<Cursor<Document>> {
        let writes = pipeline.last().and_then(Document::first_key).is_some_and(|k| k == "$out" || k == "$merge");
        let cmd = doc! { "aggregate" => self.name.clone(), "pipeline" => pipeline.into_iter().map(Bson::Document).collect::<Vec<_>>(), "cursor" => Document::new() };
        let (sel, retry) = if writes { (Selection::Write, Retry::No) } else { (self.read_selection(None), Retry::Read) };
        let ran = self.exec(cmd, &[], sel, retry, true, writes).await?;
        Cursor::from_reply(&self.client, ran, self.session.as_ref(), None)
    }

    /// Create indexes; returns their names.
    pub async fn create_indexes(&self, models: Vec<IndexModel>) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let mut specs = Vec::new();
        for m in models {
            let name = m.name.clone().unwrap_or_else(|| {
                m.keys.iter().map(|(k, v)| format!("{k}_{}", v.as_i64().map_or_else(|| v.as_str().unwrap_or("x").to_string(), |i| i.to_string()))).collect::<Vec<_>>().join("_")
            });
            let mut spec = doc! { "key" => m.keys.clone(), "name" => name.clone() };
            if m.unique {
                spec.insert("unique", true);
            }
            if m.sparse {
                spec.insert("sparse", true);
            }
            if let Some(t) = m.expire_after {
                spec.insert("expireAfterSeconds", t.as_secs() as i64);
            }
            if let Some(p) = m.partial_filter {
                spec.insert("partialFilterExpression", p);
            }
            for (k, v) in m.options {
                spec.insert(k, v);
            }
            names.push(name);
            specs.push(Bson::Document(spec));
        }
        self.write(doc! { "createIndexes" => self.name.clone(), "indexes" => specs }, &[], false).await?;
        Ok(names)
    }

    /// Create one index; returns its name.
    pub async fn create_index(&self, model: IndexModel) -> Result<String> {
        Ok(self.create_indexes(vec![model]).await?.remove(0))
    }

    /// The indexes.
    pub async fn list_indexes(&self) -> Result<Vec<Document>> {
        let ran = self.exec(doc! { "listIndexes" => self.name.clone() }, &[], Selection::Write, Retry::Read, false, false).await?;
        Cursor::<Document>::from_reply(&self.client, ran, self.session.as_ref(), None)?.collect().await
    }

    /// Drop the index named `name` (`"*"`: all but `_id`).
    pub async fn drop_index(&self, name: &str) -> Result<()> {
        self.write(doc! { "dropIndexes" => self.name.clone(), "index" => name }, &[], false).await.map(|_| ())
    }

    /// Drop the collection (a missing one is fine).
    pub async fn drop(&self) -> Result<()> {
        match self.write(doc! { "drop" => self.name.clone() }, &[], false).await {
            Ok(_) => Ok(()),
            Err(e) if e.code() == Some(26) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Run `models` as bulk writes: consecutive writes of one kind share a
    /// command. Ordered: stops at the first error.
    pub async fn bulk_write(&self, models: Vec<WriteModel>, ordered: bool) -> Result<BulkWriteResult> {
        let mut out = BulkWriteResult::default();
        let mut i = 0;
        while i < models.len() {
            let group_kind = |m: &WriteModel| match m {
                WriteModel::InsertOne(_) => 0,
                WriteModel::UpdateOne { .. } | WriteModel::UpdateMany { .. } | WriteModel::ReplaceOne { .. } => 1,
                WriteModel::DeleteOne { .. } | WriteModel::DeleteMany { .. } => 2,
            };
            let g = group_kind(&models[i]);
            let mut j = i;
            while j < models.len() && group_kind(&models[j]) == g {
                j += 1;
            }
            let group = &models[i..j];
            let r = match g {
                0 => {
                    let docs: Vec<Document> = group.iter().map(|m| if let WriteModel::InsertOne(d) = m { d.clone() } else { Document::new() }).collect();
                    let r = self.insert_many_documents(docs, ordered).await;
                    r.map(|ins| {
                        out.inserted += ins.inserted_ids.len() as u64;
                        out.inserted_ids.extend(ins.inserted_ids.into_iter().enumerate().map(|(k, id)| (i + k, id)));
                    })
                }
                1 => {
                    let updates: Vec<Document> = group
                        .iter()
                        .map(|m| {
                            let (q, u, multi, o) = match m {
                                WriteModel::UpdateOne { filter, update, options } => (filter.clone(), update.clone(), false, options),
                                WriteModel::UpdateMany { filter, update, options } => (filter.clone(), update.clone(), true, options),
                                WriteModel::ReplaceOne { filter, replacement, options } => (filter.clone(), Bson::Document(replacement.clone()), false, options),
                                _ => unreachable!("grouped by kind"),
                            };
                            let mut d = doc! { "q" => q, "u" => u, "multi" => multi, "upsert" => o.upsert };
                            if let Some(af) = &o.array_filters {
                                d.insert("arrayFilters", af.iter().cloned().map(Bson::Document).collect::<Vec<_>>());
                            }
                            if let Some(c) = &o.collation {
                                d.insert("collation", c.clone());
                            }
                            d
                        })
                        .collect();
                    let retry = !group.iter().any(|m| matches!(m, WriteModel::UpdateMany { .. }));
                    self.write(doc! { "update" => self.name.clone(), "ordered" => ordered }, &[Sequence { identifier: "updates", documents: &updates }], retry)
                        .await
                        .map(|r| {
                            let ups: Vec<(usize, Bson)> = r
                                .get_array("upserted")
                                .map(|a| {
                                    a.iter()
                                        .filter_map(Bson::as_document)
                                        .map(|u| (i + u.get_i64("index").unwrap_or(0) as usize, u.get("_id").cloned().unwrap_or(Bson::Null)))
                                        .collect()
                                })
                                .unwrap_or_default();
                            out.matched += r.get_i64("n").unwrap_or(0) as u64 - ups.len() as u64;
                            out.modified += r.get_i64("nModified").unwrap_or(0) as u64;
                            out.upserted.extend(ups);
                        })
                }
                _ => {
                    let deletes: Vec<Document> = group
                        .iter()
                        .map(|m| match m {
                            WriteModel::DeleteOne { filter } => doc! { "q" => filter.clone(), "limit" => 1 },
                            WriteModel::DeleteMany { filter } => doc! { "q" => filter.clone(), "limit" => 0 },
                            _ => unreachable!("grouped by kind"),
                        })
                        .collect();
                    let retry = !group.iter().any(|m| matches!(m, WriteModel::DeleteMany { .. }));
                    self.write(doc! { "delete" => self.name.clone(), "ordered" => ordered }, &[Sequence { identifier: "deletes", documents: &deletes }], retry)
                        .await
                        .map(|r| out.deleted += r.get_i64("n").unwrap_or(0) as u64)
                }
            };
            if let Err(mut e) = r {
                // Indexes within the group → indexes of the models.
                if let ErrorKind::Write { write_errors, .. } = &mut e.kind {
                    for w in write_errors.iter_mut() {
                        w.0 += i;
                    }
                }
                if ordered || !matches!(e.kind, ErrorKind::Write { .. }) {
                    return Err(e);
                }
            }
            i = j;
        }
        Ok(out)
    }

    /// Watch changes to this collection.
    pub async fn watch(&self, pipeline: Vec<Document>, options: ChangeStreamOptions) -> Result<ChangeStream> {
        ChangeStream::open(self.client.clone(), self.db.clone(), Some(self.name.clone()), pipeline, options, self.session.clone()).await
    }
}

impl<T: Serialize + DeserializeOwned> Collection<T> {
    /// Insert one; returns its `_id` (added when `T` has none).
    pub async fn insert_one(&self, item: &T) -> Result<Bson> {
        let d = to_document(item)?;
        Ok(self.insert_many_documents(vec![d], true).await?.inserted_ids.remove(0))
    }

    /// Insert many (ordered: stop at the first failure).
    pub async fn insert_many(&self, items: &[T], ordered: bool) -> Result<InsertManyResult> {
        let docs = items.iter().map(to_document).collect::<std::result::Result<Vec<_>, _>>()?;
        self.insert_many_documents(docs, ordered).await
    }

    /// Replace the first match with `item`.
    pub async fn replace_one(&self, filter: Document, item: &T, options: UpdateOptions) -> Result<UpdateResult> {
        let d = to_document(item)?;
        if d.keys().any(|k| k.starts_with('$')) {
            return Err(Error::config("a replacement document has no update operators"));
        }
        self.update_raw(filter, Bson::Document(d), false, &options).await
    }

    /// Find matching documents.
    pub async fn find(&self, filter: Document, o: FindOptions) -> Result<Cursor<T>> {
        let mut cmd = doc! { "find" => self.name.clone(), "filter" => filter };
        if let Some(s) = &o.sort {
            cmd.insert("sort", s.clone());
        }
        if let Some(p) = &o.projection {
            cmd.insert("projection", p.clone());
        }
        if let Some(s) = o.skip {
            cmd.insert("skip", s as i64);
        }
        match o.limit {
            Some(l) if l < 0 => {
                cmd.insert("limit", -l);
                cmd.insert("singleBatch", true);
            }
            Some(l) if l > 0 => {
                cmd.insert("limit", l);
            }
            _ => {}
        }
        if let Some(b) = o.batch_size {
            cmd.insert("batchSize", i64::from(b));
        }
        if let Some(h) = &o.hint {
            cmd.insert("hint", h.clone());
        }
        if let Some(c) = &o.collation {
            cmd.insert("collation", c.clone());
        }
        if let Some(t) = o.max_time {
            cmd.insert("maxTimeMS", t.as_millis() as i64);
        }
        if let Some(c) = &o.comment {
            cmd.insert("comment", c.clone());
        }
        if let Some(a) = o.allow_disk_use {
            cmd.insert("allowDiskUse", a);
        }
        let ran = self.exec(cmd, &[], self.read_selection(o.read_preference.as_ref()), Retry::Read, true, false).await?;
        Cursor::from_reply(&self.client, ran, self.session.as_ref(), o.batch_size.map(i64::from))
    }

    /// The first match.
    pub async fn find_one(&self, filter: Document, mut o: FindOptions) -> Result<Option<T>> {
        o.limit = Some(-1);
        let mut c = self.find(filter, o).await?;
        c.next().await.transpose()
    }

    async fn find_and_modify(&self, filter: Document, action: Document, o: &FindOneAndModifyOptions) -> Result<Option<T>> {
        let mut cmd = doc! { "findAndModify" => self.name.clone(), "query" => filter };
        for (k, v) in action {
            cmd.insert(k, v);
        }
        if let Some(s) = &o.sort {
            cmd.insert("sort", s.clone());
        }
        if let Some(p) = &o.projection {
            cmd.insert("fields", p.clone());
        }
        if let Some(af) = &o.array_filters {
            cmd.insert("arrayFilters", af.iter().cloned().map(Bson::Document).collect::<Vec<_>>());
        }
        if let Some(c) = &o.collation {
            cmd.insert("collation", c.clone());
        }
        let r = self.write(cmd, &[], true).await?;
        match r.get("value") {
            Some(Bson::Document(d)) => Ok(Some(crate::bson::from_document(d)?)),
            _ => Ok(None),
        }
    }

    /// Atomically update the first match and return it (before, or after
    /// with `return_after`).
    pub async fn find_one_and_update(&self, filter: Document, update: impl Into<Bson>, o: FindOneAndModifyOptions) -> Result<Option<T>> {
        let action = doc! { "update" => update.into(), "new" => o.return_after, "upsert" => o.upsert };
        self.find_and_modify(filter, action, &o).await
    }

    /// Atomically replace the first match and return it.
    pub async fn find_one_and_replace(&self, filter: Document, item: &T, o: FindOneAndModifyOptions) -> Result<Option<T>> {
        let action = doc! { "update" => to_document(item)?, "new" => o.return_after, "upsert" => o.upsert };
        self.find_and_modify(filter, action, &o).await
    }

    /// Atomically delete the first match and return it.
    pub async fn find_one_and_delete(&self, filter: Document, o: FindOneAndModifyOptions) -> Result<Option<T>> {
        self.find_and_modify(filter, doc! { "remove" => true }, &o).await
    }
}

/// A change stream: a cursor over a `$changeStream` aggregation that
/// resumes itself (once per error) after network errors, elections and
/// other resumable failures, from the last token it saw.
pub struct ChangeStream {
    client: Client,
    db: String,
    collection: Option<String>,
    pipeline: Vec<Document>,
    options: ChangeStreamOptions,
    session: Option<Session>,
    cursor: Cursor<Document>,
    resume_token: Option<Bson>,
}

impl ChangeStream {
    async fn open(
        client: Client,
        db: String,
        collection: Option<String>,
        pipeline: Vec<Document>,
        options: ChangeStreamOptions,
        session: Option<Session>,
    ) -> Result<Self> {
        let cursor = Self::aggregate(&client, &db, collection.as_deref(), &pipeline, &options, session.as_ref(), None).await?;
        let resume_token = options.start_after.clone().or(options.resume_after.clone()).or(cursor.post_batch_token.clone());
        Ok(ChangeStream { client, db, collection, pipeline, options, session, cursor, resume_token })
    }

    async fn aggregate(
        client: &Client,
        db: &str,
        collection: Option<&str>,
        pipeline: &[Document],
        o: &ChangeStreamOptions,
        session: Option<&Session>,
        resume: Option<&Bson>,
    ) -> Result<Cursor<Document>> {
        let mut stage = Document::new();
        if let Some(f) = &o.full_document {
            stage.insert("fullDocument", f.clone());
        }
        if let Some(f) = &o.full_document_before_change {
            stage.insert("fullDocumentBeforeChange", f.clone());
        }
        match resume {
            Some(t) => {
                stage.insert(if o.start_after.is_some() && resume == o.start_after.as_ref() { "startAfter" } else { "resumeAfter" }, t.clone());
            }
            None => {
                if let Some(t) = &o.start_after {
                    stage.insert("startAfter", t.clone());
                } else if let Some(t) = &o.resume_after {
                    stage.insert("resumeAfter", t.clone());
                } else if let Some(t) = o.start_at_operation_time {
                    stage.insert("startAtOperationTime", t);
                }
            }
        }
        if collection.is_none() && db == "admin" {
            stage.insert("allChangesForCluster", true);
        }
        let mut full = vec![Bson::Document(doc! { "$changeStream" => stage })];
        full.extend(pipeline.iter().cloned().map(Bson::Document));
        let mut cursor_opts = Document::new();
        if let Some(b) = o.batch_size {
            cursor_opts.insert("batchSize", i64::from(b));
        }
        let target: Bson = collection.map_or(Bson::Int32(1), Bson::from);
        let cmd = doc! { "aggregate" => target, "pipeline" => full, "cursor" => cursor_opts };
        let op = Op { db, command: cmd, seqs: &[], selection: Selection::Read(client.options().read_preference.clone()), retry: Retry::Read, read_concern: true, write_concern: false, session };
        let ran = client.execute(op).await?;
        let mut cur = Cursor::from_reply(client, ran, session, o.batch_size.map(i64::from))?;
        cur.max_await = o.max_await;
        Ok(cur)
    }

    /// The token to resume after the last change returned.
    pub fn resume_token(&self) -> Option<&Bson> {
        self.resume_token.as_ref()
    }

    /// The next change, waiting for one (up to `max_await` per round trip;
    /// returns `None` only if the stream was invalidated).
    pub async fn next(&mut self) -> Option<Result<Document>> {
        let mut resumed = false;
        loop {
            if let Some(d) = self.cursor.buffer.pop_front() {
                self.resume_token = d.get("_id").cloned();
                if self.cursor.buffer.is_empty() {
                    if let Some(t) = &self.cursor.post_batch_token {
                        self.resume_token = Some(t.clone());
                    }
                }
                if d.get_str("operationType") == Some("invalidate") {
                    self.cursor.id = 0;
                }
                return Some(Ok(d));
            }
            if self.cursor.id == 0 {
                return None;
            }
            match self.cursor.get_more().await {
                Ok(()) => {
                    if self.cursor.buffer.is_empty() {
                        if let Some(t) = &self.cursor.post_batch_token {
                            self.resume_token = Some(t.clone());
                        }
                    }
                }
                Err(e) if !resumed && resumable(&e) => {
                    resumed = true;
                    match Self::aggregate(&self.client, &self.db, self.collection.as_deref(), &self.pipeline, &self.options, self.session.as_ref(), self.resume_token.as_ref()).await {
                        Ok(c) => self.cursor = c,
                        Err(e) => return Some(Err(e)),
                    }
                }
                Err(e) => return Some(Err(e)),
            }
        }
    }

    /// Wait for a change for up to `wait`; `Ok(None)` when none came.
    pub async fn try_next(&mut self, wait: Duration) -> Result<Option<Document>> {
        match rt_core::timeout(wait, self.next()).await {
            Ok(Some(r)) => r.map(Some),
            Ok(None) => Ok(None),
            Err(_) => {
                // The getMore was abandoned mid-flight: its connection was
                // dropped, so resume on a fresh cursor.
                self.cursor.id = 0;
                let c = Self::aggregate(&self.client, &self.db, self.collection.as_deref(), &self.pipeline, &self.options, self.session.as_ref(), self.resume_token.as_ref()).await?;
                self.cursor = c;
                Ok(None)
            }
        }
    }
}

fn resumable(e: &Error) -> bool {
    if e.is_network() || e.has_label("ResumableChangeStreamError") || e.is_state_change() {
        return true;
    }
    matches!(e.code(), Some(6 | 7 | 89 | 91 | 189 | 262 | 9001 | 10107 | 11600 | 11602 | 13435 | 13436 | 63 | 150 | 13388 | 234 | 133 | 43))
}

impl Client {
    /// Watch every change in the deployment.
    pub async fn watch(&self, pipeline: Vec<Document>, options: ChangeStreamOptions) -> Result<ChangeStream> {
        ChangeStream::open(self.clone(), "admin".into(), None, pipeline, options, None).await
    }
}

impl Database {
    /// Watch every change in this database.
    pub async fn watch(&self, pipeline: Vec<Document>, options: ChangeStreamOptions) -> Result<ChangeStream> {
        ChangeStream::open(self.client.clone(), self.name.clone(), None, pipeline, options, self.session.clone()).await
    }
}
