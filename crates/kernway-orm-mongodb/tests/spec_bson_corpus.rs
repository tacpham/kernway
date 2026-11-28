//! The official BSON corpus (mongodb/specifications `source/bson-corpus`), run as
//! its README's "Testing validity / decode errors / parsing errors" prescribe for a
//! codec with a native representation. The data is fetched by
//! `tests/support/specs-fetch.sh` (pinned commit), which prints
//! `KERNWAY_MONGO_SPECS`; without it each test reports `skip:` (a failure in CI and
//! the acceptance run).
//!
//! JSON is compared as the README asks ("normalize JSON comparisons however
//! necessary"): objects unordered, and doubles — `$numberDouble` strings and relaxed
//! JSON numbers — by value, since "string representation of doubles is fairly
//! unportable".

use kernway_orm_mongodb::{Bson, Decimal128, Document, Regex};
use serde_json::Value;

fn spec_dir() -> Option<std::path::PathBuf> {
    match std::env::var("KERNWAY_MONGO_SPECS") {
        Ok(d) => Some(std::path::Path::new(&d).join("bson-corpus").join("tests")),
        Err(_) => {
            eprintln!("skip: KERNWAY_MONGO_SPECS not set (tests/support/specs-fetch.sh)");
            None
        }
    }
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

/// Encode; a failure yields no bytes, which never equals the expected cB.
fn enc(d: &Document) -> Vec<u8> {
    d.to_bytes().unwrap_or_default()
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// A double's identity for comparison: -0.0 and NaN kept distinct and equal to themselves.
fn double_key(f: f64) -> String {
    if f.is_nan() {
        "NaN".into()
    } else if f == 0.0 && f.is_sign_negative() {
        "-0".into()
    } else {
        format!("{f:e}")
    }
}

fn norm(v: &Value) -> Value {
    match v {
        Value::Object(m) if m.len() == 1 && m.contains_key("$numberDouble") => {
            let s = m["$numberDouble"].as_str().unwrap_or("");
            let f = match s {
                "Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                "NaN" => f64::NAN,
                s => s.parse().unwrap_or(f64::NAN),
            };
            serde_json::json!({ "$numberDouble": double_key(f) })
        }
        Value::Object(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), norm(v))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(norm).collect()),
        // Relaxed JSON: an integer literal stays an integer; a fractional one is a double.
        Value::Number(n) if n.is_f64() => {
            Value::String(format!("double:{}", double_key(n.as_f64().unwrap())))
        }
        other => other.clone(),
    }
}

fn json(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|e| panic!("the corpus JSON {s:?} does not parse: {e}"))
}

fn doc(b: Bson) -> Result<Document, String> {
    match b {
        Bson::Document(d) => Ok(d),
        other => Err(format!("not a document: {other:?}")),
    }
}

/// Run one corpus file; returns the failures (empty: all assertions held) and the
/// number of assertions checked.
fn run_file(name: &str) -> (Vec<String>, usize) {
    let Some(dir) = spec_dir() else {
        return (Vec::new(), 0);
    };
    let text = std::fs::read_to_string(dir.join(format!("{name}.json"))).expect("the corpus file");
    let t: Value = json(&text);
    let mut fails = Vec::new();
    let mut checked = 0usize;
    let mut check = |ok: bool, what: String| {
        checked += 1;
        if !ok {
            fails.push(what);
        }
    };
    for case in t["valid"].as_array().into_iter().flatten() {
        let d = case["description"].as_str().unwrap_or("?");
        let lossy = case["lossy"].as_bool().unwrap_or(false);
        let cb = hex(case["canonical_bson"].as_str().expect("canonical_bson"));
        let cej = json(
            case["canonical_extjson"]
                .as_str()
                .expect("canonical_extjson"),
        );
        let rej = case["relaxed_extjson"].as_str().map(json);
        match Document::from_bytes(&cb) {
            Err(e) => check(false, format!("{d}: cB does not decode: {e}")),
            Ok(native) => {
                check(
                    enc(&native) == cb,
                    format!(
                        "{d}: native_to_bson(bson_to_native(cB)) = {} ≠ cB",
                        to_hex(&enc(&native))
                    ),
                );
                let got = Bson::Document(native.clone()).to_canonical_json();
                check(
                    norm(&got) == norm(&cej),
                    format!("{d}: bson→cEJ = {got} ≠ {cej}"),
                );
                if let Some(rej) = &rej {
                    let got = Bson::Document(native).to_relaxed_json();
                    check(
                        norm(&got) == norm(rej),
                        format!("{d}: bson→rEJ = {got} ≠ {rej}"),
                    );
                }
            }
        }
        let cej_text = case["canonical_extjson"].as_str().unwrap();
        match Bson::parse_extended_json(cej_text)
            .map_err(|e| e.to_string())
            .and_then(doc)
        {
            Err(e) => check(false, format!("{d}: cEJ does not parse: {e}")),
            Ok(native) => {
                let got = Bson::Document(native.clone()).to_canonical_json();
                check(
                    norm(&got) == norm(&cej),
                    format!("{d}: cEJ→cEJ = {got} ≠ {cej}"),
                );
                if !lossy {
                    check(
                        enc(&native) == cb,
                        format!("{d}: cEJ→bson = {} ≠ cB", to_hex(&enc(&native))),
                    );
                }
            }
        }
        if let Some(db) = case["degenerate_bson"].as_str() {
            match Document::from_bytes(&hex(db)) {
                Err(e) => check(false, format!("{d}: dB does not decode: {e}")),
                Ok(native) => check(
                    enc(&native) == cb,
                    format!("{d}: dB→bson = {} ≠ cB", to_hex(&enc(&native))),
                ),
            }
        }
        if let Some(dej) = case["degenerate_extjson"].as_str() {
            match Bson::parse_extended_json(dej)
                .map_err(|e| e.to_string())
                .and_then(doc)
            {
                Err(e) => check(false, format!("{d}: dEJ does not parse: {e}")),
                Ok(native) => {
                    let got = Bson::Document(native.clone()).to_canonical_json();
                    check(
                        norm(&got) == norm(&cej),
                        format!("{d}: dEJ→cEJ = {got} ≠ {cej}"),
                    );
                    if !lossy {
                        check(enc(&native) == cb, format!("{d}: dEJ→bson ≠ cB"));
                    }
                }
            }
        }
        if let (Some(rej), Some(text)) = (&rej, case["relaxed_extjson"].as_str()) {
            match Bson::parse_extended_json(text)
                .map_err(|e| e.to_string())
                .and_then(doc)
            {
                Err(e) => check(false, format!("{d}: rEJ does not parse: {e}")),
                Ok(native) => {
                    let got = Bson::Document(native).to_relaxed_json();
                    check(
                        norm(&got) == norm(rej),
                        format!("{d}: rEJ→rEJ = {got} ≠ {rej}"),
                    );
                }
            }
        }
    }
    for case in t["decodeErrors"].as_array().into_iter().flatten() {
        let d = case["description"].as_str().unwrap_or("?");
        let b = hex(case["bson"].as_str().expect("bson"));
        check(
            Document::from_bytes(&b).is_err(),
            format!("decodeErrors: {d}: decoded without an error"),
        );
    }
    let kind = t["bson_type"].as_str().unwrap_or("");
    for case in t["parseErrors"].as_array().into_iter().flatten() {
        let d = case["description"].as_str().unwrap_or("?");
        let s = case["string"].as_str().expect("string");
        if kind == "0x13" {
            check(
                Decimal128::parse(s).is_none(),
                format!("parseErrors: {d}: Decimal128 {s:?} parsed"),
            );
        } else {
            // An Extended JSON error, in JSON that is itself valid.
            let ok_json = serde_json::from_str::<Value>(s).is_ok();
            let refused = Bson::parse_extended_json(s).is_err();
            check(
                refused,
                format!("parseErrors: {d}: {s} parsed as Extended JSON"),
            );
            check(
                ok_json || kind == "0x00",
                format!("parseErrors: {d}: not even valid JSON"),
            );
        }
    }
    (fails, checked)
}

macro_rules! corpus {
    ($($test:ident => $file:literal),* $(,)?) => {$(
        #[test]
        fn $test() {
            let (fails, checked) = run_file($file);
            if checked > 0 {
                eprintln!("bson-corpus {}: {} assertions, {} failed", $file, checked, fails.len());
            }
            assert!(fails.is_empty(), "bson-corpus {}: {} of {checked} assertions failed:\n{}", $file, fails.len(), fails.join("\n"));
        }
    )*};
}

corpus! {
    bson_corpus_array => "array",
    bson_corpus_binary => "binary",
    bson_corpus_boolean => "boolean",
    bson_corpus_code => "code",
    bson_corpus_code_w_scope => "code_w_scope",
    bson_corpus_datetime => "datetime",
    bson_corpus_dbpointer => "dbpointer",
    bson_corpus_dbref => "dbref",
    bson_corpus_decimal128_1 => "decimal128-1",
    bson_corpus_decimal128_2 => "decimal128-2",
    bson_corpus_decimal128_3 => "decimal128-3",
    bson_corpus_decimal128_4 => "decimal128-4",
    bson_corpus_decimal128_5 => "decimal128-5",
    bson_corpus_decimal128_6 => "decimal128-6",
    bson_corpus_decimal128_7 => "decimal128-7",
    bson_corpus_document => "document",
    bson_corpus_double => "double",
    bson_corpus_int32 => "int32",
    bson_corpus_int64 => "int64",
    bson_corpus_maxkey => "maxkey",
    bson_corpus_minkey => "minkey",
    bson_corpus_multi_type => "multi-type",
    bson_corpus_multi_type_deprecated => "multi-type-deprecated",
    bson_corpus_null => "null",
    bson_corpus_oid => "oid",
    bson_corpus_regex => "regex",
    bson_corpus_string => "string",
    bson_corpus_symbol => "symbol",
    bson_corpus_timestamp => "timestamp",
    bson_corpus_top => "top",
    bson_corpus_undefined => "undefined",
}

/// Every file in the corpus has a test above (a new one fails here until added).
#[test]
fn bson_corpus_every_file_is_run() {
    let Some(dir) = spec_dir() else { return };
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            e.ok()?
                .file_name()
                .to_str()?
                .strip_suffix(".json")
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    assert_eq!(names.len(), 31, "{names:?}");
}

// ---- the corpus README's prose tests (not data-driven; always run) --------

/// Prose test 1: NUL bytes are refused when encoding a key (root and nested),
/// a regex pattern and regex options.
#[test]
fn prose_1_nul_bytes_are_refused_when_encoding() {
    let mut root = Document::new();
    root.insert("a\0b", 1);
    let mut nested = Document::new();
    nested.insert("x", Bson::Document(root.clone()));
    let mut pattern = Document::new();
    pattern.insert("r", Bson::RegularExpression(Regex::new("a\0b", "i")));
    let mut options = Document::new();
    options.insert(
        "r",
        Bson::RegularExpression(Regex {
            pattern: "ab".into(),
            options: "i\0".into(),
        }),
    );
    for (what, d) in [
        ("root key", root),
        ("sub-document key", nested),
        ("regex pattern", pattern),
        ("regex options", options),
    ] {
        assert!(d.to_bytes().is_err(), "{what}: a NUL byte was encoded");
        assert!(
            d.encode_into(&mut Vec::new()).is_err(),
            "{what}: encode_into accepted a NUL byte"
        );
    }
}

const DUPLICATE: &str = "1700000010666f6f000100000010666f6f000200000000";

/// Prose test 2.1: the builder replaces a repeated key (last one wins, in place).
#[test]
fn prose_2_1_encoding_duplicate_keys() {
    let mut d = Document::new();
    d.insert("foo", 1).insert("foo", 2);
    assert_eq!(
        to_hex(&d.to_bytes().unwrap()),
        to_hex(&hex("0e00000010666f6f000200000000"))
    );
}

/// Prose test 2.2: decoding keeps both elements; lookup returns the first;
/// the ordered Extended JSON writer keeps both; the serde_json map keeps the last.
#[test]
fn prose_2_2_decoding_duplicate_keys() {
    let d = Document::from_bytes(&hex(DUPLICATE)).unwrap();
    let elements: Vec<(&str, &Bson)> = d.iter().collect();
    assert_eq!(
        elements,
        vec![("foo", &Bson::Int32(1)), ("foo", &Bson::Int32(2))]
    );
    assert_eq!(d.get("foo"), Some(&Bson::Int32(1)));
    assert_eq!(
        Bson::Document(d.clone()).to_relaxed_json_string(),
        r#"{"foo":1,"foo":2}"#
    );
    assert_eq!(d.to_relaxed_json(), serde_json::json!({"foo": 2}));
}

/// Prose test 2.3: decode then encode reproduces the bytes (both kept).
#[test]
fn prose_2_3_round_trip_duplicate_keys() {
    let d = Document::from_bytes(&hex(DUPLICATE)).unwrap();
    assert_eq!(to_hex(&d.to_bytes().unwrap()), to_hex(&hex(DUPLICATE)));
}

/// Prose test 2.4: Extended JSON with a repeated key keeps the last value.
#[test]
fn prose_2_4_extended_json_duplicate_keys() {
    let b = Bson::parse_extended_json(r#"{ "foo": 1, "foo": 2 }"#).unwrap();
    let Bson::Document(d) = b else {
        panic!("not a document")
    };
    assert_eq!(
        to_hex(&d.to_bytes().unwrap()),
        to_hex(&hex("0e00000010666f6f000200000000"))
    );
}
