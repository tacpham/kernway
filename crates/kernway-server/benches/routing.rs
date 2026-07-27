#![allow(missing_docs)] // a benchmark binary, not public API
//! Routing — runs once per request, before any handler does anything.
//!
//! Parsing and encoding are now measured; routing sits between them and was
//! not. It is also the one per-request cost that grows with the size of the
//! application, so a number that looks fine on a toy router is not evidence
//! about a real one — hence the sweep over route-table sizes.

use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use kernway_core::{error::StatusCode, response::Response};
use kernway_server::router::{Handler, Router};

/// A route table shaped like a real API: mostly static paths, some
/// parameterised, spread over several resources.
fn router_with(pairs: usize) -> Router {
    let mut router = Router::new();
    let handler: Handler = Arc::new(|_req, _ctx| Box::pin(async { Response::new(StatusCode::OK) }));
    for i in 0..pairs {
        router.add("GET", &format!("/resource{i}/items"), Arc::clone(&handler));
        router.add(
            "GET",
            &format!("/resource{i}/items/{{id}}"),
            Arc::clone(&handler),
        );
    }
    // The targets go last, so a linear scan has to walk everything else first —
    // which is simply what the last-registered route gets.
    router.add("GET", "/users/{id}/posts/{post}", Arc::clone(&handler));
    router.add("GET", "/health", handler);
    router
}

fn routing(c: &mut Criterion) {
    let mut group = c.benchmark_group("route");

    for pairs in [1usize, 10, 50] {
        let router = router_with(pairs);
        let total = pairs * 2 + 2;

        group.bench_with_input(BenchmarkId::new("static_hit", total), &router, |b, r| {
            b.iter(|| black_box(r.find(black_box("GET"), black_box("/health"))).is_some());
        });

        group.bench_with_input(BenchmarkId::new("param_hit", total), &router, |b, r| {
            b.iter(|| {
                black_box(r.find(black_box("GET"), black_box("/users/7/posts/42"))).is_some()
            });
        });

        // A 404 is the true worst case: every route is tried and none matches.
        group.bench_with_input(BenchmarkId::new("miss", total), &router, |b, r| {
            b.iter(|| black_box(r.find(black_box("GET"), black_box("/nope/nothing"))).is_none());
        });
    }
    group.finish();
}

criterion_group!(benches, routing);
criterion_main!(benches);
