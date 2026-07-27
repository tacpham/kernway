//! The per-request CPU cost of the client — the work that scales with request volume,
//! isolated from the network (real throughput is I/O-bound; this is what we control).
//!
//! Measured: URL parsing, request encoding, response-head parsing, chunked decoding,
//! and form percent-encoding — the whole CPU path of issuing one request and reading
//! one response, minus the socket.

#![allow(missing_docs)] // a benchmark binary, not public API

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use kernway_http_client::{
    bench_chunk_decoder_incremental, bench_decode_chunked, bench_encode_request, bench_parse_head, percent_encode,
    Method, Request, Url,
};

/// A response head with a realistic set of headers.
const RESPONSE_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\n\
Content-Type: application/json; charset=utf-8\r\n\
Content-Length: 137\r\n\
Date: Wed, 23 Jul 2026 10:00:00 GMT\r\n\
Server: nginx\r\n\
Cache-Control: no-store\r\n\
Vary: Accept-Encoding\r\n\
Connection: close\r\n\r\n";

fn client(c: &mut Criterion) {
    let mut group = c.benchmark_group("http-client");

    // URL parsing — done once per request.
    group.bench_function("url_parse", |b| {
        b.iter(|| Url::parse(black_box("https://oauth2.googleapis.com/token?foo=bar&baz=qux")).unwrap());
    });

    // Request encoding — build the wire bytes for a POST with a form body + headers.
    let req = Request::new(Method::Post, Url::parse("https://oauth2.googleapis.com/token").unwrap())
        .body("application/x-www-form-urlencoded", b"grant_type=authorization_code&code=abc123&client_id=xyz".to_vec())
        .header("accept", "application/json");
    group.bench_function("encode_request", |b| {
        b.iter(|| black_box(bench_encode_request(black_box(&req))));
    });

    // Response-head parsing — status line + 7 headers — ours vs httparse (the
    // SIMD-optimised parser hyper/reqwest use), on identical bytes.
    group.bench_function("parse_head/kernway", |b| {
        b.iter(|| black_box(bench_parse_head(black_box(RESPONSE_HEAD))));
    });
    group.bench_function("parse_head/httparse", |b| {
        b.iter(|| {
            let mut headers = [httparse::EMPTY_HEADER; 16];
            let mut resp = httparse::Response::new(&mut headers);
            black_box(resp.parse(black_box(RESPONSE_HEAD)).unwrap());
            black_box(resp.code)
        });
    });

    // Chunked decoding at a couple of body sizes (chunks of 256 bytes).
    for chunks in [1usize, 16] {
        let body = make_chunked(chunks, 256);
        group.bench_with_input(BenchmarkId::new("decode_chunked", chunks * 256), &body, |b, body| {
            b.iter(|| black_box(bench_decode_chunked(black_box(body))));
        });
    }

    // Streaming (incremental) vs whole-buffer chunked decoding on a larger body: the
    // whole-buffer decoder re-parses from the start every call (O(n²) when fed
    // progressively), the incremental one is O(consumed). Fed whole here for parity.
    for chunks in [64usize, 256] {
        let body = make_chunked(chunks, 256);
        group.bench_with_input(BenchmarkId::new("decode_chunked/whole", chunks * 256), &body, |b, body| {
            b.iter(|| black_box(bench_decode_chunked(black_box(body))));
        });
        group.bench_with_input(BenchmarkId::new("decode_chunked/incremental", chunks * 256), &body, |b, body| {
            b.iter(|| black_box(bench_chunk_decoder_incremental(black_box(body))));
        });
    }

    // Form percent-encoding — a token-request value.
    group.bench_function("percent_encode", |b| {
        b.iter(|| black_box(percent_encode(black_box("a value/with?reserved&chars=to encode"))));
    });

    group.finish();
}

/// Build a chunked body of `n` chunks of `size` bytes each, terminated.
fn make_chunked(n: usize, size: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.extend_from_slice(format!("{size:x}\r\n").as_bytes());
        out.extend(std::iter::repeat_n(b'x', size));
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

criterion_group!(benches, client);
criterion_main!(benches);
