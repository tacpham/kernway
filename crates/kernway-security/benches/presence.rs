#![allow(missing_docs)] // a benchmark binary, not public API
//! What presence costs the server, by operation and by number of online users.
//!
//! The question behind "how many online users can we track": which operation is
//! the bottleneck, and how does it scale with N (users online)?
//!
//!   - `heartbeat`      — one beat (a map write under the lock). Should be flat in N.
//!   - `is_online`      — one lookup. Flat in N.
//!   - `count/N`        — how many are online: an O(N) scan.
//!   - `online/N`       — the full sorted list: O(N) scan + N string clones + sort.
//!
//! `heartbeat`/`is_online` are what every client hits constantly; `online`/`count`
//! are the "who's online" read. The numbers show which one to worry about at scale.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use kernway_security::{InMemoryPresence, Presence};

/// Drive a ready presence future (the in-memory tracker resolves on first poll).
fn drive<T>(mut fut: Pin<Box<dyn Future<Output = T> + Send + '_>>) -> T {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => unreachable!("in-memory presence resolves on the first poll"),
    }
}

/// A tracker with `n` users online, all beaten at t=1000 (window 30s).
fn populated(n: usize) -> InMemoryPresence {
    let p = InMemoryPresence::new(Duration::from_secs(30));
    for i in 0..n {
        drive(p.heartbeat(&format!("user{i:08}"), 1000)).unwrap();
    }
    p
}

fn presence(c: &mut Criterion) {
    // Constant-time ops, measured against a populated tracker (10k online).
    {
        let p = populated(10_000);
        let mut g = c.benchmark_group("presence_constant");
        g.bench_function("heartbeat", |b| {
            // Beat an existing user (the common case: a repeated heartbeat).
            b.iter(|| drive(p.heartbeat(black_box("user00005000"), black_box(1010))).unwrap());
        });
        g.bench_function("is_online", |b| {
            b.iter(|| {
                black_box(drive(p.is_online(black_box("user00005000"), black_box(1010))).unwrap())
            });
        });
        g.finish();
    }

    // The reads that scale with N — the "who's online" query.
    let mut g = c.benchmark_group("presence_read");
    for n in [100usize, 10_000, 100_000] {
        let p = populated(n);
        g.bench_with_input(BenchmarkId::new("count", n), &n, |b, _| {
            b.iter(|| black_box(drive(p.count(black_box(1005))).unwrap()));
        });
        g.bench_with_input(BenchmarkId::new("online_list", n), &n, |b, _| {
            b.iter(|| black_box(drive(p.online(black_box(1005))).unwrap()));
        });
    }
    g.finish();
}

criterion_group!(benches, presence);
criterion_main!(benches);
