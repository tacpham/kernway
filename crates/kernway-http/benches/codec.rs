#![allow(missing_docs)] // a benchmark binary, not public API
//! HTTP/1.1 codec — runs once per request in each direction.
//!
//! With keep-alive, parse and encode are the per-request cost that no longer
//! hides behind a TCP handshake, which makes them worth watching.
//!
//! `parse_bytes` is also measured on a buffer that does *not* yet hold a whole
//! request: with a persistent connection that path runs on every partial read,
//! so a slow "not yet" answer costs more than the successful one.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use kernway_core::{error::StatusCode, response::Response};
use kernway_http::{encode_response, encode_response_with, parse_bytes, writer::Connection};

const SIMPLE_GET: &[u8] = b"GET /health HTTP/1.1\r\nHost: localhost:8080\r\n\r\n";

const BROWSER_GET: &[u8] = b"GET /users/42?expand=profile&fields=id,name HTTP/1.1\r\n\
Host: api.example.com\r\n\
User-Agent: Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36\r\n\
Accept: application/json, text/plain, */*\r\n\
Accept-Language: en-GB,en;q=0.9\r\n\
Accept-Encoding: gzip, deflate, br\r\n\
Connection: keep-alive\r\n\
Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.payload.signature\r\n\
\r\n";

fn json_post() -> Vec<u8> {
    let body = br#"{"name":"Alice","email":"alice@example.com","roles":["admin","user"]}"#;
    let mut raw = format!(
        "POST /users HTTP/1.1\r\nHost: api.example.com\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    raw.extend_from_slice(body);
    raw
}

fn parsing(c: &mut Criterion) {
    let post = json_post();

    let mut group = c.benchmark_group("parse");
    group.throughput(Throughput::Bytes(SIMPLE_GET.len() as u64));
    group.bench_function("minimal_get", |b| {
        b.iter(|| black_box(parse_bytes(black_box(SIMPLE_GET)).unwrap()));
    });

    group.throughput(Throughput::Bytes(BROWSER_GET.len() as u64));
    group.bench_function("browser_get_8_headers", |b| {
        b.iter(|| black_box(parse_bytes(black_box(BROWSER_GET)).unwrap()));
    });

    group.throughput(Throughput::Bytes(post.len() as u64));
    group.bench_function("json_post_with_body", |b| {
        b.iter(|| black_box(parse_bytes(black_box(&post)).unwrap()));
    });

    // The keep-alive read loop hits this on every partial read, so it must be
    // cheap to say "not yet" — it scans for the blank line and gives up.
    let partial = &BROWSER_GET[..BROWSER_GET.len() - 20];
    group.throughput(Throughput::Bytes(partial.len() as u64));
    group.bench_function("incomplete_head", |b| {
        b.iter(|| black_box(parse_bytes(black_box(partial)).unwrap()));
    });
    group.finish();
}

fn encoding(c: &mut Criterion) {
    let small = Response::new(StatusCode::OK)
        .content_type("application/json")
        .body(br#"{"status":"UP"}"#.to_vec());

    // A response with the header set a static file or a secured page carries —
    // the "many headers" side of the structure question. Built fresh in the
    // closure so the set cost is measured too, not just the encode.
    let many_pairs: &[(&str, &str)] = &[
        ("etag", "\"4c6-18c4fd34848dd5fd\""),
        ("cache-control", "no-cache"),
        ("x-content-type-options", "nosniff"),
        ("accept-ranges", "bytes"),
        ("vary", "Accept-Encoding"),
        ("x-frame-options", "DENY"),
        ("x-request-id", "550e8400-e29b-41d4-a716-446655440000"),
    ];

    let large = Response::new(StatusCode::OK)
        .content_type("application/json")
        .body(vec![b'x'; 64 * 1024]);

    let mut group = c.benchmark_group("encode");
    group.bench_function("small_json_close", |b| {
        b.iter(|| black_box(encode_response(black_box(&small))));
    });
    group.bench_function("small_json_keep_alive", |b| {
        b.iter(|| {
            black_box(encode_response_with(
                black_box(&small),
                Connection::KeepAlive,
            ))
        });
    });
    // Build a many-header response and encode it: the full response path with a
    // realistic header count, which is where the one-buffer layout should pay.
    group.bench_function("eight_headers", |b| {
        b.iter(|| {
            let mut resp = Response::new(StatusCode::OK).content_type("text/html; charset=utf-8");
            for (k, v) in many_pairs {
                resp.headers.insert(k, v);
            }
            black_box(encode_response_with(
                black_box(&resp),
                Connection::KeepAlive,
            ))
        });
    });

    // Head and body share one buffer, so a large body pays a copy. This is the
    // number to watch if that trade is ever revisited.
    group.throughput(Throughput::Bytes(64 * 1024));
    group.bench_function("64kb_body", |b| {
        b.iter(|| {
            black_box(encode_response_with(
                black_box(&large),
                Connection::KeepAlive,
            ))
        });
    });
    group.finish();
}

criterion_group!(benches, parsing, encoding);
criterion_main!(benches);
