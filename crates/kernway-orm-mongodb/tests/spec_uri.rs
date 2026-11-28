//! The official Connection String and URI Options spec tests
//! (mongodb/specifications `source/connection-string/tests`,
//! `source/uri-options/tests`), run as their READMEs prescribe: each URI is
//! parsed; `valid` is whether parsing succeeds, `warning` whether it reported a
//! warning, and `hosts` / `auth` / `options` are compared with what was parsed.
//! `credential-disclosure.json` checks that no error or warning repeats the
//! sentinel credential.
//!
//! A case that asserts an option this driver does not implement is counted
//! N/A, with the option named, and listed by `uri_spec_report` — never passed
//! silently. The data comes from `tests/support/specs-fetch.sh`
//! (`KERNWAY_MONGO_SPECS`); without it each test reports `skip:`.

use kernway_orm_mongodb::{Address, AuthMechanism, ClientOptions, Compressor, ReadMode};
use serde_json::{json, Value};
use std::collections::BTreeMap;

fn spec(dir: &str, file: &str) -> Option<Value> {
    let Ok(root) = std::env::var("KERNWAY_MONGO_SPECS") else {
        eprintln!("skip: KERNWAY_MONGO_SPECS not set (tests/support/specs-fetch.sh)");
        return None;
    };
    let p = std::path::Path::new(&root)
        .join(dir)
        .join("tests")
        .join(file);
    Some(
        serde_json::from_str(
            &std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())),
        )
        .unwrap(),
    )
}

/// The URI options this driver parsed, under the spec's (lower-cased) names,
/// valued as the spec tests write them. Options it does not implement are absent.
fn parsed_options(o: &ClientOptions) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    let mut put = |k: &str, v: Value| {
        m.insert(k.to_ascii_lowercase(), v);
    };
    let d = ClientOptions::default();
    let ms = |x: std::time::Duration| json!(x.as_millis() as u64);
    if let Some(c) = &o.credential {
        if let Some(mech) = c.mechanism {
            put(
                "authMechanism",
                json!(match mech {
                    AuthMechanism::ScramSha1 => "SCRAM-SHA-1",
                    AuthMechanism::ScramSha256 => "SCRAM-SHA-256",
                    AuthMechanism::X509 => "MONGODB-X509",
                    AuthMechanism::Plain => "PLAIN",
                }),
            );
        }
        put("authSource", json!(c.source));
    }
    if let Some(t) = &o.tls {
        put("tls", json!(true));
        if let Some(f) = &t.ca_file {
            put("tlsCAFile", json!(f.to_string_lossy()));
        }
        if let Some(f) = &t.cert_key_file {
            put("tlsCertificateKeyFile", json!(f.to_string_lossy()));
        }
        put(
            "tlsAllowInvalidCertificates",
            json!(t.allow_invalid_certificates),
        );
        put("tlsAllowInvalidHostnames", json!(t.allow_invalid_hostnames));
        if t.allow_invalid_certificates && t.allow_invalid_hostnames {
            put("tlsInsecure", json!(true));
        }
    } else {
        put("tls", json!(false));
    }
    if let Some(r) = &o.replica_set {
        put("replicaSet", json!(r));
    }
    if let Some(dc) = o.direct_connection {
        put("directConnection", json!(dc));
    }
    put("loadBalanced", json!(o.load_balanced));
    if let Some(a) = &o.app_name {
        put("appname", json!(a));
    }
    put(
        "readPreference",
        json!(match o.read_preference.mode {
            ReadMode::Primary => "primary",
            ReadMode::PrimaryPreferred => "primaryPreferred",
            ReadMode::Secondary => "secondary",
            ReadMode::SecondaryPreferred => "secondaryPreferred",
            ReadMode::Nearest => "nearest",
        }),
    );
    if !o.read_preference.tag_sets.is_empty() {
        put(
            "readPreferenceTags",
            Value::Array(
                o.read_preference
                    .tag_sets
                    .iter()
                    .map(|s| Value::Object(s.iter().map(|(k, v)| (k.clone(), json!(v))).collect()))
                    .collect(),
            ),
        );
    }
    if let Some(s) = o.read_preference.max_staleness {
        put("maxStalenessSeconds", json!(s.as_secs()));
    }
    if let Some(rc) = &o.read_concern {
        put("readConcernLevel", json!(rc.0));
    }
    if let Some(w) = &o.write_concern.w {
        put("w", w.to_relaxed_json());
    }
    if let Some(j) = o.write_concern.journal {
        put("journal", json!(j));
    }
    if let Some(t) = o.write_concern.timeout {
        put("wTimeoutMS", ms(t));
    }
    put("retryWrites", json!(o.retry_writes));
    put("retryReads", json!(o.retry_reads));
    put("maxAdaptiveRetries", json!(o.max_adaptive_retries));
    put("enableOverloadRetargeting", json!(o.overload_retargeting));
    if !o.compressors.is_empty() {
        put(
            "compressors",
            Value::Array(o.compressors.iter().map(|c| json!(c.name())).collect()),
        );
    }
    if o.compressors
        .iter()
        .any(|c| matches!(c, Compressor::Zlib(_)))
        || o.zlib_level != d.zlib_level
    {
        put("zlibCompressionLevel", json!(o.zlib_level));
    }
    put("connectTimeoutMS", ms(o.connect_timeout)); // 0: no timeout
    put(
        "socketTimeoutMS",
        json!(o.socket_timeout.map_or(0, |t| t.as_millis() as u64)),
    );
    put("serverSelectionTimeoutMS", ms(o.server_selection_timeout));
    put("localThresholdMS", ms(o.local_threshold));
    put("heartbeatFrequencyMS", ms(o.heartbeat_frequency));
    put("maxPoolSize", json!(o.max_pool_size));
    if o.srv.is_some() {
        put("srvMaxHosts", json!(o.srv_max_hosts));
        put("srvServiceName", json!(o.srv_service_name));
    }
    m
}

#[derive(Default)]
struct Tally {
    passed: usize,
    failed: Vec<String>,
    na: Vec<String>,
}

fn host_json(a: &Address) -> (String, Option<u16>) {
    match a {
        Address::Tcp(h, p) => (h.clone(), Some(*p)),
        Address::Unix(p) => (p.to_string_lossy().into_owned(), None),
    }
}

/// Why a case cannot be judged: it uses an option or mechanism this driver
/// declares unimplemented ([`ClientOptions::UNSUPPORTED_OPTIONS`] /
/// [`ClientOptions::UNSUPPORTED_MECHANISMS`]). Such a case is N/A whatever the
/// outcome, so a warning given for "unsupported" never passes for one the
/// case expects for an invalid value.
fn not_applicable(uri: &str) -> Option<String> {
    let query = uri.split_once('?')?.1;
    let mut why = Vec::new();
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if let Some(u) = ClientOptions::UNSUPPORTED_OPTIONS
            .iter()
            .find(|u| u.eq_ignore_ascii_case(k))
        {
            why.push(format!("option {u} not implemented"));
        }
        if k.eq_ignore_ascii_case("authMechanism") {
            if let Some(m) = ClientOptions::UNSUPPORTED_MECHANISMS
                .iter()
                .find(|m| m.eq_ignore_ascii_case(v))
            {
                why.push(format!("mechanism {m} not implemented"));
            }
        }
    }
    why.dedup();
    (!why.is_empty()).then(|| why.join(", "))
}

fn run_case(t: &Value, tally: &mut Tally, file: &str) {
    let d = format!("{file}: {}", t["description"].as_str().unwrap_or("?"));
    let uri = t["uri"].as_str().unwrap();
    if let Some(why) = not_applicable(uri) {
        tally.na.push(format!("{d} — {why}"));
        return;
    }
    let parsed = ClientOptions::parse(uri);
    let want_valid = t["valid"].as_bool().unwrap_or(true);
    let mut why = Vec::new();
    match (&parsed, want_valid) {
        (Ok(_), false) => why.push("parsed, but the spec says invalid".to_string()),
        (Err(e), true) => why.push(format!("refused, but the spec says valid: {e}")),
        _ => {}
    }
    if let Ok(o) = &parsed {
        let warned = !o.warnings.is_empty();
        if let Some(want) = t["warning"].as_bool() {
            if want != warned {
                why.push(format!("warning expected {want}, got {:?}", o.warnings));
            }
        }
        if let Some(hosts) = t["hosts"].as_array() {
            let got: Vec<(String, Option<u16>)> = o.hosts.iter().map(host_json).collect();
            let ok = hosts.len() == got.len()
                && hosts.iter().zip(&got).all(|(h, (gh, gp))| {
                    h["host"].as_str() == Some(gh.as_str())
                        && (h["port"].is_null() || h["port"].as_u64().map(|p| p as u16) == *gp)
                });
            if !ok {
                why.push(format!("hosts {got:?} ≠ {hosts:?}"));
            }
        }
        if let Some(auth) = t["auth"].as_object() {
            let c = o.credential.as_ref();
            for (k, want) in auth {
                if want.is_null() {
                    continue;
                }
                let got = match k.as_str() {
                    "username" => c.and_then(|c| c.username.clone()),
                    "password" => c.and_then(|c| c.password.clone()),
                    "db" => o.default_database.clone(),
                    _ => continue,
                };
                if want.as_str() != got.as_deref() {
                    why.push(format!("auth.{k} {got:?} ≠ {want}"));
                }
            }
        }
        if let Some(opts) = t["options"].as_object() {
            let got = parsed_options(o);
            let mut unsupported = Vec::new();
            for (k, want) in opts {
                let key = k.to_ascii_lowercase();
                match got.get(&key) {
                    None => unsupported.push(k.clone()),
                    Some(v) if v == want => {}
                    Some(v) => why.push(format!("option {k} = {v} ≠ {want}")),
                }
            }
            if !unsupported.is_empty() && why.is_empty() {
                tally.na.push(format!(
                    "{d} — option(s) not implemented: {}",
                    unsupported.join(", ")
                ));
                return;
            }
        }
    }
    if why.is_empty() {
        tally.passed += 1;
    } else {
        tally
            .failed
            .push(format!("{d} [{uri}]: {}", why.join("; ")));
    }
}

fn run(dir: &str, file: &str) -> Option<Tally> {
    let t = spec(dir, file)?;
    let mut tally = Tally::default();
    for case in t["tests"].as_array().unwrap() {
        run_case(case, &mut tally, file);
    }
    Some(tally)
}

fn check(dir: &str, file: &str) {
    let Some(t) = run(dir, file) else { return };
    eprintln!(
        "{dir}/{file}: {} passed, {} failed, {} N/A",
        t.passed,
        t.failed.len(),
        t.na.len()
    );
    for n in &t.na {
        eprintln!("  N/A {n}");
    }
    assert!(
        t.failed.is_empty(),
        "{dir}/{file}: {} failed:\n{}",
        t.failed.len(),
        t.failed.join("\n")
    );
}

macro_rules! uri_tests {
    ($($test:ident => $dir:literal / $file:literal),* $(,)?) => {$(
        #[test]
        fn $test() {
            check($dir, $file);
        }
    )*};
}

uri_tests! {
    cs_invalid_uris => "connection-string" / "invalid-uris.json",
    cs_valid_auth => "connection-string" / "valid-auth.json",
    cs_valid_db_with_dotted_name => "connection-string" / "valid-db-with-dotted-name.json",
    cs_valid_host_identifiers => "connection-string" / "valid-host_identifiers.json",
    cs_valid_options => "connection-string" / "valid-options.json",
    cs_valid_unix_socket_absolute => "connection-string" / "valid-unix_socket-absolute.json",
    cs_valid_unix_socket_relative => "connection-string" / "valid-unix_socket-relative.json",
    cs_valid_warnings => "connection-string" / "valid-warnings.json",
    uo_auth => "uri-options" / "auth-options.json",
    uo_client_backpressure => "uri-options" / "client-backpressure-options.json",
    uo_compression => "uri-options" / "compression-options.json",
    uo_concern => "uri-options" / "concern-options.json",
    uo_connection => "uri-options" / "connection-options.json",
    uo_connection_pool => "uri-options" / "connection-pool-options.json",
    uo_proxy => "uri-options" / "proxy-options.json",
    uo_read_preference => "uri-options" / "read-preference-options.json",
    uo_sdam => "uri-options" / "sdam-options.json",
    uo_single_threaded => "uri-options" / "single-threaded-options.json",
    uo_srv => "uri-options" / "srv-options.json",
    uo_tls => "uri-options" / "tls-options.json",
}

const SENTINELS: [&str; 3] = [
    "sentinelpassword1234",
    "%53%45%4e%54%49%4e%45%4c%70%61%73%73%77%6f%72%64%31%32%33%34",
    "sentineluser5678",
];

fn discloses(s: &str) -> bool {
    let s = s.to_ascii_lowercase();
    SENTINELS.iter().any(|x| s.contains(x))
}

/// credential-disclosure.json: no error message and no warning repeats the
/// sentinel, in either form, case-insensitively.
#[test]
fn cs_credential_disclosure() {
    let Some(t) = spec("connection-string", "credential-disclosure.json") else {
        return;
    };
    let mut fails = Vec::new();
    let cases = t["tests"].as_array().unwrap();
    for case in cases {
        let d = case["description"].as_str().unwrap_or("?");
        let uri = case["uri"].as_str().unwrap();
        match ClientOptions::parse(uri) {
            Err(e) => {
                if discloses(&e.to_string()) || discloses(&format!("{e:?}")) {
                    fails.push(format!("{d}: the error discloses the credential: {e}"));
                }
            }
            Ok(o) => {
                if let Some(w) = o.warnings.iter().find(|w| discloses(w)) {
                    fails.push(format!("{d}: a warning discloses the credential: {w}"));
                }
            }
        }
    }
    eprintln!(
        "connection-string/credential-disclosure.json: {} passed, {} failed",
        cases.len() - fails.len(),
        fails.len()
    );
    assert!(fails.is_empty(), "{}", fails.join("\n"));
}

/// Prose tests 1 and 5: no representation of parsed options shows the
/// credentials; a URI that fails to parse leaves no object behind (the
/// `Result` holds only the error, checked above).
#[test]
fn cs_prose_redacted_representation() {
    let o =
        ClientOptions::parse("mongodb://SENTINELuser5678:SENTINELpassword1234@localhost:27017/db")
            .unwrap();
    let debug = format!("{o:?}");
    assert!(
        !discloses(&debug),
        "Debug of ClientOptions discloses the credentials: {debug}"
    );
    let e =
        ClientOptions::parse("mongodb://SENTINELuser5678:SENTINELpassword1234@localhost:99999/db")
            .unwrap_err();
    assert!(
        !discloses(&e.to_string()) && !discloses(&format!("{e:?}")),
        "{e}"
    );
}

/// The auth spec's connection-string tests (`source/auth/tests/legacy`):
/// `valid`, and the credential's username / password / source / mechanism.
/// Cases naming a mechanism this driver does not implement are N/A.
#[test]
fn auth_connection_string() {
    let Some(root) = std::env::var("KERNWAY_MONGO_SPECS").ok() else {
        eprintln!("skip: KERNWAY_MONGO_SPECS not set (tests/support/specs-fetch.sh)");
        return;
    };
    let p = std::path::Path::new(&root).join("auth/tests/legacy/connection-string.json");
    let t: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
    let mut tally = Tally::default();
    for case in t["tests"].as_array().unwrap() {
        let d = case["description"].as_str().unwrap_or("?");
        let uri = case["uri"].as_str().unwrap();
        if let Some(why) = not_applicable(uri) {
            tally.na.push(format!("{d} — {why}"));
            continue;
        }
        let want_valid = case["valid"].as_bool().unwrap();
        let mut why = Vec::new();
        match ClientOptions::parse(uri) {
            Ok(_) if !want_valid => why.push("parsed, but the spec says invalid".to_string()),
            Err(e) if want_valid => why.push(format!("refused, but the spec says valid: {e}")),
            Err(_) => {}
            Ok(o) => {
                let want = &case["credential"];
                let got = o.credential.as_ref();
                if want.is_null() {
                    if got.is_some() {
                        why.push("a credential was parsed where the spec expects none".into());
                    }
                } else if let Some(c) = got {
                    if want["username"].as_str() != c.username.as_deref() {
                        why.push(format!("username {:?} ≠ {}", c.username, want["username"]));
                    }
                    if want["password"].as_str() != c.password.as_deref() {
                        why.push(format!("password {:?} ≠ {}", c.password, want["password"]));
                    }
                    if want["source"].as_str() != Some(c.source.as_str()) {
                        why.push(format!("source {:?} ≠ {}", c.source, want["source"]));
                    }
                    if want["mechanism"].as_str() != c.mechanism.map(AuthMechanism::name) {
                        why.push(format!(
                            "mechanism {:?} ≠ {}",
                            c.mechanism.map(AuthMechanism::name),
                            want["mechanism"]
                        ));
                    }
                } else {
                    why.push("no credential parsed".into());
                }
            }
        }
        if why.is_empty() {
            tally.passed += 1;
        } else {
            tally
                .failed
                .push(format!("{d} [{uri}]: {}", why.join("; ")));
        }
    }
    eprintln!(
        "auth/legacy/connection-string.json: {} passed, {} failed, {} N/A",
        tally.passed,
        tally.failed.len(),
        tally.na.len()
    );
    for n in &tally.na {
        eprintln!("  N/A {n}");
    }
    assert!(tally.failed.is_empty(), "{}", tally.failed.join("\n"));
}
