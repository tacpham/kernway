//! Static resolution — runs on every static request, before any I/O.
//!
//! `resolve` is the security boundary and the per-request cost that scales with
//! path depth. `etag_matches` runs on every conditional request. `mime_for` on
//! every response. None touch the filesystem, so what is measured here is pure
//! CPU on the hot path — the file read itself is I/O and is measured elsewhere
//! (a load test, not a micro-benchmark).

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use kernway_static::{etag, etag_matches, mime_for, StaticFiles};
use std::path::Path;

fn resolve(c: &mut Criterion) {
    let sf = StaticFiles::new("public");
    let mut group = c.benchmark_group("resolve");

    // The common case: a plain asset path, no encoding, resolves under the root.
    group.bench_function("plain", |b| {
        b.iter(|| black_box(&sf).resolve(black_box("/assets/app.css")))
    });

    // Directory request → index. One extra push.
    group.bench_function("index", |b| {
        b.iter(|| black_box(&sf).resolve(black_box("/")))
    });

    // Percent-encoded: pays the decode pass. `%2e%2e` is the shape an attacker
    // sends, so the reject path is worth watching too.
    group.bench_function("percent_encoded_traversal_rejected", |b| {
        b.iter(|| black_box(&sf).resolve(black_box("/%2e%2e/%2e%2e/etc/passwd")))
    });

    // A deep legitimate path — cost grows with segment count.
    group.bench_function("deep", |b| {
        b.iter(|| black_box(&sf).resolve(black_box("/a/b/c/d/e/f/g/style.css")))
    });

    group.finish();
}

fn conditional(c: &mut Criterion) {
    let tag = etag(1222, 0x18c4_fd34_848d_d5fd);
    let mut group = c.benchmark_group("etag");

    group.bench_function("build", |b| {
        b.iter(|| etag(black_box(1222), black_box(0x18c4_fd34_848d_d5fd)))
    });

    // The 304 decision on a matching conditional request.
    group.bench_function("matches_hit", |b| {
        b.iter(|| etag_matches(black_box(&tag), black_box(&tag)))
    });

    // A list of candidates, ours last — the miss walks the whole list.
    let list = format!("\"a-1\", \"b-2\", \"c-3\", {tag}");
    group.bench_function("matches_in_list", |b| {
        b.iter(|| etag_matches(black_box(&list), black_box(&tag)))
    });

    group.finish();
}

fn mime(c: &mut Criterion) {
    c.bench_function("mime_for", |b| {
        b.iter(|| mime_for(black_box(Path::new("public/assets/app.css"))))
    });
}

criterion_group!(benches, resolve, conditional, mime);
criterion_main!(benches);
