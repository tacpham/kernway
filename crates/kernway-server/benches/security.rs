#![allow(missing_docs)] // a benchmark binary, not public API
//! The per-request authorization cost: how much a `SecurityLayer` policy adds, and
//! how it scales with the number of rules.
//!
//! `allows(method, path, ctx)` is what the security middleware runs per request —
//! scan the rules for the first matching (method + Ant pattern), then decide against
//! the context. Measured at a few policy sizes, for a rule *hit* (matches the first
//! rule) and a full *scan* (falls through to `any_request`), plus the role check
//! alone.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use kernway_security::SecurityContext;
use kernway_server::{Access, HttpSecurity, SecurityLayer};

/// A policy of `n` role rules, then a public rule and an authenticated default.
fn policy(n: usize) -> SecurityLayer {
    let mut builder = HttpSecurity::new();
    for i in 0..n {
        builder = builder.has_role(&format!("/resource{i}/**"), "ADMIN");
    }
    builder
        .permit_all("/public/**")
        .any_request(Access::Authenticated)
        .build()
}

fn security(c: &mut Criterion) {
    let admin = SecurityContext::authenticated("admin", ["ADMIN"]);
    let mut group = c.benchmark_group("security");

    for n in [1usize, 10, 50] {
        let layer = policy(n);
        // Hit: matches the first rule (short scan).
        group.bench_with_input(BenchmarkId::new("allows_hit", n), &n, |b, _| {
            b.iter(|| black_box(layer.allows("GET", black_box("/resource0/x"), &admin)));
        });
        // Scan: no rule matches, falls through to the default (walks all n rules).
        group.bench_with_input(BenchmarkId::new("allows_scan", n), &n, |b, _| {
            b.iter(|| black_box(layer.allows("GET", black_box("/unmatched/deep/path"), &admin)));
        });
    }

    // The role check on its own (a HashSet lookup) — what #[require_role] pays.
    group.bench_function("has_role", |b| {
        b.iter(|| black_box(admin.has_role(black_box("ADMIN"))));
    });

    group.finish();
}

criterion_group!(benches, security);
criterion_main!(benches);
