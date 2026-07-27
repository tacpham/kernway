#![allow(missing_docs)] // a benchmark binary, not public API
//! Our router against the incumbent — `matchit`, the radix-trie router axum uses.
//!
//! KEP-0000 §2: writing our own router is only justified if it is at least as
//! fast as the crate a mainstream framework reaches for. Same route table, same
//! lookups, same machine, same process. Two designs are compared:
//!
//! - Kernway: a hash map for static paths, a linear scan for the parameterised
//!   ones. O(1) for static, O(n-dynamic-routes) for a param match.
//! - matchit: one radix trie. O(path-length) for everything.
//!
//! The expected story, which the numbers confirm or refute: Kernway wins on
//! static hits (a hash lookup beats a trie walk) and loses on param hits as the
//! table grows (a linear scan is O(n); a trie is not). That loss is the
//! optimisation target the loop exists to close.

use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use kernway_core::{error::StatusCode, response::Response};
use kernway_server::router::{Handler, Router};

/// A route table shaped like a real API: `pairs` resources, each with a static
/// collection path and a parameterised item path, plus two extras.
fn routes(pairs: usize) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    for i in 0..pairs {
        out.push((format!("/resource{i}/items"), false));
        out.push((format!("/resource{i}/items/{{id}}"), true));
    }
    out.push(("/users/{id}/posts/{post}".to_string(), true));
    out.push(("/health".to_string(), false));
    out
}

fn kernway_router(pairs: usize) -> Router {
    let mut r = Router::new();
    let h: Handler = Arc::new(|_req, _ctx| Box::pin(async { Response::new(StatusCode::OK) }));
    for (pattern, _) in routes(pairs) {
        r.add("GET", &pattern, Arc::clone(&h));
    }
    r
}

fn matchit_router(pairs: usize) -> matchit::Router<usize> {
    let mut r = matchit::Router::new();
    for (i, (pattern, _)) in routes(pairs).into_iter().enumerate() {
        r.insert(pattern, i).unwrap();
    }
    r
}

fn comparison(c: &mut Criterion) {
    for pairs in [10usize, 50] {
        let total = pairs * 2 + 2;
        let kw = kernway_router(pairs);
        let mi = matchit_router(pairs);

        // --- static hit: /health, registered last ---
        let mut g = c.benchmark_group(format!("static_hit/{total}"));
        g.bench_function("kernway", |b| {
            b.iter(|| black_box(kw.find(black_box("GET"), black_box("/health"))).is_some())
        });
        g.bench_function("matchit", |b| {
            b.iter(|| black_box(mi.at(black_box("/health"))).is_ok())
        });
        g.finish();

        // --- param hit: /users/7/posts/42 ---
        let mut g = c.benchmark_group(format!("param_hit/{total}"));
        g.bench_function("kernway", |b| {
            b.iter(|| black_box(kw.find(black_box("GET"), black_box("/users/7/posts/42"))).is_some())
        });
        g.bench_function("matchit", |b| {
            b.iter(|| black_box(mi.at(black_box("/users/7/posts/42"))).is_ok())
        });
        g.finish();

        let _ = BenchmarkId::new("", total); // keep the import if the shape changes
    }
}

criterion_group!(benches, comparison);
criterion_main!(benches);
