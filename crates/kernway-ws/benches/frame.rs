//! Hot paths of the WebSocket server: unmasking and frame parsing.
//!
//! `cargo bench -p kernway-ws`

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use kernway_ws::frame::{encode_server_frame, parse_client_header, unmask};

const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];

fn naive_unmask(data: &mut [u8], mask: [u8; 4]) {
    for (i, b) in data.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
}

fn bench_unmask(c: &mut Criterion) {
    let mut g = c.benchmark_group("unmask");
    for size in [16usize, 128, 1024, 64 * 1024] {
        let mut buf = vec![0x5au8; size];
        g.throughput(Throughput::Bytes(size as u64));
        g.bench_with_input(BenchmarkId::new("wide_u64", size), &size, |b, _| {
            b.iter(|| unmask(black_box(&mut buf), MASK));
        });
        g.bench_with_input(BenchmarkId::new("byte_loop", size), &size, |b, _| {
            b.iter(|| naive_unmask(black_box(&mut buf), MASK));
        });
    }
    g.finish();
}

/// A burst of masked client frames, as one TCP read delivers them.
fn client_burst(n: usize, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..n {
        out.push(0x81);
        out.push(0x80 | payload.len() as u8);
        out.extend_from_slice(&MASK);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ MASK[i & 3]));
    }
    out
}

fn bench_parse(c: &mut Criterion) {
    // A chess move message: ~40 bytes of JSON.
    let msg = br#"{"t":"mv","g":123456,"ply":41,"u":"h2e2"}"#;
    let burst = client_burst(1000, msg);
    let mut g = c.benchmark_group("parse_burst_1000_moves");
    g.throughput(Throughput::Elements(1000));
    g.bench_function("cursor_in_place", |b| {
        let mut buf = burst.clone();
        b.iter(|| {
            buf.copy_from_slice(&burst);
            let mut start = 0;
            let mut total = 0usize;
            while let Ok(Some(h)) = parse_client_header(&buf[start..], 1 << 20) {
                let (s, e) = (start + h.payload_start, start + h.frame_len());
                unmask(&mut buf[s..e], h.mask);
                total += buf[s] as usize;
                start += h.frame_len();
                if start == buf.len() {
                    break;
                }
            }
            black_box(total)
        });
    });
    g.bench_function("drain_and_copy (old design)", |b| {
        b.iter(|| {
            let mut buf = burst.clone();
            let mut total = 0usize;
            while let Ok(Some(h)) = parse_client_header(&buf, 1 << 20) {
                let mut payload = buf[h.payload_start..h.frame_len()].to_vec();
                naive_unmask(&mut payload, h.mask);
                total += payload[0] as usize;
                buf.drain(..h.frame_len());
                if buf.is_empty() {
                    break;
                }
            }
            black_box(total)
        });
    });
    g.finish();
}

fn bench_encode(c: &mut Criterion) {
    let msg = br#"{"t":"mv","g":123456,"ply":41,"u":"h2e2","clk":[581,602]}"#;
    let mut g = c.benchmark_group("encode_server_frame");
    g.throughput(Throughput::Elements(1));
    g.bench_function("move", |b| {
        let mut out = Vec::with_capacity(128);
        b.iter(|| {
            out.clear();
            encode_server_frame(&mut out, 0x1, black_box(msg));
            black_box(out.len())
        });
    });
    g.finish();
}

criterion_group!(benches, bench_unmask, bench_parse, bench_encode);
criterion_main!(benches);
