//! The official Server Selection and Max Staleness tests
//! (mongodb/specifications `source/server-selection/tests` — `server_selection`,
//! `rtt`, `in_window` — and `source/max-staleness/tests`), run as their
//! README prescribes: build the described topology, select for the operation
//! and read preference, and compare the suitable servers and the latency
//! window (or the expected error); check the RTT average; and check the
//! operationCount-based choice frequencies within the window.
//!
//! The data comes from `tests/support/specs-fetch.sh` (`KERNWAY_MONGO_SPECS`);
//! without it each test reports `skip:`.

use kernway_orm_mongodb::topology::{choose_in_window, Selection, ServerDescription, ServerType, Topology, TopologyType};
use kernway_orm_mongodb::{Address, Document, ReadMode, ReadPreference};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

fn root() -> Option<std::path::PathBuf> {
    match std::env::var("KERNWAY_MONGO_SPECS") {
        Ok(d) => Some(std::path::PathBuf::from(d)),
        Err(_) => {
            eprintln!("skip: KERNWAY_MONGO_SPECS not set (tests/support/specs-fetch.sh)");
            None
        }
    }
}

fn json_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(json_files(&p));
        } else if p.extension().is_some_and(|x| x == "json") {
            out.push(p);
        }
    }
    out.sort();
    out
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

fn number(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.get("$numberLong").and_then(Value::as_str).and_then(|s| s.parse().ok()))
}

/// The topology a test describes. `anchor` is time zero for `lastUpdateTime`.
fn topology(td: &Value, anchor: Instant) -> Topology {
    let kind = topology_type(td["type"].as_str().unwrap());
    let servers: Vec<&Value> = td["servers"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    let addrs: Vec<Address> = servers.iter().map(|s| Address::parse(s["address"].as_str().unwrap()).unwrap()).collect();
    let mut t = Topology::new(&addrs, None, None, false);
    t.kind = kind;
    for (s, a) in servers.iter().zip(addrs) {
        let mut d = ServerDescription::unknown(a.clone(), None);
        d.kind = server_type(s["type"].as_str().unwrap());
        d.rtt = s["avg_rtt_ms"].as_f64().map(|ms| Duration::from_secs_f64(ms / 1000.0));
        d.tags = s["tags"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect()).unwrap_or_default();
        d.max_wire_version = s["maxWireVersion"].as_i64().unwrap_or(21);
        d.min_wire_version = 0;
        d.last_write = number(&s["lastWrite"]["lastWriteDate"]);
        d.last_update = anchor + Duration::from_millis(number(&s["lastUpdateTime"]).unwrap_or(0) as u64);
        d.logical_session_timeout = Some(30);
        t.servers.insert(a, d);
    }
    t
}

fn read_preference(v: &Value) -> ReadPreference {
    let mode = match v["mode"].as_str().unwrap_or("Primary") {
        "Primary" | "primary" => ReadMode::Primary,
        "PrimaryPreferred" | "primaryPreferred" => ReadMode::PrimaryPreferred,
        "Secondary" | "secondary" => ReadMode::Secondary,
        "SecondaryPreferred" | "secondaryPreferred" => ReadMode::SecondaryPreferred,
        "Nearest" | "nearest" => ReadMode::Nearest,
        other => panic!("mode {other}"),
    };
    let mut rp = ReadPreference::new(mode);
    rp.tag_sets = v["tag_sets"]
        .as_array()
        .map(|sets| {
            sets.iter().map(|set| set.as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect()).collect()
        })
        .unwrap_or_default();
    // [{}] (match anything) is the same as no tag sets.
    if rp.tag_sets.iter().all(Vec::is_empty) {
        rp.tag_sets.clear();
    }
    // -1 (or absent) is no maximum; 0 is a value, and an invalid one.
    rp.max_staleness = v["maxStalenessSeconds"].as_i64().filter(|s| *s >= 0).map(|s| Duration::from_secs(s as u64));
    rp
}

fn addresses(v: &Value) -> BTreeSet<String> {
    v.as_array().into_iter().flatten().map(|s| Address::parse(s["address"].as_str().unwrap()).unwrap().to_string()).collect()
}

/// One selection test; `None` if it passed, else why not.
fn selection_case(t: &Value) -> Option<String> {
    let anchor = Instant::now();
    let topo = topology(&t["topology_description"], anchor);
    let heartbeat = Duration::from_millis(t["heartbeatFrequencyMS"].as_u64().unwrap_or(10_000));
    let rp = read_preference(&t["read_preference"]);
    let sel = match t["operation"].as_str().unwrap_or("read") {
        "write" => Selection::Write,
        _ => Selection::Read(rp.clone()),
    };
    let error = match &sel {
        Selection::Read(rp) => topo.read_preference_error(rp, heartbeat),
        _ => None,
    };
    let primary_with_tags = matches!(&sel, Selection::Read(rp) if rp.mode == ReadMode::Primary && !rp.tag_sets.is_empty());
    if t["error"].as_bool() == Some(true) {
        return (error.is_none() && !primary_with_tags).then(|| "selected, but the spec expects an error".to_string());
    }
    if let Some(e) = error {
        return Some(format!("error {e:?}, but the spec expects a selection"));
    }
    let deprioritized: Vec<Address> = t["deprioritized_servers"].as_array().into_iter().flatten().map(|s| Address::parse(s["address"].as_str().unwrap()).unwrap()).collect();
    let mut why = Vec::new();
    if t.get("suitable_servers").is_some() {
        let mut suitable: BTreeSet<String> = topo.suitable(&sel, heartbeat, &deprioritized).iter().map(|s| s.address.to_string()).collect();
        if suitable.is_empty() && !deprioritized.is_empty() {
            suitable = topo.suitable(&sel, heartbeat, &[]).iter().map(|s| s.address.to_string()).collect();
        }
        let want = addresses(&t["suitable_servers"]);
        if suitable != want {
            why.push(format!("suitable {suitable:?} ≠ {want:?}"));
        }
    }
    let window: BTreeSet<String> = topo.select_avoiding(&sel, Duration::from_millis(15), heartbeat, &deprioritized).iter().map(Address::to_string).collect();
    let want = addresses(&t["in_latency_window"]);
    if window != want {
        why.push(format!("in_latency_window {window:?} ≠ {want:?}"));
    }
    (!why.is_empty()).then(|| why.join("; "))
}

fn run(dir: &std::path::Path, label: &str, case: impl Fn(&Value) -> Option<String>) {
    let files = json_files(dir);
    let mut fails = Vec::new();
    for f in &files {
        let t: Value = serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap();
        if let Some(why) = case(&t) {
            fails.push(format!("{}: {why}", f.strip_prefix(dir).unwrap().display()));
        }
    }
    eprintln!("{label}: {} files, {} passed, {} failed", files.len(), files.len() - fails.len(), fails.len());
    assert!(fails.is_empty(), "{label}: {} failed:\n{}", fails.len(), fails.join("\n"));
}

#[test]
fn server_selection_logic() {
    let Some(r) = root() else { return };
    run(&r.join("server-selection/tests/server_selection"), "server-selection/server_selection", selection_case);
}

#[test]
fn max_staleness() {
    let Some(r) = root() else { return };
    run(&r.join("max-staleness/tests"), "max-staleness", selection_case);
}

/// rtt/: the exponentially weighted average (α = 0.2), from a first value.
#[test]
fn server_selection_rtt() {
    let Some(r) = root() else { return };
    run(&r.join("server-selection/tests/rtt"), "server-selection/rtt", |t| {
        let a = Address::parse("a:27017").unwrap();
        let mut topo = Topology::new(std::slice::from_ref(&a), None, Some(true), false);
        let hello = Document::new().with("ok", 1).with("isWritablePrimary", true).with("maxWireVersion", 21);
        if let Some(ms) = t["avg_rtt_ms"].as_f64() {
            topo.apply(ServerDescription::from_hello(a.clone(), &hello, Duration::from_secs_f64(ms / 1000.0)));
        }
        let new = t["new_rtt_ms"].as_f64().unwrap();
        topo.apply(ServerDescription::from_hello(a.clone(), &hello, Duration::from_secs_f64(new / 1000.0)));
        let got = topo.servers[&a].rtt.unwrap().as_secs_f64() * 1000.0;
        let want = t["new_avg_rtt"].as_f64().unwrap();
        ((got - want).abs() > 1e-6).then(|| format!("average {got} ≠ {want}"))
    });
}

/// in_window/: two random choices in the window, the one with fewer
/// in-flight operations wins; observed frequencies within the tolerance.
#[test]
fn server_selection_in_window() {
    let Some(r) = root() else { return };
    run(&r.join("server-selection/tests/in_window"), "server-selection/in_window", |t| {
        let topo = topology(&t["topology_description"], Instant::now());
        let counts: BTreeMap<String, u64> = t["mocked_topology_state"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (Address::parse(s["address"].as_str().unwrap()).unwrap().to_string(), s["operation_count"].as_u64().unwrap()))
            .collect();
        let window = topo.select(&Selection::Read(ReadPreference::new(ReadMode::Nearest)), Duration::from_millis(15), Duration::from_secs(10));
        let iterations = t["iterations"].as_u64().unwrap();
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut random = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let mut picked: BTreeMap<String, u64> = BTreeMap::new();
        for _ in 0..iterations {
            let a = choose_in_window(&window, |a| counts.get(&a.to_string()).copied().unwrap_or(0), &mut random).unwrap();
            *picked.entry(a.to_string()).or_default() += 1;
        }
        let tolerance = t["outcome"]["tolerance"].as_f64().unwrap();
        let mut why = Vec::new();
        for (a, want) in t["outcome"]["expected_frequencies"].as_object().unwrap() {
            let a = Address::parse(a).unwrap().to_string();
            let want = want.as_f64().unwrap();
            let got = *picked.get(&a).unwrap_or(&0) as f64 / iterations as f64;
            let ok = if want == 0.0 || want == 1.0 { got == want } else { (got - want).abs() <= tolerance };
            if !ok {
                why.push(format!("{a}: {got:.3} ≠ {want} ± {tolerance}"));
            }
        }
        (!why.is_empty()).then(|| why.join("; "))
    });
}

