//! `MongoClient.bulkWrite` (the CRUD bulk-write spec): writes to any number
//! of namespaces in one `bulkWrite` command per batch (MongoDB 8.0+), with
//! summary or verbose results and an exception that keeps everything that
//! happened.

use crate::bson::{Bson, Document, ObjectId};
use crate::client::{Client, Cursor, Op, Retry, Session};
use crate::collection::{DeleteOptions, UpdateOptions};
use crate::doc;
use crate::error::{Error, ErrorKind, Result};
use crate::options::WriteConcern;
use crate::topology::Selection;
use crate::wire::Sequence;
use std::collections::BTreeMap;

/// One write of a client bulk write; `namespace` is `"db.collection"`.
#[derive(Debug, Clone)]
pub enum ClientWriteModel {
    /// Insert a document (an `_id` is added first when missing).
    InsertOne {
        /// `db.collection`.
        namespace: String,
        /// The document.
        document: Document,
    },
    /// Update the first match.
    UpdateOne {
        /// `db.collection`.
        namespace: String,
        /// Which document.
        filter: Document,
        /// Update operators, or a pipeline array.
        update: Bson,
        /// Upsert, array filters, collation, hint, sort.
        options: UpdateOptions,
    },
    /// Update every match.
    UpdateMany {
        /// `db.collection`.
        namespace: String,
        /// Which documents.
        filter: Document,
        /// Update operators, or a pipeline array.
        update: Bson,
        /// Upsert, array filters, collation, hint.
        options: UpdateOptions,
    },
    /// Replace the first match.
    ReplaceOne {
        /// `db.collection`.
        namespace: String,
        /// Which document.
        filter: Document,
        /// The new document (no operators).
        replacement: Document,
        /// Upsert, collation, hint, sort.
        options: UpdateOptions,
    },
    /// Delete the first match.
    DeleteOne {
        /// `db.collection`.
        namespace: String,
        /// Which document.
        filter: Document,
        /// Collation, hint.
        options: DeleteOptions,
    },
    /// Delete every match.
    DeleteMany {
        /// `db.collection`.
        namespace: String,
        /// Which documents.
        filter: Document,
        /// Collation, hint.
        options: DeleteOptions,
    },
}

impl ClientWriteModel {
    fn namespace(&self) -> &str {
        match self {
            ClientWriteModel::InsertOne { namespace, .. }
            | ClientWriteModel::UpdateOne { namespace, .. }
            | ClientWriteModel::UpdateMany { namespace, .. }
            | ClientWriteModel::ReplaceOne { namespace, .. }
            | ClientWriteModel::DeleteOne { namespace, .. }
            | ClientWriteModel::DeleteMany { namespace, .. } => namespace,
        }
    }

    fn multi(&self) -> bool {
        matches!(self, ClientWriteModel::UpdateMany { .. } | ClientWriteModel::DeleteMany { .. })
    }
}

/// `MongoClient.bulkWrite` options.
#[derive(Debug, Clone, Default)]
pub struct ClientBulkWriteOptions {
    /// Stop at the first failure (default true).
    pub ordered: Option<bool>,
    /// Skip document validation.
    pub bypass_document_validation: Option<bool>,
    /// Comment.
    pub comment: Option<Bson>,
    /// Variables for `$$var` (`let`).
    pub let_vars: Option<Document>,
    /// Write concern (default: the client's).
    pub write_concern: Option<WriteConcern>,
    /// Report each write's result (default: counts only).
    pub verbose_results: Option<bool>,
    #[doc(hidden)]
    pub raw_data: Option<bool>,
}

/// One update's result.
#[derive(Debug, Clone, PartialEq)]
pub struct ClientUpdateResult {
    /// Documents matched.
    pub matched: i64,
    /// Documents modified.
    pub modified: i64,
    /// The upserted `_id`, if an upsert happened (`Some(Null)` is an upserted null id).
    pub upserted_id: Option<Bson>,
}

/// Each write's result (verbose results).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VerboseResults {
    /// Inserts: model index → inserted `_id`.
    pub insert_results: BTreeMap<usize, Bson>,
    /// Updates and replaces: model index → result.
    pub update_results: BTreeMap<usize, ClientUpdateResult>,
    /// Deletes: model index → documents deleted.
    pub delete_results: BTreeMap<usize, i64>,
}

/// The result of [`Client::bulk_write`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClientBulkWriteResult {
    /// Whether the server acknowledged the writes (false with `w: 0`:
    /// the counts are then meaningless).
    pub acknowledged: bool,
    /// Documents inserted.
    pub inserted: i64,
    /// Documents upserted.
    pub upserted: i64,
    /// Documents matched by updates.
    pub matched: i64,
    /// Documents modified by updates.
    pub modified: i64,
    /// Documents deleted.
    pub deleted: i64,
    /// Each write's result, when `verbose_results` was set.
    pub verbose: Option<VerboseResults>,
}

/// What a failed client bulk write reports (inside [`ErrorKind::ClientBulkWrite`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClientBulkWriteFailure {
    /// The error that stopped it (network, command, client-side), if any.
    pub error: Option<Error>,
    /// Write-concern errors, one per batch that had one: (code, message).
    pub write_concern_errors: Vec<(i32, String)>,
    /// Individual write errors: model index → (code, message, errInfo).
    pub write_errors: BTreeMap<usize, (i32, String, Option<Document>)>,
    /// What succeeded, when at least one write did.
    pub partial_result: Option<ClientBulkWriteResult>,
}

const MESSAGE_OVERHEAD_BYTES: usize = 1000;

fn put(d: &mut Document, k: &str, v: Option<impl Into<Bson>>) {
    if let Some(v) = v {
        d.insert(k, v.into());
    }
}

/// The `ops` entry of a model (`index`: its namespace in `nsInfo`).
fn op_doc(m: &ClientWriteModel, index: i32) -> Result<Document> {
    let update = |kind: &str, filter: &Document, mods: Bson, multi: bool, o: &UpdateOptions| {
        let mut d = doc! { kind => index, "filter" => filter.clone(), "updateMods" => mods, "multi" => multi };
        if o.upsert {
            d.insert("upsert", true);
        }
        if let Some(af) = &o.array_filters {
            d.insert("arrayFilters", af.iter().cloned().map(Bson::Document).collect::<Vec<_>>());
        }
        put(&mut d, "hint", o.hint.clone());
        put(&mut d, "collation", o.collation.clone());
        put(&mut d, "sort", o.sort.clone());
        d
    };
    Ok(match m {
        ClientWriteModel::InsertOne { document, .. } => doc! { "insert" => index, "document" => document.clone() },
        ClientWriteModel::UpdateOne { filter, update: u, options, .. } => update("update", filter, u.clone(), false, options),
        ClientWriteModel::UpdateMany { filter, update: u, options, .. } => {
            if options.sort.is_some() {
                return Err(Error::invalid("sort applies to updateOne / replaceOne only"));
            }
            update("update", filter, u.clone(), true, options)
        }
        ClientWriteModel::ReplaceOne { filter, replacement, options, .. } => update("update", filter, Bson::Document(replacement.clone()), false, options),
        ClientWriteModel::DeleteOne { filter, options, .. } | ClientWriteModel::DeleteMany { filter, options, .. } => {
            let mut d = doc! { "delete" => index, "filter" => filter.clone(), "multi" => m.multi() };
            put(&mut d, "hint", options.hint.clone());
            put(&mut d, "collation", options.collation.clone());
            d
        }
    })
}

fn check_model(m: &ClientWriteModel) -> Result<()> {
    match m {
        ClientWriteModel::UpdateOne { update, .. } | ClientWriteModel::UpdateMany { update, .. } => match update {
            Bson::Document(d) if d.first_key().is_some_and(|k| k.starts_with('$')) => Ok(()),
            Bson::Array(_) => Ok(()),
            _ => Err(Error::invalid("an update document needs update operators ($set, $inc, …) or a pipeline")),
        },
        ClientWriteModel::ReplaceOne { replacement, .. } if replacement.first_key().is_some_and(|k| k.starts_with('$')) => {
            Err(Error::invalid("a replacement document cannot hold update operators"))
        }
        _ => Ok(()),
    }
}

impl Client {
    /// Write to any number of collections in as few round trips as the
    /// server's limits allow (`bulkWrite`, MongoDB 8.0+).
    pub async fn bulk_write(&self, models: Vec<ClientWriteModel>, options: ClientBulkWriteOptions) -> Result<ClientBulkWriteResult> {
        self.bulk_write_in(models, options, None).await
    }

    /// [`Client::bulk_write`] in `session`.
    pub async fn bulk_write_in(&self, mut models: Vec<ClientWriteModel>, o: ClientBulkWriteOptions, session: Option<&Session>) -> Result<ClientBulkWriteResult> {
        if models.is_empty() {
            return Err(Error::invalid("a bulk write needs at least one write"));
        }
        if o.write_concern.is_some() && session.is_some_and(Session::in_transaction) {
            return Err(Error::transaction("Cannot set write concern after starting a transaction"));
        }
        let ordered = o.ordered.unwrap_or(true);
        let verbose = o.verbose_results.unwrap_or(false);
        let wc = o.write_concern.clone().unwrap_or_else(|| self.options().write_concern.clone());
        let unacknowledged = wc.is_unacknowledged();
        if unacknowledged && verbose {
            return Err(Error::invalid("Cannot request unacknowledged write concern and verbose results"));
        }
        if unacknowledged && ordered {
            return Err(Error::invalid("Cannot request unacknowledged write concern and ordered writes"));
        }
        for m in &models {
            check_model(m)?;
        }
        // _id first where missing (recorded for insertResults).
        for m in &mut models {
            if let ClientWriteModel::InsertOne { document, .. } = m {
                if !document.contains_key("_id") {
                    let mut with_id = Document::new().with("_id", ObjectId::new());
                    for (k, v) in std::mem::take(document) {
                        with_id.insert(k, v);
                    }
                    *document = with_id;
                }
            }
        }
        let addr = self.select(&Selection::Write).await?;
        let (max_count, max_message, max_bson, wire) = {
            let c = self.connection(&addr).await?;
            let h = c.hello();
            (h.max_write_batch(), h.max_message_size(), h.max_bson_size(), h.max_wire_version())
        };
        if wire < 25 {
            return Err(Error::invalid("MongoClient.bulkWrite needs MongoDB 8.0 or later"));
        }
        let mut base = doc! { "bulkWrite" => 1, "errorsOnly" => !verbose, "ordered" => ordered };
        put(&mut base, "bypassDocumentValidation", o.bypass_document_validation);
        put(&mut base, "comment", o.comment.clone());
        put(&mut base, "let", o.let_vars.clone());
        put(&mut base, "rawData", o.raw_data);
        let base_size = base.to_bytes()?.len();
        let max_ops_bytes = max_message.saturating_sub(MESSAGE_OVERHEAD_BYTES + base_size);

        let mut out = ClientBulkWriteResult { acknowledged: !unacknowledged, verbose: verbose.then(VerboseResults::default), ..Default::default() };
        let mut failure = ClientBulkWriteFailure::default();
        let mut any_success = false;
        let mut start = 0;
        while start < models.len() {
            // One batch: as many models as the limits allow.
            let mut ops: Vec<Document> = Vec::new();
            let mut ns_info: Vec<Document> = Vec::new();
            let mut ns_index: BTreeMap<String, i32> = BTreeMap::new();
            let mut bytes = 0usize;
            let mut end = start;
            while end < models.len() && ops.len() < max_count {
                let ns = models[end].namespace().to_string();
                let (index, ns_doc) = match ns_index.get(&ns) {
                    Some(i) => (*i, None),
                    None => (ns_index.len() as i32, Some(doc! { "ns" => ns.clone() })),
                };
                let op = op_doc(&models[end], index)?;
                let op_size = op.to_bytes()?.len();
                if op_size > max_bson + 16 * 1024 {
                    return Err(Error::invalid(format!("write {end} is {op_size} bytes, above the server's maxBsonObjectSize")));
                }
                let added = op_size + ns_doc.as_ref().map_or(Ok(0), |d| d.to_bytes().map(|b| b.len()))?;
                if bytes + added > max_ops_bytes {
                    break;
                }
                bytes += added;
                if let Some(d) = ns_doc {
                    ns_index.insert(ns, index);
                    ns_info.push(d);
                }
                ops.push(op);
                end += 1;
            }
            if end == start {
                return Err(Error::invalid("a write is too large to fit in a bulkWrite command"));
            }
            let retry = if models[start..end].iter().any(ClientWriteModel::multi) { Retry::No } else { Retry::Write };
            let seqs = [Sequence { identifier: "ops", documents: &ops }, Sequence { identifier: "nsInfo", documents: &ns_info }];
            let op = Op {
                db: "admin",
                command: base.clone(),
                seqs: &seqs,
                selection: Selection::Write,
                retry,
                read_concern: false,
                write_concern: true,
                session,
                cursor_session: false,
                rc: None,
                wc: Some(wc.clone()),
            };
            let ran = match self.execute(op).await {
                Ok(r) => r,
                Err(e) => {
                    // A write-concern error with results still has them.
                    let reply = e.server_response().filter(|r| r.contains_key("cursor") && crate::error::reply_ok(r)).cloned();
                    match (reply, &e.kind) {
                        (Some(reply), ErrorKind::Write { write_concern: Some((c, m)), .. }) => {
                            failure.write_concern_errors.push((*c, m.clone()));
                            let ran = crate::client::Ran { reply, address: addr.clone(), implicit: None };
                            if self.absorb(ran, &models, start, end, ordered, &mut out, &mut failure, &mut any_success, session).await? {
                                break;
                            }
                            start = end;
                            continue;
                        }
                        _ => {
                            failure.error = Some(e);
                            break;
                        }
                    }
                }
            };
            if unacknowledged {
                start = end;
                continue;
            }
            if let Some(Bson::Document(w)) = ran.reply.get("writeConcernError") {
                failure.write_concern_errors.push((w.get_i64("code").unwrap_or(0) as i32, w.get_str("errmsg").unwrap_or("").to_string()));
            }
            match self.absorb(ran, &models, start, end, ordered, &mut out, &mut failure, &mut any_success, session).await {
                Ok(true) => break,
                Ok(false) => {}
                Err(e) => {
                    failure.error = Some(e);
                    break;
                }
            }
            start = end;
        }
        if failure.error.is_none() && failure.write_errors.is_empty() && failure.write_concern_errors.is_empty() {
            return Ok(out);
        }
        if any_success {
            failure.partial_result = Some(out);
        }
        // A top-level error alone, with nothing else observed, is just that error.
        if failure.write_errors.is_empty() && failure.write_concern_errors.is_empty() && failure.partial_result.is_none() {
            if let Some(e) = failure.error.take() {
                return Err(e);
            }
        }
        let message = match (&failure.error, failure.write_errors.values().next(), failure.write_concern_errors.first()) {
            (Some(e), _, _) => format!("bulk write failed: {e}"),
            (None, Some((_, m, _)), _) => format!("bulk write: {} write error(s), the first: {m}", failure.write_errors.len()),
            (None, None, Some((_, m))) => format!("bulk write: write concern error: {m}"),
            _ => "bulk write failed".into(),
        };
        let mut labels = failure.error.as_ref().map(|e| e.labels.clone()).unwrap_or_default();
        labels.dedup();
        let mut e = Error::new(ErrorKind::ClientBulkWrite(Box::new(failure)), message);
        e.labels = labels;
        Err(e)
    }

    /// Fold one batch's reply (counts and results cursor) into the result;
    /// `Ok(true)`: an ordered bulk write must stop.
    #[allow(clippy::too_many_arguments)]
    async fn absorb(
        &self,
        ran: crate::client::Ran,
        models: &[ClientWriteModel],
        start: usize,
        end: usize,
        ordered: bool,
        out: &mut ClientBulkWriteResult,
        failure: &mut ClientBulkWriteFailure,
        any_success: &mut bool,
        session: Option<&Session>,
    ) -> Result<bool> {
        let r = ran.reply.clone();
        let n = |k: &str| r.get_i64(k).unwrap_or(0);
        let n_errors = n("nErrors");
        out.inserted += n("nInserted");
        out.upserted += n("nUpserted");
        out.matched += n("nMatched");
        out.modified += n("nModified");
        out.deleted += n("nDeleted");
        if !r.contains_key("cursor") {
            *any_success = true;
            return Ok(false);
        }
        let mut cursor: Cursor<Document> = Cursor::from_reply(self, ran, session, None)?;
        let mut first_error: Option<usize> = None;
        while let Some(d) = cursor.next_document().await {
            let d = d?;
            let idx = d.get_i64("idx").unwrap_or(0) as usize + start;
            let ok = d.get("ok").and_then(Bson::as_f64).unwrap_or(0.0) == 1.0;
            if !ok {
                let info = d.get_document("errInfo").cloned();
                failure.write_errors.insert(idx, (d.get_i64("code").unwrap_or(0) as i32, d.get_str("errmsg").unwrap_or("").to_string(), info));
                first_error = Some(first_error.map_or(idx, |f| f.min(idx)));
                continue;
            }
            if let Some(v) = out.verbose.as_mut() {
                match &models[idx] {
                    ClientWriteModel::InsertOne { document, .. } => {
                        if d.get_i64("n") == Some(1) {
                            v.insert_results.insert(idx, document.get("_id").cloned().unwrap_or(Bson::Null));
                        }
                    }
                    ClientWriteModel::UpdateOne { .. } | ClientWriteModel::UpdateMany { .. } | ClientWriteModel::ReplaceOne { .. } => {
                        let upserted_id = d.get_document("upserted").and_then(|u| u.get("_id").cloned());
                        let matched = d.get_i64("n").unwrap_or(0);
                        v.update_results.insert(idx, ClientUpdateResult { matched, modified: d.get_i64("nModified").unwrap_or(0), upserted_id });
                    }
                    ClientWriteModel::DeleteOne { .. } | ClientWriteModel::DeleteMany { .. } => {
                        v.delete_results.insert(idx, d.get_i64("n").unwrap_or(0));
                    }
                }
            }
        }
        let batch = (end - start) as i64;
        let succeeded = if ordered { n_errors == 0 || first_error.is_some_and(|f| f > start) } else { n_errors < batch };
        if succeeded {
            *any_success = true;
        }
        Ok(ordered && n_errors > 0)
    }
}
