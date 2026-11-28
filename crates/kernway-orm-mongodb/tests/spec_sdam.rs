//! The official Server Discovery and Monitoring tests
//! (mongodb/specifications `source/server-discovery-and-monitoring/tests`:
//! `single`, `rs`, `sharded`, `load-balanced`, `errors`), run as their README
//! prescribes: build the topology from the URI, feed each phase's hello
//! responses (an empty one is a network error) and application errors, then
//! compare the topology with the phase's outcome — type, set name, every
//! server (type, set name, setVersion, electionId, wire versions,
//! topologyVersion, error text, pool generation), maxSetVersion,
//! maxElectionId, logicalSessionTimeoutMinutes and compatibility.
//!
//! The data comes from `tests/support/specs-fetch.sh` (`KERNWAY_MONGO_SPECS`);
//! without it each test reports `skip:`.

use kernway_orm_mongodb::topology::{
    ApplicationError, ApplicationErrorKind, ServerDescription, ServerType, Topology, TopologyType, TopologyVersion,
};
use kernway_orm_mongodb::{Address, Bson, ClientOptions, Document, ObjectId};
use serde_json::Value;
use std::time::Duration;

fn dir(sub: &str) -> Option<std::path::PathBuf> {
    match std::env::var("KERNWAY_MONGO_SPECS") {
        Ok(d) => Some(std::path::Path::new(&d).join("server-discovery-and-monitoring/tests").join(sub)),
        Err(_) => {
            eprintln!("skip: KERNWAY_MONGO_SPECS not set (tests/support/specs-fetch.sh)");
            None
        }
    }
}

fn doc(v: &Value) -> Document {
    match Bson::parse_extended_json(&v.to_string()).expect("extended JSON") {
        Bson::Document(d) => d,
        other => panic!("not a document: {other:?}"),
    }
}

fn server_type(name: &str) -> ServerType {
    match name {
        "Unknown" | "PossiblePrimary" => ServerType::Unknown,
        "Standalone" => ServerType::Standalone,
        "Mongos" => ServerType::Mongos,
        "RSPrimary" => ServerType::RsPrimary,
        "RSSecondary" => ServerType::RsSecondary,
        "RSArbiter" => ServerType::RsArbiter,
        "RSOther" => ServerType::RsOther,
        "RSGhost" => ServerType::RsGhost,
        "LoadBalancer" => ServerType::LoadBalancer,
        other => panic!("server type {other}"),
    }
}

fn topology_type(name: &str) -> TopologyType {
    match name {
        "Unknown" => TopologyType::Unknown,
        "Single" => TopologyType::Single,
        "ReplicaSetNoPrimary" => TopologyType::ReplicaSetNoPrimary,
        "ReplicaSetWithPrimary" => TopologyType::ReplicaSetWithPrimary,
        "Sharded" => TopologyType::Sharded,
        "LoadBalanced" => TopologyType::LoadBalanced,
        other => panic!("topology type {other}"),
    }
}

fn oid(v: &Value) -> Option<ObjectId> {
    v.get("$oid").and_then(Value::as_str).and_then(ObjectId::parse_str)
}

fn tv(v: &Value) -> Option<TopologyVersion> {
    if v.is_null() {
        return None;
    }
    let d = Document::new().with("topologyVersion", Bson::Document(doc(v)));
    TopologyVersion::from_reply(&d)
}

fn apply_error(t: &mut Topology, e: &Value) {
    let address = Address::parse(e["address"].as_str().unwrap()).unwrap();
    let kind = match e["type"].as_str().unwrap() {
        "network" => ApplicationErrorKind::Network,
        "timeout" => ApplicationErrorKind::Timeout,
        "command" => {
            let r = doc(&e["response"]);
            // The error is the reply's, or else its write-concern error's;
            // writeErrors are never checked.
            let (code, message) = if r.get_bool("ok") == Some(false) || r.get("ok").and_then(Bson::as_f64) == Some(0.0) {
                (r.get_i64("code").map(|c| c as i32), r.get_str("errmsg").unwrap_or("").to_string())
            } else if let Some(w) = r.get_document("writeConcernError") {
                (w.get_i64("code").map(|c| c as i32), w.get_str("errmsg").unwrap_or("").to_string())
            } else {
                (None, String::new())
            };
            let labels = r.get_array("errorLabels").map(|a| a.iter().filter_map(Bson::as_str).map(String::from).collect()).unwrap_or_default();
            ApplicationErrorKind::Command { code, message, topology_version: TopologyVersion::from_reply(&r), labels }
        }
        other => panic!("error type {other}"),
    };
    let err = ApplicationError {
        address,
        generation: e["generation"].as_u64(),
        completed_handshake: e["when"].as_str() == Some("afterHandshakeCompletes"),
        kind,
    };
    t.handle_application_error(&err);
}

fn check_outcome(t: &Topology, o: &Value, why: &mut Vec<String>) {
    if let Some(k) = o["topologyType"].as_str() {
        if t.kind != topology_type(k) {
            why.push(format!("topologyType {:?} ≠ {k}", t.kind));
        }
    }
    if o.get("setName").is_some() && o["setName"].as_str() != t.set_name.as_deref() {
        why.push(format!("setName {:?} ≠ {}", t.set_name, o["setName"]));
    }
    if o.get("logicalSessionTimeoutMinutes").is_some() && o["logicalSessionTimeoutMinutes"].as_i64() != t.session_timeout {
        why.push(format!("logicalSessionTimeoutMinutes {:?} ≠ {}", t.session_timeout, o["logicalSessionTimeoutMinutes"]));
    }
    if o.get("maxSetVersion").is_some() && o["maxSetVersion"].as_i64() != t.max_set_version() {
        why.push(format!("maxSetVersion {:?} ≠ {}", t.max_set_version(), o["maxSetVersion"]));
    }
    if o.get("maxElectionId").is_some() && oid(&o["maxElectionId"]) != t.max_election_id() {
        why.push(format!("maxElectionId {:?} ≠ {}", t.max_election_id().map(|o| o.to_hex()), o["maxElectionId"]));
    }
    if let Some(c) = o["compatible"].as_bool() {
        if c != t.compatible() {
            why.push(format!("compatible {} ≠ {c}", t.compatible()));
        }
    }
    let Some(servers) = o["servers"].as_object() else { return };
    let want: Vec<Address> = servers.keys().map(|a| Address::parse(a).unwrap()).collect();
    let got: Vec<&Address> = t.servers.keys().collect();
    if got.len() != want.len() || want.iter().any(|a| !t.servers.contains_key(a)) {
        why.push(format!("servers {got:?} ≠ {want:?}"));
        return;
    }
    for (a, s) in servers {
        let addr = Address::parse(a).unwrap();
        let d: &ServerDescription = &t.servers[&addr];
        if let Some(k) = s["type"].as_str() {
            if d.kind != server_type(k) {
                why.push(format!("{a}: type {:?} ≠ {k}", d.kind));
            }
        }
        if s.get("setName").is_some() && s["setName"].as_str() != d.set_name.as_deref() {
            why.push(format!("{a}: setName {:?} ≠ {}", d.set_name, s["setName"]));
        }
        if s.get("setVersion").is_some() && s["setVersion"].as_i64() != d.set_version {
            why.push(format!("{a}: setVersion {:?} ≠ {}", d.set_version, s["setVersion"]));
        }
        if s.get("electionId").is_some() && oid(&s["electionId"]) != d.election_id {
            why.push(format!("{a}: electionId {:?} ≠ {}", d.election_id.map(|o| o.to_hex()), s["electionId"]));
        }
        if s.get("logicalSessionTimeoutMinutes").is_some() && s["logicalSessionTimeoutMinutes"].as_i64() != d.logical_session_timeout {
            why.push(format!("{a}: logicalSessionTimeoutMinutes {:?} ≠ {}", d.logical_session_timeout, s["logicalSessionTimeoutMinutes"]));
        }
        if let Some(w) = s["minWireVersion"].as_i64() {
            if w != d.min_wire_version {
                why.push(format!("{a}: minWireVersion {} ≠ {w}", d.min_wire_version));
            }
        }
        if let Some(w) = s["maxWireVersion"].as_i64() {
            if w != d.max_wire_version {
                why.push(format!("{a}: maxWireVersion {} ≠ {w}", d.max_wire_version));
            }
        }
        if s.get("topologyVersion").is_some() && tv(&s["topologyVersion"]) != d.topology_version {
            why.push(format!("{a}: topologyVersion {:?} ≠ {}", d.topology_version, s["topologyVersion"]));
        }
        if let Some(e) = s["error"].as_str() {
            if !d.error.as_deref().is_some_and(|got| got.contains(e)) {
                why.push(format!("{a}: error {:?} does not contain {e:?}", d.error));
            }
        }
        if let Some(g) = s["pool"]["generation"].as_u64() {
            if t.pool_generation(&addr) != g {
                why.push(format!("{a}: pool generation {} ≠ {g}", t.pool_generation(&addr)));
            }
        }
    }
}

/// Run one test file; the failures, one line each.
fn run_file(path: &std::path::Path) -> Vec<String> {
    let t: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let o = ClientOptions::parse(t["uri"].as_str().unwrap()).unwrap_or_else(|e| panic!("{name}: {e}"));
    let mut topo = Topology::new(&o.hosts, o.replica_set.clone(), o.direct_connection, o.load_balanced);
    let mut fails = Vec::new();
    for (i, phase) in t["phases"].as_array().unwrap().iter().enumerate() {
        for r in phase["responses"].as_array().into_iter().flatten() {
            let addr = Address::parse(r[0].as_str().unwrap()).unwrap();
            let hello = &r[1];
            if hello.as_object().is_some_and(|m| m.is_empty()) {
                topo.check_failed(&addr, "network error".into());
            } else {
                topo.apply(ServerDescription::from_hello(addr, &doc(hello), Duration::ZERO));
            }
        }
        for e in phase["applicationErrors"].as_array().into_iter().flatten() {
            apply_error(&mut topo, e);
        }
        let mut why = Vec::new();
        check_outcome(&topo, &phase["outcome"], &mut why);
        if !why.is_empty() {
            let d = phase["description"].as_str().unwrap_or("");
            fails.push(format!("{name} phase {} ({d}): {}", i + 1, why.join("; ")));
        }
    }
    fails
}

fn run_dir(sub: &str) {
    let Some(d) = dir(sub) else { return };
    let mut files: Vec<_> = std::fs::read_dir(&d).unwrap().filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    files.sort();
    let mut fails = Vec::new();
    for f in &files {
        fails.extend(run_file(f));
    }
    let failed_files = files.iter().filter(|f| fails.iter().any(|x| x.starts_with(&*f.file_name().unwrap().to_string_lossy()))).count();
    eprintln!("sdam/{sub}: {} files, {} passed, {} failed", files.len(), files.len() - failed_files, failed_files);
    assert!(fails.is_empty(), "sdam/{sub}: {} phase(s) failed:\n{}", fails.len(), fails.join("\n"));
}

#[test]
fn sdam_single() {
    run_dir("single");
}

#[test]
fn sdam_rs() {
    run_dir("rs");
}

#[test]
fn sdam_sharded() {
    run_dir("sharded");
}

#[test]
fn sdam_load_balanced() {
    run_dir("load-balanced");
}

#[test]
fn sdam_errors() {
    run_dir("errors");
}
