#![allow(missing_docs)] // a benchmark binary, not public API
//! Micro-benchmarks for the DNS wire codec — the CPU-bound hot path when a
//! process resolves many names. The I/O (UDP/TCP) is not benchmarked here; it is
//! reactor-bound and measured elsewhere.
//!
//! Run: `cargo bench -p kernway-dns`

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use kernway_dns::message::{encode_query, encode_query_edns, parse_response, EDNS_UDP_SIZE, TYPE_A};

/// Build a realistic response: `www.example.com` with three A records, using a
/// compression pointer for the answer names (as a real server does).
fn a_response() -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&0x1234u16.to_be_bytes());
    b.extend_from_slice(&0x8180u16.to_be_bytes()); // QR + RD + RA
    b.extend_from_slice(&1u16.to_be_bytes()); // QDCOUNT
    b.extend_from_slice(&3u16.to_be_bytes()); // ANCOUNT
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    let qname_off = b.len();
    for label in ["www", "example", "com"] {
        b.push(label.len() as u8);
        b.extend_from_slice(label.as_bytes());
    }
    b.push(0);
    b.extend_from_slice(&TYPE_A.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    for ip in [[93, 184, 216, 34], [93, 184, 216, 35], [93, 184, 216, 36]] {
        b.push(0xC0 | (qname_off >> 8) as u8); // compression pointer to the qname
        b.push((qname_off & 0xFF) as u8);
        b.extend_from_slice(&TYPE_A.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&300u32.to_be_bytes());
        b.extend_from_slice(&4u16.to_be_bytes());
        b.extend_from_slice(&ip);
    }
    b
}

fn bench_codec(c: &mut Criterion) {
    c.bench_function("dns/encode_query", |b| {
        b.iter(|| encode_query(black_box(0x1234), black_box("www.example.com"), TYPE_A).unwrap())
    });

    c.bench_function("dns/encode_query_edns", |b| {
        b.iter(|| {
            encode_query_edns(black_box(0x1234), black_box("www.example.com"), TYPE_A, EDNS_UDP_SIZE)
                .unwrap()
        })
    });

    let packet = a_response();
    c.bench_function("dns/parse_response_3xA", |b| {
        b.iter(|| parse_response(black_box(&packet)).unwrap())
    });
}

criterion_group!(benches, bench_codec);
criterion_main!(benches);
