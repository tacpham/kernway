//! Executor hot paths.
//!
//! The thread-per-core design pays for itself in two places, and both are
//! measured here:
//!
//! - **spawn** — an `Rc<Task>` in a slab, no atomics, no work-stealing deque.
//! - **wake → poll** — the round trip a future takes every time it yields.
//!   Under load this runs far more often than spawn, so it is the number that
//!   decides whether the runtime is fast.
//!
//! Deliberately *not* here: throughput over a socket. That belongs in a
//! macro-benchmark against a load generator on Linux, where thread pinning and
//! `SO_REUSEPORT` balancing actually exist — measuring it in-process would
//! produce a number that means nothing.

use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use rt_core::Executor;

/// Yields `n` times before completing — each yield is one wake → poll cycle.
struct YieldN(usize);

impl Future for YieldN {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 == 0 {
            return Poll::Ready(());
        }
        self.0 -= 1;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

fn block_on_overhead(c: &mut Criterion) {
    let ex = Executor::new().unwrap();
    let mut group = c.benchmark_group("block_on");
    // Floor: what `block_on` costs when the future is already done. Anything
    // measured below is this plus the real work.
    group.bench_function("ready_future", |b| {
        b.iter(|| black_box(ex.block_on(async { black_box(1u32) }).unwrap()));
    });
    group.finish();
}

fn wake_poll_cycle(c: &mut Criterion) {
    let ex = Executor::new().unwrap();
    let mut group = c.benchmark_group("wake_poll_cycle");
    for yields in [1usize, 100, 1000] {
        group.throughput(criterion::Throughput::Elements(yields as u64));
        group.bench_with_input(BenchmarkId::from_parameter(yields), &yields, |b, &n| {
            b.iter(|| ex.block_on(YieldN(black_box(n))).unwrap());
        });
    }
    group.finish();
}

fn spawn_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("spawn");
    for count in [10usize, 1000] {
        group.throughput(criterion::Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &n| {
            b.iter(|| {
                // A fresh executor per iteration: reusing one would measure a
                // warmed slab and free list rather than honest spawn cost.
                let ex = Executor::new().unwrap();
                let done = Rc::new(Cell::new(0usize));
                for _ in 0..n {
                    let done = Rc::clone(&done);
                    ex.spawn(async move {
                        done.set(done.get() + 1);
                    });
                }
                ex.run().unwrap();
                black_box(done.get())
            });
        });
    }
    group.finish();
}

fn timer_overhead(c: &mut Criterion) {
    let ex = Executor::new().unwrap();
    let mut group = c.benchmark_group("timers");
    // Registration + immediate fire. Measures the heap and waker bookkeeping,
    // not the wait itself — a zero deadline is already expired.
    group.bench_function("expired_sleep", |b| {
        b.iter(|| ex.block_on(rt_core::sleep(Duration::ZERO)).unwrap());
    });
    // The cancelled path: `timeout` around a future that wins, which drops its
    // `Sleep` and must remove the registration.
    group.bench_function("timeout_cancelled", |b| {
        b.iter(|| {
            ex.block_on(async {
                black_box(rt_core::timeout(Duration::from_secs(60), async { 1u32 }).await)
            })
            .unwrap()
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    block_on_overhead,
    wake_poll_cycle,
    spawn_throughput,
    timer_overhead
);
criterion_main!(benches);
