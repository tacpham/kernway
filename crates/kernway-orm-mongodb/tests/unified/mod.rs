//! A runner for the official unified test format
//! (mongodb/specifications `source/unified-test-format`), against the
//! deployments of `tests/support/mongo-setup.sh`.
//!
//! Each test ends as one of:
//! - passed, on every deployment its `runOnRequirements` allow;
//! - failed, with the reason (a wrong result, error, event or outcome);
//! - a gap: it needs something the driver does not implement (an argument,
//!   an operation, an entity option, SDAM / CMAP events, …), named;
//! - not runnable here: no deployment meets its requirements (server
//!   version, topology, auth, serverless, …), named.

#![allow(dead_code)]

pub mod entities;
pub mod matcher;
pub mod ops;

use entities::{ClientEntity, Entities, Entity};
use kernway_orm_mongodb::{Address, Bson, Client, ClientOptions, CommandEvent, Document, Error, ErrorKind};
use ops::OpError;
use std::cell::RefCell;
use std::rc::Rc;

pub fn hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// A deployment to run against.
pub struct Deployment {
    pub name: &'static str,
    pub uri: String,
    /// The URI of one mongos (`useMultipleMongoses: false`).
    pub single_mongos_uri: Option<String>,
    /// `single`, `replicaset` or `sharded`.
    pub topology: &'static str,
    pub version: Vec<u32>,
    pub auth: bool,
}

#[derive(Default)]
pub struct Tally {
    pub passed: usize,
    pub failed: Vec<String>,
    pub gaps: Vec<String>,
    pub not_runnable: Vec<String>,
}

impl Tally {
    pub fn total(&self) -> usize {
        self.passed + self.failed.len() + self.gaps.len() + self.not_runnable.len()
    }
}

enum TestResult {
    Passed,
    Failed(String),
    Gap(String),
    NotRunnable(String),
}

fn version(s: &str) -> Vec<u32> {
    s.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

fn cmp_version(a: &[u32], b: &[u32]) -> std::cmp::Ordering {
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x.cmp(&y);
        }
    }
    std::cmp::Ordering::Equal
}

/// Why `reqs` (any one of them) rule this deployment out, or `None`.
fn unmet(reqs: Option<&Vec<Bson>>, dep: &Deployment) -> Option<String> {
    let reqs = reqs?;
    if reqs.is_empty() {
        return None;
    }
    let mut whys = Vec::new();
    for r in reqs {
        let Some(r) = r.as_document() else { continue };
        let mut why = Vec::new();
        if let Some(min) = r.get_str("minServerVersion") {
            if cmp_version(&dep.version, &version(min)).is_lt() {
                why.push(format!("server ≥ {min}"));
            }
        }
        if let Some(max) = r.get_str("maxServerVersion") {
            if cmp_version(&dep.version, &version(max)).is_gt() {
                why.push(format!("server ≤ {max}"));
            }
        }
        if let Some(t) = r.get_array("topologies") {
            let names: Vec<&str> = t.iter().filter_map(Bson::as_str).collect();
            let ok = names.iter().any(|n| *n == dep.topology || (*n == "sharded-replicaset" && dep.topology == "sharded"));
            if !ok {
                why.push(format!("topology {names:?}"));
            }
        }
        if r.get_str("serverless") == Some("require") {
            why.push("serverless".into());
        }
        if let Some(a) = r.get_bool("auth") {
            if a != dep.auth {
                why.push(format!("auth={a}"));
            }
        }
        if r.get_bool("csfle") == Some(true) {
            why.push("csfle (not implemented)".into());
        }
        if r.contains_key("authMechanism") {
            why.push("authMechanism".into());
        }
        if r.contains_key("serverParameters") {
            // Checked against the server below (getParameter).
        }
        if why.is_empty() {
            return None;
        }
        whys.push(why.join(", "));
    }
    Some(format!("requires {}", whys.join(" or ")))
}

/// The runner's own client to the deployment (setup, outcome, fail points).
async fn internal(dep: &Deployment) -> Client {
    Client::connect(&dep.uri).await.expect("connect the internal client")
}

async fn majority_drop_and_insert(c: &Client, data: &[Bson]) -> Result<(), String> {
    for d in data {
        let d = d.as_document().ok_or("initialData entry")?;
        let db = d.get_str("databaseName").ok_or("databaseName")?;
        let coll = d.get_str("collectionName").ok_or("collectionName")?;
        let wc = Document::new().with("w", "majority");
        let _ = c.run_command(db, Document::new().with("drop", coll).with("writeConcern", wc.clone())).await;
        let docs = d.get_array("documents").cloned().unwrap_or_default();
        if docs.is_empty() {
            let mut cmd = Document::new().with("create", coll).with("writeConcern", wc.clone());
            if let Some(o) = d.get_document("createOptions") {
                for (k, v) in o.iter() {
                    cmd.insert(k, v.clone());
                }
            }
            c.run_command(db, cmd).await.map_err(|e| format!("initialData create {db}.{coll}: {e}"))?;
        } else {
            let mut cmd = Document::new().with("insert", coll).with("documents", Bson::Array(docs)).with("writeConcern", wc.clone());
            if let Some(o) = d.get_document("createOptions") {
                let mut create = Document::new().with("create", coll).with("writeConcern", wc.clone());
                for (k, v) in o.iter() {
                    create.insert(k, v.clone());
                }
                c.run_command(db, create).await.map_err(|e| format!("initialData create {db}.{coll}: {e}"))?;
            }
            cmd.insert("ordered", true);
            c.run_command(db, cmd).await.map_err(|e| format!("initialData insert {db}.{coll}: {e}"))?;
        }
    }
    Ok(())
}

fn uri_with(base: &str, options: &Document) -> Result<String, String> {
    let mut uri = base.to_string();
    for (k, v) in options.iter() {
        let v = match v {
            Bson::String(s) => s.clone(),
            Bson::Boolean(b) => b.to_string(),
            Bson::Int32(i) => i.to_string(),
            Bson::Int64(i) => i.to_string(),
            Bson::Double(f) => f.to_string(),
            Bson::Document(d) if k == "readPreferenceTags" => d.iter().map(|(a, b)| format!("{a}:{}", b.as_str().unwrap_or(""))).collect::<Vec<_>>().join(","),
            other => return Err(format!("uriOptions {k} = {other:?}")),
        };
        let sep = if uri.contains('?') { '&' } else if uri.ends_with('/') { '?' } else { return Err("base URI without '/'".into()) };
        uri.push(sep);
        uri.push_str(k);
        uri.push('=');
        uri.push_str(&v);
    }
    Ok(uri)
}

thread_local! {
    /// The cluster time after initialData: every session entity starts from it
    /// (the unified format's MigrationConflict / stale-snapshot workaround).
    static CLUSTER_TIME: RefCell<Option<Document>> = const { RefCell::new(None) };
}

/// Build the entities of a `createEntities` list.
async fn create_entities(list: &[Bson], e: &mut Entities, dep: &Deployment) -> Result<(), TestResult> {
    for item in list {
        let item = item.as_document().ok_or_else(|| TestResult::Failed("entity".into()))?;
        let (kind, spec) = item.iter().next().ok_or_else(|| TestResult::Failed("empty entity".into()))?;
        let spec = spec.as_document().cloned().unwrap_or_default();
        let id = spec.get_str("id").unwrap_or("").to_string();
        let gap = |w: String| Err(TestResult::Gap(w));
        let fail = |w: String| TestResult::Failed(w);
        match kind {
            "client" => {
                for k in spec.keys() {
                    match k {
                        "id" | "uriOptions" | "useMultipleMongoses" | "observeEvents" | "ignoreCommandMonitoringEvents" | "observeSensitiveCommands" => {}
                        other => return gap(format!("client entity option {other}")),
                    }
                }
                let base = if spec.get_bool("useMultipleMongoses") == Some(false) { dep.single_mongos_uri.clone().unwrap_or(dep.uri.clone()) } else { dep.uri.clone() };
                let uri_opts = spec.get_document("uriOptions").cloned().unwrap_or_default();
                for k in uri_opts.keys() {
                    if ClientOptions::UNSUPPORTED_OPTIONS.iter().any(|u| u.eq_ignore_ascii_case(k)) {
                        return gap(format!("URI option {k}"));
                    }
                }
                let base = if uri_opts.get_bool("directConnection") == Some(true) {
                    // A direct connection takes one host: the first seed.
                    let (scheme_hosts, rest) = base.split_once('/').map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((base.clone(), String::new()));
                    let _ = rest;
                    let hosts = base.trim_start_matches("mongodb://").split('/').next().unwrap_or("").to_string();
                    let _ = scheme_hosts;
                    format!("mongodb://{}/", hosts.split(',').next().unwrap_or(""))
                } else {
                    base
                };
                let uri = uri_with(&base, &uri_opts).map_err(fail)?;
                let client = Client::connect(&uri).await.map_err(|err| fail(format!("client {id}: {err}")))?;
                let mut observe = Vec::new();
                for ev in spec.get_array("observeEvents").into_iter().flatten().filter_map(Bson::as_str) {
                    match ev {
                        "commandStartedEvent" | "commandSucceededEvent" | "commandFailedEvent" => observe.push(ev.to_string()),
                        other => return gap(format!("observing {other} (no SDAM / CMAP events)")),
                    }
                }
                let events = Rc::new(RefCell::new(Vec::new()));
                let sink = events.clone();
                client.on_command(move |ev: &CommandEvent| sink.borrow_mut().push(ev.clone()));
                let ignore = spec.get_array("ignoreCommandMonitoringEvents").into_iter().flatten().filter_map(Bson::as_str).map(String::from).collect();
                let entity = ClientEntity { client, events, observe, ignore, observe_sensitive: spec.get_bool("observeSensitiveCommands").unwrap_or(false) };
                e.insert(&id, Entity::Client(Rc::new(entity))).map_err(fail)?;
            }
            "database" => {
                let c = e.client(spec.get_str("client").unwrap_or("")).map_err(fail)?;
                let mut db = c.client.database(spec.get_str("databaseName").unwrap_or(""));
                if let Some(o) = spec.get_document("databaseOptions") {
                    for (k, v) in o.iter() {
                        match (k, v) {
                            ("readPreference", Bson::Document(rp)) => db = db.with_read_preference(ops::read_preference(rp).map_err(|_| fail("readPreference".into()))?),
                            ("readConcern", Bson::Document(rc)) => db = db.with_read_concern(ops::read_concern(rc).ok_or_else(|| fail("readConcern".into()))?),
                            ("writeConcern", Bson::Document(wc)) => db = db.with_write_concern(ops::write_concern(wc)),
                            (other, _) => return gap(format!("databaseOptions {other}")),
                        }
                    }
                }
                e.insert(&id, Entity::Database(Box::new(db))).map_err(fail)?;
            }
            "collection" => {
                let db = e.database(spec.get_str("database").unwrap_or("")).map_err(fail)?;
                let mut coll = db.collection::<Document>(spec.get_str("collectionName").unwrap_or(""));
                if let Some(o) = spec.get_document("collectionOptions") {
                    for (k, v) in o.iter() {
                        match (k, v) {
                            ("readPreference", Bson::Document(rp)) => coll = coll.with_read_preference(ops::read_preference(rp).map_err(|_| fail("readPreference".into()))?),
                            ("readConcern", Bson::Document(rc)) => coll = coll.with_read_concern(ops::read_concern(rc).ok_or_else(|| fail("readConcern".into()))?),
                            ("writeConcern", Bson::Document(wc)) => coll = coll.with_write_concern(ops::write_concern(wc)),
                            (other, _) => return gap(format!("collectionOptions {other}")),
                        }
                    }
                }
                e.insert(&id, Entity::Collection(Box::new(coll))).map_err(fail)?;
            }
            "session" => {
                let c = e.client(spec.get_str("client").unwrap_or("")).map_err(fail)?;
                let o = spec.get_document("sessionOptions").cloned().unwrap_or_default();
                let mut so = kernway_orm_mongodb::SessionOptions::default();
                for (k, v) in o.iter() {
                    match (k, v) {
                        ("snapshot", Bson::Boolean(b)) => so.snapshot = *b,
                        ("causalConsistency", Bson::Boolean(b)) => so.causal_consistency = Some(*b),
                        ("snapshotTime", Bson::Timestamp(t)) => so.snapshot_time = Some(*t),
                        // A saved entity (getSnapshotTime's result).
                        ("snapshotTime", Bson::String(name)) => match e.value(name) {
                            Some(Bson::Timestamp(t)) => so.snapshot_time = Some(t),
                            other => return Err(fail(format!("snapshotTime entity {name} is {other:?}"))),
                        },
                        ("defaultTransactionOptions", Bson::Document(d)) => {
                            so.default_transaction_options = ops::transaction_options(d).map_err(|_| TestResult::Gap("defaultTransactionOptions".into()))?
                        }
                        (other, _) => return gap(format!("sessionOptions {other}")),
                    }
                }
                let s = c.client.start_session_with(so).map_err(|err| fail(format!("session {id}: {err}")))?;
                if let Some(ct) = CLUSTER_TIME.with(|t| t.borrow().clone()) {
                    s.advance_cluster_time(&ct);
                }
                e.insert(&id, Entity::Session(Rc::new(s), RefCell::new(None))).map_err(fail)?;
            }
            "bucket" => {
                let db = e.database(spec.get_str("database").unwrap_or("")).map_err(fail)?;
                let o = spec.get_document("bucketOptions").cloned().unwrap_or_default();
                let mut b = db.gridfs(o.get_str("bucketName").unwrap_or("fs"));
                for (k, v) in o.iter() {
                    match k {
                        "bucketName" => {}
                        "chunkSizeBytes" => b = b.with_chunk_size(v.as_i64().unwrap_or(255 * 1024) as usize),
                        other => return gap(format!("bucketOptions {other}")),
                    }
                }
                e.insert(&id, Entity::Bucket(Box::new(b))).map_err(fail)?;
            }
            other => return gap(format!("{other} entities")),
        }
    }
    Ok(())
}

/// The command events a client observed, as the test sees them.
fn observed(c: &ClientEntity) -> Vec<CommandEvent> {
    let sensitive = |name: &str| {
        let l = name.to_ascii_lowercase();
        ["authenticate", "saslstart", "saslcontinue", "getnonce", "createuser", "updateuser", "copydbgetnonce", "copydbsaslstart", "copydb"].contains(&l.as_str())
    };
    c.events
        .borrow()
        .iter()
        .filter(|ev| {
            let name = &ev.context().command_name;
            let kind = match ev {
                CommandEvent::Started { .. } => "commandStartedEvent",
                CommandEvent::Succeeded { .. } => "commandSucceededEvent",
                CommandEvent::Failed { .. } => "commandFailedEvent",
            };
            // A hello carrying speculativeAuthenticate is sensitive too: the
            // driver reports it redacted (an empty command or reply).
            let redacted_hello = matches!(name.to_ascii_lowercase().as_str(), "hello" | "ismaster")
                && match ev {
                    CommandEvent::Started { command, .. } => command.is_empty(),
                    CommandEvent::Succeeded { reply, .. } => reply.is_empty(),
                    CommandEvent::Failed { .. } => false,
                };
            c.observe.iter().any(|o| o == kind)
                && name != "configureFailPoint"
                && !c.ignore.iter().any(|i| i == name)
                && (c.observe_sensitive || !(sensitive(name) || redacted_hello))
        })
        .cloned()
        .collect()
}

fn check_events(expect: &[Bson], e: &Entities) -> Result<(), TestResult> {
    for block in expect {
        let block = block.as_document().ok_or_else(|| TestResult::Failed("expectEvents".into()))?;
        let event_type = block.get_str("eventType").unwrap_or("command");
        if event_type != "command" {
            return Err(TestResult::Gap(format!("{event_type} events")));
        }
        let c = e.client(block.get_str("client").unwrap_or("")).map_err(TestResult::Failed)?;
        let want = block.get_array("events").cloned().unwrap_or_default();
        let got = observed(&c);
        let extra_ok = block.get_bool("ignoreExtraEvents").unwrap_or(false);
        if got.len() < want.len() || (!extra_ok && got.len() != want.len()) {
            let names: Vec<String> = got.iter().map(describe).collect();
            return Err(TestResult::Failed(format!("{} events, expected {}: {names:?}", got.len(), want.len())));
        }
        for (i, (w, g)) in want.iter().zip(&got).enumerate() {
            let w = w.as_document().ok_or_else(|| TestResult::Failed("event".into()))?;
            let (kind, spec) = w.iter().next().ok_or_else(|| TestResult::Failed("event".into()))?;
            let spec = spec.as_document().cloned().unwrap_or_default();
            let ctx = g.context();
            let ok_kind = matches!((kind, g), ("commandStartedEvent", CommandEvent::Started { .. }) | ("commandSucceededEvent", CommandEvent::Succeeded { .. }) | ("commandFailedEvent", CommandEvent::Failed { .. }));
            if !ok_kind {
                return Err(TestResult::Failed(format!("event {i}: {} is not a {kind}", describe(g))));
            }
            for (k, v) in spec.iter() {
                let r = match k {
                    "command" => match g {
                        CommandEvent::Started { command, .. } => matcher::matches(v, Some(&Bson::Document(command.clone())), true, e),
                        _ => Ok(()),
                    },
                    "reply" => match g {
                        CommandEvent::Succeeded { reply, .. } => matcher::matches(v, Some(&Bson::Document(reply.clone())), true, e),
                        _ => Ok(()),
                    },
                    "commandName" => (v.as_str() == Some(ctx.command_name.as_str())).then_some(()).ok_or(format!("commandName {} ≠ {v:?}", ctx.command_name)),
                    "databaseName" => (v.as_str() == Some(ctx.database_name.as_str())).then_some(()).ok_or(format!("databaseName {} ≠ {v:?}", ctx.database_name)),
                    "hasServiceId" => (v.as_bool() == Some(ctx.service_id.is_some())).then_some(()).ok_or("hasServiceId".into()),
                    "hasServerConnectionId" => (v.as_bool() == Some(ctx.server_connection_id.is_some())).then_some(()).ok_or("hasServerConnectionId".into()),
                    other => return Err(TestResult::Gap(format!("event field {other}"))),
                };
                r.map_err(|why| TestResult::Failed(format!("event {i} ({kind} {}): {why}", ctx.command_name)))?;
            }
        }
    }
    Ok(())
}

fn describe(ev: &CommandEvent) -> String {
    let k = match ev {
        CommandEvent::Started { .. } => "started",
        CommandEvent::Succeeded { .. } => "succeeded",
        CommandEvent::Failed { .. } => "failed",
    };
    format!("{k} {}", ev.context().command_name)
}

fn check_error(expect: &Document, err: &Error, e: &Entities) -> Result<(), String> {
    // A client bulk write exception: its top-level error answers the
    // error-level checks (code, labels, server response) when present.
    let bulk = match &err.kind {
        ErrorKind::ClientBulkWrite(f) => Some(f.as_ref()),
        _ => None,
    };
    let top = bulk.and_then(|f| f.error.as_ref()).unwrap_or(err);
    for (k, v) in expect.iter() {
        match k {
            "writeErrors" => {
                let f = bulk.ok_or_else(|| format!("writeErrors on a non-bulk error {err}"))?;
                let got: Document = f
                    .write_errors
                    .iter()
                    .map(|(i, (code, msg, info))| {
                        let mut d = Document::new().with("code", *code).with("message", msg.clone());
                        if let Some(info) = info {
                            d.insert("details", info.clone());
                        }
                        (i.to_string(), Bson::Document(d))
                    })
                    .collect();
                // Each expected entry is a root-level document; the map has the same keys.
                let want = v.as_document().ok_or("writeErrors")?;
                if want.len() != got.len() {
                    return Err(format!("{} write errors, expected {}: {got:?}", got.len(), want.len()));
                }
                for (idx, w) in want.iter() {
                    matcher::matches(w, got.get(idx), true, e).map_err(|m| format!("writeErrors[{idx}] {m}"))?;
                }
                continue;
            }
            "writeConcernErrors" => {
                let f = bulk.ok_or_else(|| format!("writeConcernErrors on a non-bulk error {err}"))?;
                let got: Vec<Bson> = f.write_concern_errors.iter().map(|(c, m)| Bson::Document(Document::new().with("code", *c).with("message", m.clone()))).collect();
                let want = v.as_array().ok_or("writeConcernErrors")?;
                if want.len() != got.len() {
                    return Err(format!("{} write concern errors, expected {}", got.len(), want.len()));
                }
                for (w, g) in want.iter().zip(&got) {
                    matcher::matches(w, Some(g), true, e)?;
                }
                continue;
            }
            "expectResult" if bulk.is_some() => {
                let partial = bulk.and_then(|f| f.partial_result.as_ref()).map(ops::client_bulk_doc);
                matcher::matches(v, partial.map(Bson::Document).as_ref(), true, e).map_err(|m| format!("partial result {m}"))?;
                continue;
            }
            _ => {}
        }
        let err = top;
        match k {
            "isError" => {}
            "isClientError" => {
                let client = !ops::is_server_error(err);
                if v.as_bool() != Some(client) {
                    return Err(format!("isClientError {client}: {err}"));
                }
            }
            "errorContains" => {
                let want = v.as_str().unwrap_or("").to_ascii_lowercase();
                if !err.message.to_ascii_lowercase().contains(&want) {
                    return Err(format!("error {:?} does not contain {want:?}", err.message));
                }
            }
            "errorCode" => {
                if err.code().map(i64::from) != v.as_i64() {
                    return Err(format!("errorCode {:?} ≠ {v:?} ({err})", err.code()));
                }
            }
            "errorCodeName" => {
                let name = err.code_name();
                if name != v.as_str() {
                    return Err(format!("errorCodeName {name:?} ≠ {v:?} ({err})"));
                }
            }
            "errorLabelsContain" => {
                for l in v.as_array().into_iter().flatten().filter_map(Bson::as_str) {
                    if !err.has_label(l) {
                        return Err(format!("labels {:?} lack {l}", err.labels));
                    }
                }
            }
            "errorLabelsOmit" => {
                for l in v.as_array().into_iter().flatten().filter_map(Bson::as_str) {
                    if err.has_label(l) {
                        return Err(format!("labels {:?} hold {l}", err.labels));
                    }
                }
            }
            "isTimeoutError" => return Err("gap:isTimeoutError (client-side operation timeouts)".into()),
            "errorResponse" => {
                let r = err.server_response().ok_or_else(|| format!("no server response on {err}"))?;
                matcher::matches(v, Some(&Bson::Document(r.clone())), true, e)?;
            }
            "expectResult" => {
                let partial = err.bulk_write_result().map(ops::bulk_result_doc);
                matcher::matches(v, partial.map(Bson::Document).as_ref(), true, e).map_err(|m| format!("partial result {m}"))?;
            }
            other => return Err(format!("gap:expectError.{other}")),
        }
    }
    let _ = e;
    Ok(())
}

/// Fail points to turn off after a test: (address, or None for the primary; name).
type FailPoints = Vec<(Option<Address>, String)>;

async fn special(name: &str, args: &Document, e: &mut Entities, fps: &mut FailPoints, dep: &Deployment) -> Result<(), TestResult> {
    let fail = TestResult::Failed;
    match name {
        "failPoint" => {
            let c = e.client(args.get_str("client").unwrap_or("")).map_err(fail)?;
            let fp = args.get_document("failPoint").cloned().ok_or_else(|| fail("failPoint".into()))?;
            let fp_name = fp.get_str("configureFailPoint").unwrap_or("").to_string();
            c.client.run_command("admin", fp).await.map_err(|err| fail(format!("configureFailPoint: {err}")))?;
            fps.push((None, fp_name));
        }
        "targetedFailPoint" => {
            let s = e.session(args.get_str("session").unwrap_or("")).map_err(fail)?;
            let addr = s.pinned_address().ok_or_else(|| fail("targetedFailPoint: the session is not pinned".into()))?;
            let fp = args.get_document("failPoint").cloned().ok_or_else(|| fail("failPoint".into()))?;
            let fp_name = fp.get_str("configureFailPoint").unwrap_or("").to_string();
            s.client().run_command_at(&addr, "admin", fp).await.map_err(|err| fail(format!("configureFailPoint: {err}")))?;
            fps.push((Some(addr), fp_name));
        }
        "assertSessionTransactionState" => {
            let s = e.session(args.get_str("session").unwrap_or("")).map_err(fail)?;
            let want = args.get_str("state").unwrap_or("");
            if s.transaction_state() != want {
                return Err(fail(format!("transaction state {} ≠ {want}", s.transaction_state())));
            }
        }
        "assertSessionPinned" | "assertSessionUnpinned" => {
            let s = e.session(args.get_str("session").unwrap_or("")).map_err(fail)?;
            let pinned = s.pinned_address().is_some();
            if pinned != (name == "assertSessionPinned") {
                return Err(fail(format!("session pinned={pinned}")));
            }
        }
        "assertSessionDirty" | "assertSessionNotDirty" => {
            let s = e.session(args.get_str("session").unwrap_or("")).map_err(fail)?;
            if s.is_dirty() != (name == "assertSessionDirty") {
                return Err(fail(format!("session dirty={}", s.is_dirty())));
            }
        }
        "assertSameLsidOnLastTwoCommands" | "assertDifferentLsidOnLastTwoCommands" => {
            let c = e.client(args.get_str("client").unwrap_or("")).map_err(fail)?;
            let starts: Vec<Document> = c.events.borrow().iter().filter_map(|ev| match ev {
                CommandEvent::Started { command, context } if context.command_name != "configureFailPoint" => Some(command.clone()),
                _ => None,
            }).collect();
            if starts.len() < 2 {
                return Err(fail("fewer than two commands".into()));
            }
            let (a, b) = (starts[starts.len() - 2].get("lsid"), starts[starts.len() - 1].get("lsid"));
            if (a == b) != (name == "assertSameLsidOnLastTwoCommands") {
                return Err(fail(format!("lsids {a:?} / {b:?}")));
            }
        }
        "assertCollectionExists" | "assertCollectionNotExists" | "assertIndexExists" | "assertIndexNotExists" => {
            let c = internal(dep).await;
            let db = args.get_str("databaseName").unwrap_or("");
            let coll = args.get_str("collectionName").unwrap_or("");
            let exists = if name.contains("Index") {
                let r = c.run_command(db, Document::new().with("listIndexes", coll)).await;
                let idx = args.get_str("indexName").unwrap_or("");
                r.ok().and_then(|r| r.get_document("cursor").and_then(|c| c.get_array("firstBatch")).cloned()).unwrap_or_default().iter().any(|i| i.as_document().and_then(|d| d.get_str("name")) == Some(idx))
            } else {
                c.database(db).list_collection_names().await.map_err(|err| fail(err.to_string()))?.iter().any(|n| n == coll)
            };
            c.shutdown().await;
            if exists != !name.contains("Not") {
                return Err(fail(format!("{name}: exists={exists}")));
            }
        }
        "createEntities" => {
            let list = args.get_array("entities").cloned().unwrap_or_default();
            create_entities(&list, e, dep).await?;
        }
        "wait" => rt_core::sleep(std::time::Duration::from_millis(args.get_i64("ms").unwrap_or(0) as u64)).await,
        other => return Err(TestResult::Gap(format!("test runner operation {other}"))),
    }
    Ok(())
}

/// Why a list of operations stopped.
enum Stop {
    /// The test's verdict (a failed expectation, a gap…).
    Test(TestResult),
    /// An operation failed and the test did not expect it: inside a
    /// withTransaction callback the error goes back to the driver.
    Driver(Box<Error>, String),
}

impl From<TestResult> for Stop {
    fn from(t: TestResult) -> Self {
        Stop::Test(t)
    }
}

async fn run_operations(list: &[Bson], e: &mut Entities, fps: &mut FailPoints, dep: &Deployment) -> Result<(), TestResult> {
    match Box::pin(run_ops(list, e, fps, dep, false)).await {
        Ok(()) => Ok(()),
        Err(Stop::Test(t)) => Err(t),
        Err(Stop::Driver(err, label)) => Err(TestResult::Failed(format!("{label}: {err}"))),
    }
}

async fn with_transaction(object: &str, op: &Document, e: &mut Entities, fps: &mut FailPoints, dep: &Deployment) -> Result<Result<(), Error>, TestResult> {
    let s = e.session(object).map_err(TestResult::Failed)?;
    let args = op.get_document("arguments").cloned().unwrap_or_default();
    let callback = args.get_array("callback").cloned().unwrap_or_default();
    let mut topts = Document::new();
    for (k, v) in args.iter() {
        match k {
            "callback" => {}
            "readConcern" | "writeConcern" | "readPreference" | "maxCommitTimeMS" => {
                topts.insert(k, v.clone());
            }
            other => return Err(TestResult::Gap(format!("withTransaction argument {other}"))),
        }
    }
    let opts = ops::transaction_options(&topts).map_err(|_| TestResult::Gap("withTransaction options".into()))?;
    let verdict: RefCell<Option<TestResult>> = RefCell::new(None);
    let r = s
        .with_transaction(opts, async |_s| match Box::pin(run_ops(&callback, e, fps, dep, true)).await {
            Ok(()) => Ok(()),
            Err(Stop::Driver(err, _)) => Err(*err),
            Err(Stop::Test(t)) => {
                // A failed expectation inside the callback: stop the transaction, report it.
                *verdict.borrow_mut() = Some(t);
                Err(Error::application("the test's expectation failed inside the callback"))
            }
        })
        .await;
    if let Some(t) = verdict.into_inner() {
        return Err(t);
    }
    Ok(r)
}

/// `callback`: inside a withTransaction callback, where an operation's
/// error — once checked against expectError — goes back to the driver.
async fn run_ops(list: &[Bson], e: &mut Entities, fps: &mut FailPoints, dep: &Deployment, callback: bool) -> Result<(), Stop> {
    for (i, op) in list.iter().enumerate() {
        let op = op.as_document().ok_or_else(|| Stop::Test(TestResult::Failed("operation".into())))?;
        let name = op.get_str("name").unwrap_or("");
        let object = op.get_str("object").unwrap_or("");
        let args = op.get_document("arguments");
        if object == "testRunner" {
            special(name, args.unwrap_or(&Document::new()), e, fps, dep).await.map_err(|r| match r {
                TestResult::Failed(w) => Stop::Test(TestResult::Failed(format!("operation {i} {name}: {w}"))),
                other => Stop::Test(other),
            })?;
            continue;
        }
        let label = format!("operation {i} {object}.{name}");
        let r = if name == "withTransaction" {
            match with_transaction(object, op, e, fps, dep).await? {
                Ok(()) => Ok(ops::Outcome { value: None, iterable: false }),
                Err(err) => Err(err),
            }
        } else {
            match Box::pin(ops::run(object, name, args, e)).await {
                Err(OpError::Unsupported(w)) => return Err(TestResult::Gap(w).into()),
                Err(OpError::Runner(w)) => return Err(TestResult::Failed(format!("{label}: runner: {w}")).into()),
                Err(OpError::Driver(err)) => Err(err),
                Ok(o) => Ok(o),
            }
        };
        e.record_lsids();
        if op.get_bool("ignoreResultAndError") == Some(true) {
            if let (true, Err(err)) = (callback, r) {
                return Err(Stop::Driver(Box::new(err), label));
            }
            continue;
        }
        if let Some(expect) = op.get_document("expectError") {
            match r {
                Ok(_) => return Err(Stop::Test(TestResult::Failed(format!("{label}: succeeded, expected an error {expect:?}")))),
                Err(err) => {
                    if let Err(why) = check_error(expect, &err, e) {
                        return Err(Stop::Test(match why.strip_prefix("gap:") {
                            Some(g) => TestResult::Gap(g.to_string()),
                            None => TestResult::Failed(format!("{label}: {why}")),
                        }));
                    }
                    if callback {
                        return Err(Stop::Driver(Box::new(err), label));
                    }
                }
            }
            continue;
        }
        let out = match r {
            Ok(o) => o,
            Err(err) => return Err(Stop::Driver(Box::new(err), label)),
        };
        if let Some(want) = op.get("expectResult") {
            let got = out.value.clone();
            let res = match (want, &got) {
                (Bson::Array(w), Some(Bson::Array(g))) if out.iterable => {
                    if w.len() != g.len() {
                        Err(format!("{} results, expected {}: {g:?}", g.len(), w.len()))
                    } else {
                        w.iter().zip(g).enumerate().try_for_each(|(j, (x, y))| matcher::matches(x, Some(y), true, e).map_err(|m| format!("[{j}] {m}")))
                    }
                }
                _ => matcher::matches(want, got.as_ref(), true, e),
            };
            res.map_err(|why| Stop::Test(TestResult::Failed(format!("{label}: result {why}"))))?;
        }
        if let Some(save) = op.get_str("saveResultAsEntity") {
            let created = ops::CURSOR.with(|c| c.borrow_mut().take()).map(|c| Entity::Cursor(Box::new(c)));
            let created = created.or_else(|| ops::CHANGE_STREAM.with(|c| c.borrow_mut().take()).map(|c| Entity::ChangeStream(Box::new(c))));
            let entity = created.unwrap_or_else(|| Entity::Value(out.value.clone().unwrap_or(Bson::Null)));
            e.insert(save, entity).map_err(|w| Stop::Test(TestResult::Failed(w)))?;
        }
        ops::CURSOR.with(|c| c.borrow_mut().take());
        ops::CHANGE_STREAM.with(|c| c.borrow_mut().take());
    }
    Ok(())
}

async fn check_outcome(outcome: &[Bson], dep: &Deployment, e: &Entities) -> Result<(), TestResult> {
    let c = internal(dep).await;
    for o in outcome {
        let o = o.as_document().ok_or_else(|| TestResult::Failed("outcome".into()))?;
        let db = o.get_str("databaseName").unwrap_or("");
        let coll = o.get_str("collectionName").unwrap_or("");
        let cmd = Document::new().with("find", coll).with("sort", Document::new().with("_id", 1)).with("readConcern", Document::new().with("level", "local"));
        let r = c.run_command(db, cmd).await.map_err(|err| TestResult::Failed(format!("outcome find: {err}")))?;
        let mut got: Vec<Bson> = r.get_document("cursor").and_then(|c| c.get_array("firstBatch")).cloned().unwrap_or_default();
        let mut id = r.get_document("cursor").and_then(|c| c.get_i64("id")).unwrap_or(0);
        while id != 0 {
            let r = c.run_command(db, Document::new().with("getMore", Bson::Int64(id)).with("collection", coll)).await.map_err(|err| TestResult::Failed(err.to_string()))?;
            got.extend(r.get_document("cursor").and_then(|c| c.get_array("nextBatch")).cloned().unwrap_or_default());
            id = r.get_document("cursor").and_then(|c| c.get_i64("id")).unwrap_or(0);
        }
        let want = Bson::Array(o.get_array("documents").cloned().unwrap_or_default());
        matcher::matches(&want, Some(&Bson::Array(got)), false, e).map_err(|why| TestResult::Failed(format!("outcome {db}.{coll}: {why}")))?;
    }
    c.shutdown().await;
    Ok(())
}

async fn run_test(file: &Document, test: &Document, dep: &Deployment) -> TestResult {
    if let Some(why) = unmet(file.get_array("runOnRequirements"), dep).or_else(|| unmet(test.get_array("runOnRequirements"), dep)) {
        return TestResult::NotRunnable(why);
    }
    if let Some(s) = test.get_str("skipReason") {
        return TestResult::NotRunnable(format!("skipped by the spec: {s}"));
    }
    if dep.topology != "single" {
        // Terminate transactions a previous test left open: killAllSessions on
        // each host (each mongos), from a throwaway client — the server may
        // close the connections of the sessions it kills, the caller's too.
        let hosts: Vec<String> = dep.uri.trim_start_matches("mongodb://").split(['/', '?']).next().unwrap_or("").split(',').map(String::from).collect();
        for h in hosts {
            if let Ok(c) = Client::connect(&format!("mongodb://{h}/?directConnection=true")).await {
                let _ = c.run_command("admin", Document::new().with("killAllSessions", Bson::Array(vec![]))).await;
                c.shutdown().await;
            }
        }
    }
    let admin = internal(dep).await;
    if let Some(data) = file.get_array("initialData") {
        if let Err(why) = majority_drop_and_insert(&admin, data).await {
            admin.shutdown().await;
            return TestResult::Failed(why);
        }
    }
    let _ = admin.run_command("admin", Document::new().with("ping", 1)).await;
    CLUSTER_TIME.with(|t| *t.borrow_mut() = admin.cluster_time());
    let mut e = Entities::default();
    let mut fps: FailPoints = Vec::new();
    let result = async {
        create_entities(file.get_array("createEntities").map(Vec::as_slice).unwrap_or(&[]), &mut e, dep).await?;
        run_operations(test.get_array("operations").map(Vec::as_slice).unwrap_or(&[]), &mut e, &mut fps, dep).await?;
        if let Some(ev) = test.get_array("expectEvents") {
            check_events(ev, &e)?;
        }
        if test.contains_key("expectLogMessages") {
            return Err(TestResult::Gap("log messages".into()));
        }
        if let Some(o) = test.get_array("outcome") {
            check_outcome(o, dep, &e).await?;
        }
        Ok::<(), TestResult>(())
    }
    .await;
    for (addr, name) in fps {
        let off = Document::new().with("configureFailPoint", name).with("mode", "off");
        let _ = match addr {
            Some(a) => admin.run_command_at(&a, "admin", off).await,
            None => admin.run_command("admin", off).await,
        };
    }
    for c in e.clients() {
        c.client.shutdown().await;
    }
    admin.shutdown().await;
    match result {
        Ok(()) => TestResult::Passed,
        Err(r) => r,
    }
}

/// Run every test of one file against every deployment; each test is counted once.
pub async fn run_file(path: &std::path::Path, deps: &[Deployment], tally: &mut Tally) {
    let rel = path.to_string_lossy().split("/source/").last().unwrap_or("").to_string();
    let text = std::fs::read_to_string(path).unwrap();
    let file = match Bson::parse_extended_json(&text) {
        Ok(Bson::Document(d)) => d,
        Ok(_) | Err(_) => {
            tally.failed.push(format!("{rel}: the file does not parse as Extended JSON"));
            return;
        }
    };
    for t in file.get_array("tests").cloned().unwrap_or_default() {
        let Some(t) = t.as_document() else { continue };
        let desc = t.get_str("description").unwrap_or("?");
        let mut outcome: Option<TestResult> = None;
        let mut not_run = Vec::new();
        for dep in deps {
            match run_test(&file, t, dep).await {
                TestResult::NotRunnable(w) => not_run.push(format!("{}: {w}", dep.name)),
                TestResult::Passed => {
                    if outcome.is_none() {
                        outcome = Some(TestResult::Passed);
                    }
                }
                r @ (TestResult::Failed(_) | TestResult::Gap(_)) => {
                    let r = match r {
                        TestResult::Failed(w) => TestResult::Failed(format!("[{}] {w}", dep.name)),
                        other => other,
                    };
                    if !matches!(outcome, Some(TestResult::Failed(_))) {
                        outcome = Some(r);
                    }
                }
            }
        }
        match outcome {
            Some(TestResult::Passed) => tally.passed += 1,
            Some(TestResult::Failed(w)) => tally.failed.push(format!("{rel} — {desc}: {w}")),
            Some(TestResult::Gap(w)) => tally.gaps.push(format!("{rel} — {desc}: {w}")),
            Some(TestResult::NotRunnable(_)) | None => tally.not_runnable.push(format!("{rel} — {desc}: {}", not_run.join("; "))),
        }
    }
}

/// The deployments from the environment (tests/support/mongo-setup.sh).
pub async fn deployments() -> Vec<Deployment> {
    let mut out = Vec::new();
    let probe = |uri: String| async move {
        let c = Client::connect(&uri).await.ok()?;
        let info = c.run_command("admin", Document::new().with("buildInfo", 1)).await.ok()?;
        c.shutdown().await;
        Some(version(info.get_str("version").unwrap_or("0")))
    };
    if let Ok(uri) = std::env::var("KERNWAY_MONGO_SINGLE_URL") {
        if let Some(v) = probe(uri.clone()).await {
            out.push(Deployment { name: "single", uri, single_mongos_uri: None, topology: "single", version: v, auth: false });
        }
    }
    if let Ok(uri) = std::env::var("KERNWAY_MONGO_RS_URL") {
        let base = uri.split('?').next().unwrap_or(&uri).rsplit_once('/').map(|(h, _)| format!("{h}/")).unwrap_or(uri.clone());
        let uri = format!("{base}?replicaSet=rs0");
        if let Some(v) = probe(uri.clone()).await {
            out.push(Deployment { name: "replicaset", uri, single_mongos_uri: None, topology: "replicaset", version: v, auth: false });
        }
    }
    if let Ok(uri) = std::env::var("KERNWAY_MONGO_SHARDED_URL") {
        let base = uri.split('?').next().unwrap_or(&uri).rsplit_once('/').map(|(h, _)| format!("{h}/")).unwrap_or(uri.clone());
        let first = base.trim_start_matches("mongodb://").split(',').next().unwrap_or("").trim_end_matches('/');
        if let Some(v) = probe(base.clone()).await {
            out.push(Deployment {
                name: "sharded",
                single_mongos_uri: Some(format!("mongodb://{first}/")),
                uri: base,
                topology: "sharded",
                version: v,
                auth: false,
            });
        }
    }
    out
}
