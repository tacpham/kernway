#![allow(missing_docs)] // a benchmark binary, not public API
//! The recurring problem: a small set of short-keyed HTTP fields, built then
//! iterated (to encode) or looked up. `Headers` is the one-buffer structure
//! Kernway wrote for it; `HashMap<String, String>` is what `Response.headers`
//! uses.
//!
//! **A cautionary benchmark.** In isolation this says `Headers` builds+iterates
//! 5 entries ~1.35x faster than a `HashMap`. That reading is a trap: migrating
//! `Response.headers` to `Headers` on the strength of it made the *actual encode
//! path* slower (75 -> 103 ns/response), because the encoder iterates the headers
//! twice and real responses carry fewer than five. It was reverted. The number
//! that ships is the pipeline, not this — kept here as the evidence behind the
//! "measure in context" rule in KEP-0000 §2, not as a recommendation.

use std::collections::HashMap;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use kernway_core::fields::Headers;

/// A typical static-file response's headers, in the order they are set.
const TYPICAL: &[(&str, &str)] = &[
    ("content-type", "text/html; charset=utf-8"),
    ("etag", "\"4c6-18c4fd34848dd5fd\""),
    ("cache-control", "no-cache"),
    ("x-content-type-options", "nosniff"),
    ("accept-ranges", "bytes"),
];

/// More headers, to see whether a linear structure degrades against the hash.
const MANY: &[(&str, &str)] = &[
    ("content-type", "application/json; charset=utf-8"),
    ("etag", "\"abc-123\""),
    ("cache-control", "max-age=0"),
    ("x-content-type-options", "nosniff"),
    ("x-frame-options", "DENY"),
    ("x-request-id", "550e8400-e29b-41d4-a716-446655440000"),
    ("vary", "Accept-Encoding"),
    ("content-encoding", "gzip"),
    ("date", "Wed, 24 Jul 2026 12:00:00 GMT"),
    ("server", "kernway"),
    ("strict-transport-security", "max-age=31536000"),
    ("access-control-allow-origin", "*"),
];

/// The response path: set every header, then iterate them all (what the encoder
/// does). No lookups.
fn build_and_iterate(c: &mut Criterion) {
    let mut group = c.benchmark_group("headers/build_iterate");

    for (label, set) in [("5", TYPICAL), ("12", MANY)] {
        group.bench_with_input(BenchmarkId::new("hashmap", label), &set, |b, set| {
            b.iter(|| {
                let mut m: HashMap<String, String> = HashMap::new();
                for (k, v) in *set {
                    m.insert((*k).to_string(), (*v).to_string());
                }
                for (k, v) in &m {
                    black_box((k.as_str(), v.as_str()));
                }
            });
        });
        group.bench_with_input(BenchmarkId::new("headers", label), &set, |b, set| {
            b.iter(|| {
                let mut h = Headers::new();
                for (k, v) in *set {
                    h.insert(k, v);
                }
                for (k, v) in h.iter() {
                    black_box((k, v));
                }
            });
        });
    }
    group.finish();
}

/// A lookup, where the hash is supposed to win: build once, get one header. This
/// is the case `Response` almost never hits (it sets and encodes; it rarely
/// reads its own headers), included so the trade-off is visible, not hidden.
fn lookup(c: &mut Criterion) {
    let mut group = c.benchmark_group("headers/lookup_one");

    let map: HashMap<String, String> = MANY.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
    let mut hdr = Headers::new();
    for (k, v) in MANY {
        hdr.insert(k, v);
    }
    // A miss is the linear structure's worst case — the whole set is scanned.
    let target = "strict-transport-security";

    group.bench_function("hashmap_12", |b| {
        b.iter(|| black_box(map.get(black_box(target))));
    });
    group.bench_function("headers_12", |b| {
        b.iter(|| black_box(hdr.get(black_box(target))));
    });
    group.finish();
}

criterion_group!(benches, build_and_iterate, lookup);
criterion_main!(benches);
