//! The ORM backend: entities as documents.
//!
//! - The primary key column is `_id`; other columns keep their names.
//! - Values are typed by the column (`Integer` → int32 when it fits,
//!   `BigInt` → int64, `Float` → double, `Boolean`, `Binary` → binary,
//!   `Json` → an embedded document / array, text otherwise), so filters
//!   compare like the SQL backends do.
//! - Generated keys: numeric ids come from a per-collection counter
//!   (`__kw_counters`, an atomic `findAndModify $inc`), text ids are fresh
//!   ObjectIds (stored as ObjectId, exposed as 24 hex digits).
//! - `#[column(unique)]` columns get unique indexes, created on first use.
//! - [`Spec`] filters become query documents (`LIKE` patterns become
//!   anchored regexes); `!=` and `NOT` treat a missing field as SQL treats
//!   `NULL`.
//! - Transactions run in a session (replica set or sharded cluster).
//!   `find_by_id_for_update` writes a lock marker, so a concurrent
//!   transaction that touches the same document fails with a write
//!   conflict instead of losing an update; [`MongoDriver::transaction_retrying`]
//!   re-runs the work on such transient errors.

use crate::ordered_json::OJ;
use crate::bson::{Binary, Bson, Document, ObjectId};
use crate::client::{Client, Session, TransactionOptions};
use crate::collection::{Collection, Database, FindOneAndModifyOptions, FindOptions, IndexModel, UpdateOptions};
use crate::doc;
use crate::error::Error;
use kernway_orm_core::{
    driver::{Driver, DriverCapabilities},
    entity::{ColumnDef, ColumnType, Entity},
    error::OrmError,
    page::Page,
    query::QueryBuilder,
    repository::Repository,
    spec::Spec,
    transaction::{Transaction, TransactionalDriver, TxRepository},
    BoxFuture,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value as J;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

const COUNTERS: &str = "__kw_counters";
const LOCK_FIELD: &str = "__kw_lock";

fn conv(e: impl std::fmt::Display) -> OrmError {
    OrmError::TypeConversion(e.to_string())
}

fn pk<T: Entity>() -> &'static ColumnDef {
    T::columns().iter().find(|c| c.primary_key).unwrap_or(&T::columns()[0])
}

fn key_of(c: &ColumnDef) -> &'static str {
    if c.primary_key {
        "_id"
    } else {
        c.name
    }
}

/// The column a filter / sort names (by field or column name).
fn column<T: Entity>(name: &str) -> Option<&'static ColumnDef> {
    T::columns().iter().find(|c| c.field == name || c.name == name)
}

/// A JSON field value as BSON, typed by its column.
fn to_bson(v: &J, c: &ColumnDef) -> Result<Bson, OrmError> {
    Ok(match (v, &c.col_type) {
        (J::Null, _) => Bson::Null,
        (J::Number(n), ColumnType::Integer) => match n.as_i64() {
            Some(i) => i32::try_from(i).map_or(Bson::Int64(i), Bson::Int32),
            None => Bson::Double(n.as_f64().unwrap_or(0.0)),
        },
        (J::Number(n), ColumnType::BigInt) => n.as_i64().map_or_else(|| Bson::Double(n.as_f64().unwrap_or(0.0)), Bson::Int64),
        (J::Number(n), ColumnType::Float) => Bson::Double(n.as_f64().unwrap_or(0.0)),
        (J::Array(items), ColumnType::Binary) => {
            Bson::Binary(Binary { subtype: 0, bytes: items.iter().map(|x| x.as_u64().unwrap_or(0) as u8).collect() })
        }
        (J::String(s), ColumnType::Text | ColumnType::Unknown) if c.primary_key && c.auto => {
            ObjectId::parse_str(s).map_or_else(|| Bson::String(s.clone()), Bson::ObjectId)
        }
        _ => Bson::from_extended_json(v).map_err(conv)?,
    })
}

/// A BSON value back as JSON for the entity's field.
fn to_json(b: &Bson, c: &ColumnDef) -> J {
    match b {
        Bson::ObjectId(o) => J::String(o.to_hex()),
        Bson::Binary(bin) if c.col_type == ColumnType::Binary => J::Array(bin.bytes.iter().map(|x| J::from(*x)).collect()),
        Bson::DateTime(d) if c.col_type != ColumnType::Json => d.to_rfc3339().map_or_else(|| J::from(d.0), J::String),
        Bson::Decimal128(d) if c.col_type != ColumnType::Json => J::String(d.to_string()),
        Bson::Int32(i) if c.col_type == ColumnType::Boolean => J::Bool(*i != 0),
        Bson::Double(f) if matches!(c.col_type, ColumnType::Integer | ColumnType::BigInt) && f.fract() == 0.0 => J::from(*f as i64),
        other => other.to_relaxed_json(),
    }
}

fn entity_to_doc<T: Entity + Serialize>(e: &T) -> Result<Document, OrmError> {
    // Through JSON text, read back order-keeping: an object inside a field
    // (a JSON column) keeps its keys' order (serde_json::Map would sort them).
    let text = serde_json::to_string(e).map_err(conv)?;
    let j: OJ = serde_json::from_str(&text).map_err(conv)?;
    let obj = j.as_object().ok_or_else(|| conv("an entity serializes as a JSON object"))?;
    let field = |c: &ColumnDef| -> Result<Bson, OrmError> {
        match obj.get(c.field) {
            None | Some(OJ::Null) => Ok(Bson::Null),
            Some(v @ (OJ::Object(_) | OJ::Array(_))) if !matches!(c.col_type, ColumnType::Binary) => Bson::from_ordered(v).map_err(conv),
            Some(v) => to_bson(&v.to_value(), c),
        }
    };
    let mut d = Document::new();
    // _id first, then the columns in declaration order.
    d.insert("_id", field(pk::<T>())?);
    for c in T::columns().iter().filter(|c| !c.primary_key) {
        d.insert(c.name, field(c)?);
    }
    Ok(d)
}

fn doc_to_entity<T: Entity + DeserializeOwned>(d: &Document) -> Result<T, OrmError> {
    let mut obj = serde_json::Map::new();
    for c in T::columns() {
        let v = d.get(key_of(c)).map_or(J::Null, |b| to_json(b, c));
        obj.insert(c.field.to_string(), v);
    }
    serde_json::from_value(J::Object(obj)).map_err(conv)
}

fn id_filter<T: Entity>(id: &T::Id) -> Result<Document, OrmError> {
    let j = serde_json::to_value(id).map_err(conv)?;
    Ok(doc! { "_id" => to_bson(&j, pk::<T>())? })
}

/// A filter value (always text in a [`Spec`]) typed by its column.
fn spec_value(c: Option<&ColumnDef>, v: &str) -> Bson {
    let Some(c) = c else { return Bson::String(v.to_string()) };
    match c.col_type {
        ColumnType::Integer | ColumnType::BigInt => v.parse::<i64>().map_or_else(|_| v.parse::<f64>().map_or_else(|_| Bson::String(v.into()), Bson::Double), Bson::Int64),
        ColumnType::Float => v.parse::<f64>().map_or_else(|_| Bson::String(v.into()), Bson::Double),
        ColumnType::Boolean => Bson::Boolean(v == "1" || v.eq_ignore_ascii_case("true")),
        _ if c.primary_key && c.auto => ObjectId::parse_str(v).map_or_else(|| Bson::String(v.into()), Bson::ObjectId),
        _ => Bson::String(v.to_string()),
    }
}

/// A SQL `LIKE` pattern as an anchored regex (`%` any run, `_` one char).
fn like_regex(p: &str) -> String {
    let mut r = String::from("^");
    let mut chars = p.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '%' => r.push_str("[\\s\\S]*"),
            '_' => r.push_str("[\\s\\S]"),
            '\\' => {
                if let Some(n) = chars.next() {
                    r.push_str(&regex_escape(n));
                }
            }
            other => r.push_str(&regex_escape(other)),
        }
    }
    r.push('$');
    r
}

fn regex_escape(c: char) -> String {
    if "\\^$.|?*+()[]{}/-".contains(c) {
        format!("\\{c}")
    } else {
        c.to_string()
    }
}

/// A [`Spec`] as a query document.
pub fn spec_filter<T: Entity>(s: &Spec) -> Document {
    let field = |f: &str| -> (String, Option<&'static ColumnDef>) {
        match column::<T>(f) {
            Some(c) => (key_of(c).to_string(), Some(c)),
            None => (f.to_string(), None),
        }
    };
    let cmp = |f: &str, op: &str, v: &str| {
        let (k, c) = field(f);
        doc! { k => doc! { op => spec_value(c, v) } }
    };
    match s {
        Spec::Eq(f, v) => {
            let (k, c) = field(f);
            doc! { k => spec_value(c, v) }
        }
        // SQL's `!=` never matches NULL.
        Spec::Ne(f, v) => {
            let (k, c) = field(f);
            doc! { k => doc! { "$nin" => vec![spec_value(c, v), Bson::Null] } }
        }
        Spec::Gt(f, v) => cmp(f, "$gt", v),
        Spec::Lt(f, v) => cmp(f, "$lt", v),
        Spec::Gte(f, v) => cmp(f, "$gte", v),
        Spec::Lte(f, v) => cmp(f, "$lte", v),
        Spec::Like(f, v) => {
            let (k, _) = field(f);
            doc! { k => crate::bson::Regex::new(&like_regex(v), "s") }
        }
        Spec::In(f, vs) => {
            let (k, c) = field(f);
            doc! { k => doc! { "$in" => vs.iter().map(|v| spec_value(c, v)).collect::<Vec<_>>() } }
        }
        Spec::Between(f, lo, hi) => {
            let (k, c) = field(f);
            doc! { k => doc! { "$gte" => spec_value(c, lo), "$lte" => spec_value(c, hi) } }
        }
        Spec::IsNull(f) => doc! { field(f).0 => Bson::Null },
        Spec::IsNotNull(f) => doc! { field(f).0 => doc! { "$ne" => Bson::Null } },
        Spec::And(a, b) => doc! { "$and" => vec![Bson::Document(spec_filter::<T>(a)), Bson::Document(spec_filter::<T>(b))] },
        Spec::Or(a, b) => doc! { "$or" => vec![Bson::Document(spec_filter::<T>(a)), Bson::Document(spec_filter::<T>(b))] },
        Spec::Not(a) => doc! { "$nor" => vec![Bson::Document(spec_filter::<T>(a))] },
    }
}

/// The documents and settings an ORM handle works with.
#[derive(Clone)]
struct Store {
    db: Database,
    session: Option<Session>,
    indexed: Rc<RefCell<HashSet<&'static str>>>,
}

impl Store {
    fn coll<T: Entity>(&self) -> Collection<Document> {
        let c = self.db.collection::<Document>(T::table_name());
        match &self.session {
            Some(s) => c.in_session(s),
            None => c,
        }
    }

    /// Unique indexes for `#[column(unique)]` columns, once per collection.
    async fn ensure_indexes<T: Entity>(&self) -> Result<(), OrmError> {
        if self.indexed.borrow().contains(T::table_name()) {
            return Ok(());
        }
        let unique: Vec<IndexModel> = T::columns()
            .iter()
            .filter(|c| c.unique && !c.primary_key)
            .map(|c| IndexModel { keys: doc! { c.name => 1 }, unique: true, ..Default::default() })
            .collect();
        if !unique.is_empty() {
            // Index builds cannot run inside a transaction: outside any session.
            self.db.collection::<Document>(T::table_name()).create_indexes(unique).await.map_err(OrmError::from)?;
        }
        self.indexed.borrow_mut().insert(T::table_name());
        Ok(())
    }

    async fn next_ids(&self, table: &str, n: i64) -> Result<i64, OrmError> {
        // Counters live outside transactions (like sequences): ids are never reused.
        let counters = self.db.collection::<Document>(COUNTERS);
        let o = FindOneAndModifyOptions { upsert: true, return_after: true, ..Default::default() };
        let d = counters.find_one_and_update(doc! { "_id" => table }, doc! { "$inc" => doc! { "seq" => Bson::Int64(n) } }, o).await.map_err(OrmError::from)?;
        Ok(d.and_then(|d| d.get_i64("seq")).ok_or_else(|| OrmError::Query("counter update returned nothing".into()))? - n + 1)
    }

    async fn find_by_id<T: Entity + DeserializeOwned>(&self, id: &T::Id) -> Result<Option<T>, OrmError> {
        let d = self.coll::<T>().find_one(id_filter::<T>(id)?, FindOptions::default()).await.map_err(OrmError::from)?;
        d.map(|d| doc_to_entity(&d)).transpose()
    }

    async fn save<T: Entity + Serialize + DeserializeOwned>(&self, entity: T) -> Result<T, OrmError> {
        self.ensure_indexes::<T>().await?;
        let p = pk::<T>();
        let id = serde_json::to_value(entity.id()).map_err(conv)?;
        let is_new = p.auto && (id.as_i64() == Some(0) || id.as_u64() == Some(0) || id.as_str() == Some(""));
        let coll = self.coll::<T>();
        if is_new {
            let mut j = serde_json::to_value(&entity).map_err(conv)?;
            let new_id = if id.is_string() { J::String(ObjectId::new().to_hex()) } else { J::from(self.next_ids(T::table_name(), 1).await?) };
            j[p.field] = new_id;
            let e: T = serde_json::from_value(j).map_err(conv)?;
            coll.insert_many_documents(vec![entity_to_doc(&e)?], true).await.map_err(OrmError::from)?;
            return Ok(e);
        }
        let d = entity_to_doc(&entity)?;
        let filter = doc! { "_id" => d.get("_id").cloned().unwrap_or(Bson::Null) };
        let mut body = d;
        body.remove("_id");
        // Manual ids are inserted when new (as SQL's save does); generated
        // ids must exist.
        let r = coll.update_one(filter, doc! { "$set" => body.clone(), "$unset" => doc! { LOCK_FIELD => "" } }, UpdateOptions { upsert: !p.auto, ..Default::default() })
            .await
            .map_err(OrmError::from)?;
        if r.matched == 0 && r.upserted_id.is_none() {
            return Err(OrmError::NotFound);
        }
        Ok(entity)
    }

    async fn delete_by_id<T: Entity>(&self, id: &T::Id) -> Result<(), OrmError> {
        self.coll::<T>().delete_one(id_filter::<T>(id)?).await.map(|_| ()).map_err(OrmError::from)
    }

    async fn find_for_update<T: Entity + DeserializeOwned>(&self, id: &T::Id) -> Result<Option<T>, OrmError> {
        let o = FindOneAndModifyOptions { return_after: true, ..Default::default() };
        let d = self
            .coll::<T>()
            .find_one_and_update(id_filter::<T>(id)?, doc! { "$set" => doc! { LOCK_FIELD => ObjectId::new() } }, o)
            .await
            .map_err(write_conflict)?;
        d.map(|d| doc_to_entity(&d)).transpose()
    }
}

fn write_conflict(e: Error) -> OrmError {
    if e.code() == Some(112) || e.has_label("TransientTransactionError") {
        return OrmError::Transaction(format!("write conflict: another transaction holds this document ({e}); retry the transaction"));
    }
    OrmError::from(e)
}

/// The MongoDB ORM driver.
pub struct MongoDriver {
    client: Client,
    store: Store,
}

impl MongoDriver {
    /// Connect; the database is the connection string's (default `test`).
    pub async fn connect(uri: &str) -> Result<Self, OrmError> {
        let client = Client::connect(uri).await.map_err(OrmError::from)?;
        let db = client.default_database().unwrap_or_else(|| client.database("test"));
        client.ping().await.map_err(OrmError::from)?;
        Ok(Self::new(client, db))
    }

    /// Over an existing client and database.
    pub fn new(client: Client, db: Database) -> Self {
        MongoDriver { client, store: Store { db, session: None, indexed: Rc::new(RefCell::new(HashSet::new())) } }
    }

    /// The client (raw collections, sessions, change streams, GridFS).
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The database.
    pub fn database(&self) -> &Database {
        &self.store.db
    }

    /// Run `work` in a transaction, re-running it on transient errors
    /// (write conflicts, elections) for up to two minutes.
    pub async fn transaction_retrying<R, F>(&self, mut work: F) -> Result<R, OrmError>
    where
        F: AsyncFnMut(&MongoTx) -> Result<R, OrmError>,
    {
        let started = std::time::Instant::now();
        loop {
            let tx = self.begin()?;
            match work(&tx).await {
                Ok(v) => match tx.session.commit_transaction().await {
                    Ok(()) => return Ok(v),
                    Err(e) if (e.has_label("TransientTransactionError") || e.has_label("UnknownTransactionCommitResult")) && started.elapsed().as_secs() < 120 => {
                        continue
                    }
                    Err(e) => return Err(e.into()),
                },
                Err(OrmError::Transaction(m)) if started.elapsed().as_secs() < 120 => {
                    let _ = tx.session.abort_transaction().await;
                    let _ = m;
                    continue;
                }
                Err(e) => {
                    let _ = tx.session.abort_transaction().await;
                    return Err(e);
                }
            }
        }
    }

    fn begin(&self) -> Result<MongoTx, OrmError> {
        let session = self.client.start_session();
        session.start_transaction(TransactionOptions::default()).map_err(|e| OrmError::Transaction(e.to_string()))?;
        let store = Store { db: session.database(self.store.db.name()), session: Some(session.clone()), indexed: self.store.indexed.clone() };
        Ok(MongoTx { session, store })
    }
}

/// A transaction handle: repositories whose operations run in its session.
pub struct MongoTx {
    session: Session,
    store: Store,
}

impl MongoTx {
    /// The session (raw collections inside the transaction).
    pub fn session(&self) -> &Session {
        &self.session
    }
}

struct MongoRepository<T> {
    store: Store,
    _t: std::marker::PhantomData<T>,
}

impl<T: Entity + Serialize + DeserializeOwned + 'static> Repository<T> for MongoRepository<T> {
    fn find_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        Box::pin(self.store.find_by_id(id))
    }

    fn save<'a>(&'a self, entity: T) -> BoxFuture<'a, Result<T, OrmError>> {
        Box::pin(self.store.save(entity))
    }

    fn delete_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>> {
        Box::pin(self.store.delete_by_id::<T>(id))
    }

    fn query(&self) -> Box<dyn QueryBuilder<T>> {
        Box::new(MongoQuery::<T> { store: self.store.clone(), filters: Vec::new(), sort: Document::new(), skip: 0, limit: None, _t: std::marker::PhantomData })
    }

    fn find_all_by_ids<'a>(&'a self, ids: &'a [T::Id]) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        Box::pin(async move {
            let keys = ids.iter().map(|id| Ok(id_filter::<T>(id)?.remove("_id").unwrap_or(Bson::Null))).collect::<Result<Vec<_>, OrmError>>()?;
            let docs = self.store.coll::<T>().find(doc! { "_id" => doc! { "$in" => keys } }, FindOptions::default()).await.map_err(OrmError::from)?;
            docs.collect().await.map_err(OrmError::from)?.iter().map(doc_to_entity).collect()
        })
    }

    fn count<'a>(&'a self) -> BoxFuture<'a, Result<u64, OrmError>> {
        Box::pin(async move { self.store.coll::<T>().count_documents(Document::new()).await.map_err(OrmError::from) })
    }

    fn save_all<'a>(&'a self, entities: Vec<T>) -> BoxFuture<'a, Result<Vec<T>, OrmError>> {
        Box::pin(async move {
            self.store.ensure_indexes::<T>().await?;
            let p = pk::<T>();
            let new: Vec<bool> = entities
                .iter()
                .map(|e| serde_json::to_value(e.id()).map(|j| p.auto && (j.as_i64() == Some(0) || j.as_str() == Some(""))))
                .collect::<Result<_, _>>()
                .map_err(conv)?;
            let fresh_numeric = entities
                .iter()
                .zip(&new)
                .filter(|(e, n)| **n && serde_json::to_value(e.id()).map(|j| j.is_number()).unwrap_or(false))
                .count() as i64;
            // One counter step for the whole batch, then one insert.
            let mut next = if fresh_numeric > 0 { self.store.next_ids(T::table_name(), fresh_numeric).await? } else { 0 };
            let mut out = Vec::with_capacity(entities.len());
            let mut docs = Vec::new();
            for (e, is_new) in entities.into_iter().zip(new) {
                if !is_new {
                    out.push(self.store.save(e).await?);
                    continue;
                }
                let mut j = serde_json::to_value(&e).map_err(conv)?;
                j[p.field] = if j[p.field].is_string() {
                    J::String(ObjectId::new().to_hex())
                } else {
                    next += 1;
                    J::from(next - 1)
                };
                let e: T = serde_json::from_value(j).map_err(conv)?;
                docs.push(entity_to_doc(&e)?);
                out.push(e);
            }
            if !docs.is_empty() {
                self.store.coll::<T>().insert_many_documents(docs, true).await.map_err(OrmError::from)?;
            }
            Ok(out)
        })
    }

    fn delete_all_by_ids<'a>(&'a self, ids: &'a [T::Id]) -> BoxFuture<'a, Result<(), OrmError>> {
        Box::pin(async move {
            let keys = ids.iter().map(|id| Ok(id_filter::<T>(id)?.remove("_id").unwrap_or(Bson::Null))).collect::<Result<Vec<_>, OrmError>>()?;
            self.store.coll::<T>().delete_many(doc! { "_id" => doc! { "$in" => keys } }).await.map(|_| ()).map_err(OrmError::from)
        })
    }
}

struct MongoQuery<T> {
    store: Store,
    filters: Vec<Document>,
    sort: Document,
    skip: u64,
    limit: Option<u64>,
    _t: std::marker::PhantomData<T>,
}

impl<T: Entity + DeserializeOwned + 'static> MongoQuery<T> {
    fn filter(&self) -> Document {
        match self.filters.len() {
            0 => Document::new(),
            1 => self.filters[0].clone(),
            _ => doc! { "$and" => self.filters.iter().cloned().map(Bson::Document).collect::<Vec<_>>() },
        }
    }

    fn spec(mut self: Box<Self>, s: Spec) -> Box<Self> {
        self.filters.push(spec_filter::<T>(&s));
        self
    }

    async fn rows(&self) -> Result<Vec<T>, OrmError> {
        let o = FindOptions {
            sort: (!self.sort.is_empty()).then(|| self.sort.clone()),
            skip: (self.skip > 0).then_some(self.skip),
            limit: self.limit.map(|l| l as i64),
            ..Default::default()
        };
        if self.limit == Some(0) {
            return Ok(Vec::new());
        }
        let cur = self.store.coll::<T>().find(self.filter(), o).await.map_err(OrmError::from)?;
        cur.collect().await.map_err(OrmError::from)?.iter().map(doc_to_entity).collect()
    }

    fn order(mut self: Box<Self>, field: &str, dir: i32) -> Box<Self> {
        let key = column::<T>(field).map_or(field.to_string(), |c| key_of(c).to_string());
        self.sort.insert(key, dir);
        self
    }
}

impl<T: Entity + DeserializeOwned + 'static> QueryBuilder<T> for MongoQuery<T> {
    fn filter_eq(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.spec(Spec::Eq(field, value.into()))
    }
    fn filter_ne(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.spec(Spec::Ne(field, value.into()))
    }
    fn filter_gt(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.spec(Spec::Gt(field, value.into()))
    }
    fn filter_lt(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.spec(Spec::Lt(field, value.into()))
    }
    fn filter_like(self: Box<Self>, field: &'static str, pattern: &str) -> Box<dyn QueryBuilder<T>> {
        self.spec(Spec::Like(field, pattern.into()))
    }
    fn filter_gte(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.spec(Spec::Gte(field, value.into()))
    }
    fn filter_lte(self: Box<Self>, field: &'static str, value: &str) -> Box<dyn QueryBuilder<T>> {
        self.spec(Spec::Lte(field, value.into()))
    }
    fn filter_in(self: Box<Self>, field: &'static str, values: Vec<String>) -> Box<dyn QueryBuilder<T>> {
        self.spec(Spec::In(field, values))
    }
    fn filter_between(self: Box<Self>, field: &'static str, from: &str, to: &str) -> Box<dyn QueryBuilder<T>> {
        self.spec(Spec::Between(field, from.into(), to.into()))
    }
    fn filter_is_null(self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self.spec(Spec::IsNull(field))
    }
    fn filter_is_not_null(self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self.spec(Spec::IsNotNull(field))
    }
    fn filter_exists(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        let key = column::<T>(field).map_or(field.to_string(), |c| key_of(c).to_string());
        self.filters.push(doc! { key => doc! { "$exists" => true } });
        self
    }
    fn filter_not_exists(mut self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        let key = column::<T>(field).map_or(field.to_string(), |c| key_of(c).to_string());
        self.filters.push(doc! { key => doc! { "$exists" => false } });
        self
    }
    fn filter_spec(self: Box<Self>, spec: Spec) -> Box<dyn QueryBuilder<T>> {
        self.spec(spec)
    }
    fn order_by_asc(self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self.order(field, 1)
    }
    fn order_by_desc(self: Box<Self>, field: &'static str) -> Box<dyn QueryBuilder<T>> {
        self.order(field, -1)
    }
    fn limit(mut self: Box<Self>, n: u64) -> Box<dyn QueryBuilder<T>> {
        self.limit = Some(n);
        self
    }
    fn offset(mut self: Box<Self>, n: u64) -> Box<dyn QueryBuilder<T>> {
        self.skip = n;
        self
    }
    fn with(self: Box<Self>, _relation: &'static str) -> Box<dyn QueryBuilder<T>> {
        // Documents embed what a relation would join; nothing to load.
        self
    }
    fn fetch_all(self: Box<Self>) -> BoxFuture<'static, Result<Vec<T>, OrmError>> {
        Box::pin(async move { self.rows().await })
    }
    fn fetch_one(mut self: Box<Self>) -> BoxFuture<'static, Result<Option<T>, OrmError>> {
        self.limit = Some(1);
        Box::pin(async move { Ok(self.rows().await?.into_iter().next()) })
    }
    fn fetch_count(self: Box<Self>) -> BoxFuture<'static, Result<u64, OrmError>> {
        Box::pin(async move { self.store.coll::<T>().count_documents(self.filter()).await.map_err(OrmError::from) })
    }
    fn fetch_page(mut self: Box<Self>, page: u64, size: u64) -> BoxFuture<'static, Result<Page<T>, OrmError>> {
        Box::pin(async move {
            let total = self.store.coll::<T>().count_documents(self.filter()).await.map_err(OrmError::from)?;
            if size == 0 {
                return Ok(Page::new(Vec::new(), total, page, size));
            }
            self.skip = page.saturating_mul(size);
            self.limit = Some(size);
            let items = self.rows().await?;
            Ok(Page::new(items, total, page, size))
        })
    }
}

impl Driver for MongoDriver {
    fn repository<T>(&self) -> Box<dyn Repository<T>>
    where
        T: Entity + Serialize + DeserializeOwned + 'static,
    {
        Box::new(MongoRepository::<T> { store: self.store.clone(), _t: std::marker::PhantomData })
    }

    fn ping(&self) -> BoxFuture<'_, Result<(), OrmError>> {
        Box::pin(async move { self.client.ping().await.map_err(OrmError::from) })
    }

    fn capabilities(&self) -> DriverCapabilities {
        DriverCapabilities { transactions: true, raw_query: true, full_text_search: true, json_columns: true, migrations: false }
    }
}

struct MongoTxRepository<'a, T> {
    store: &'a Store,
    _t: std::marker::PhantomData<T>,
}

impl<T: Entity + Serialize + DeserializeOwned + 'static> TxRepository<T> for MongoTxRepository<'_, T> {
    fn find_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        Box::pin(async move { self.store.find_by_id(id).await.map_err(tx_error) })
    }
    fn find_by_id_for_update<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<Option<T>, OrmError>> {
        Box::pin(self.store.find_for_update(id))
    }
    fn save<'a>(&'a self, entity: T) -> BoxFuture<'a, Result<T, OrmError>> {
        Box::pin(async move { self.store.save(entity).await.map_err(tx_error) })
    }
    fn delete_by_id<'a>(&'a self, id: &'a T::Id) -> BoxFuture<'a, Result<(), OrmError>> {
        Box::pin(async move { self.store.delete_by_id::<T>(id).await.map_err(tx_error) })
    }
}

/// Inside a transaction, a write conflict is a transaction error (retry).
fn tx_error(e: OrmError) -> OrmError {
    match e {
        OrmError::Query(m) if m.contains("WriteConflict") || m.contains("TransientTransactionError") => OrmError::Transaction(m),
        other => other,
    }
}

impl Transaction for MongoTx {
    fn repository<T>(&self) -> Box<dyn TxRepository<T> + '_>
    where
        T: Entity + Serialize + DeserializeOwned + 'static,
    {
        Box::new(MongoTxRepository::<T> { store: &self.store, _t: std::marker::PhantomData })
    }
}

impl TransactionalDriver for MongoDriver {
    type Tx = MongoTx;

    fn transaction<'a, R, F>(&'a self, work: F) -> BoxFuture<'a, Result<R, OrmError>>
    where
        F: FnOnce(MongoTx) -> BoxFuture<'a, Result<R, OrmError>> + 'a,
        R: 'a,
    {
        Box::pin(async move {
            let tx = self.begin()?;
            let session = tx.session.clone();
            match work(tx).await {
                Ok(v) => {
                    session.commit_transaction().await.map_err(|e| OrmError::Transaction(e.to_string()))?;
                    session.end().await;
                    Ok(v)
                }
                Err(e) => {
                    let _ = session.abort_transaction().await;
                    session.end().await;
                    Err(e)
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns() {
        assert_eq!(like_regex("r%"), "^r[\\s\\S]*$");
        assert_eq!(like_regex("a_c"), "^a[\\s\\S]c$");
        assert_eq!(like_regex("1.5%"), "^1\\.5[\\s\\S]*$");
        assert_eq!(like_regex("100\\%"), "^100%$");
    }
}
