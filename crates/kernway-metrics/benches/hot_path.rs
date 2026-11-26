#![allow(missing_docs)] // a benchmark binary, not public API
//! The hot path: `counter.inc()` and `histogram.record()` from 1 and N real
//! rt-net shard threads at once, against a naive shared `AtomicU64` that every
//! thread hits.
//!
//! The worker threads are genuine shards — started by `run_shards_with_init`,
//! so `rt_net::shard_index()` is set exactly as in a server — and the bench
//! loop runs inside their `init`. Each sample releases all N workers through a
//! barrier; each runs `iters` operations; the sample time is the slowest
//! worker's. So a reported time is "one op on each of N cores, concurrently":
//! flat across N means no contention, growing with N means cache-line
//! ping-pong.

use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant};

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use kernway_metrics::{Counter, Histogram, HistogramConfig, Registry};
use rt_core::Shutdown;
use rt_net::{run_shards_with_init, shard_thread_count, ShardConfig};

#[derive(Clone)]
enum Op {
    CounterInc(Counter),
    HistogramRecord(Histogram),
    SharedAtomic(Arc<AtomicU64>),
}

enum Cmd {
    Run {
        op: Op,
        iters: u64,
        barrier: Arc<Barrier>,
    },
    Stop,
}

/// `n` live shard threads waiting for work.
struct ShardPool {
    cmds: Vec<Sender<Cmd>>,
    done: Receiver<Duration>,
    shutdown: Shutdown,
    server: Option<std::thread::JoinHandle<std::io::Result<()>>>,
}

impl ShardPool {
    fn start(n: usize) -> Self {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        let config = ShardConfig::new(addr).shards(n).pin_threads(true);
        let got = shard_thread_count(&config);
        assert_eq!(got, n, "platform started {got} shards, wanted {n}");

        let (cmd_txs, cmd_rxs): (Vec<_>, Vec<_>) = (0..n).map(|_| channel::<Cmd>()).unzip();
        let cmd_rxs = Arc::new(Mutex::new(
            cmd_rxs.into_iter().map(Some).collect::<Vec<_>>(),
        ));
        let (done_tx, done) = channel();
        let shutdown = Shutdown::new();
        let init = move |index: usize| {
            assert_eq!(rt_net::shard_index(), Some(index));
            let rx = cmd_rxs.lock().unwrap()[index].take().unwrap();
            // Blocks this shard's executor on purpose: it is a bench worker now.
            while let Ok(Cmd::Run { op, iters, barrier }) = rx.recv() {
                barrier.wait();
                let start = Instant::now();
                match &op {
                    Op::CounterInc(c) => (0..iters).for_each(|_| black_box(c).inc()),
                    Op::HistogramRecord(h) => {
                        (0..iters).for_each(|i| black_box(h).record(black_box(i & 0xFFFF)))
                    }
                    Op::SharedAtomic(a) => (0..iters).for_each(|_| {
                        black_box(a).fetch_add(1, Relaxed);
                    }),
                }
                done_tx.send(start.elapsed()).unwrap();
            }
        };
        let server = {
            let shutdown = shutdown.clone();
            std::thread::spawn(move || run_shards_with_init(config, shutdown, init, |_s| async {}))
        };
        Self {
            cmds: cmd_txs,
            done,
            shutdown,
            server: Some(server),
        }
    }

    /// Run `op` `iters` times on every shard at once; the slowest shard's time.
    fn run(&self, op: &Op, iters: u64) -> Duration {
        let barrier = Arc::new(Barrier::new(self.cmds.len()));
        for tx in &self.cmds {
            tx.send(Cmd::Run {
                op: op.clone(),
                iters,
                barrier: barrier.clone(),
            })
            .unwrap();
        }
        (0..self.cmds.len())
            .map(|_| self.done.recv().unwrap())
            .max()
            .unwrap()
    }
}

impl Drop for ShardPool {
    fn drop(&mut self) {
        for tx in &self.cmds {
            let _ = tx.send(Cmd::Stop);
        }
        self.shutdown.trigger();
        if let Some(s) = self.server.take() {
            let _ = s.join();
        }
    }
}

fn thread_counts() -> Vec<usize> {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    [1, 2, 4, 8].into_iter().filter(|&n| n <= cores).collect()
}

fn concurrent(c: &mut Criterion) {
    for n in thread_counts() {
        let pool = ShardPool::start(n);
        let reg = Registry::with_shard_slots(n);
        let counter = reg.counter("bench_total", "").unwrap();
        let histogram = reg
            .histogram("bench_seconds", "", HistogramConfig::latency_seconds())
            .unwrap();
        let shared = Arc::new(AtomicU64::new(0));

        let mut g = c.benchmark_group("concurrent");
        for (name, op) in [
            ("counter.inc", Op::CounterInc(counter)),
            ("histogram.record", Op::HistogramRecord(histogram)),
            ("shared_atomic_u64 (baseline)", Op::SharedAtomic(shared)),
        ] {
            g.bench_with_input(
                BenchmarkId::new(name, format!("{n}-shards")),
                &op,
                |b, op| b.iter_custom(|iters| pool.run(op, iters)),
            );
        }
        g.finish();
    }
}

fn single_thread(c: &mut Criterion) {
    let mut g = c.benchmark_group("off_shard");
    let counter = Counter::detached(4);
    g.bench_function("counter.inc (overflow slot)", |b| {
        b.iter(|| black_box(&counter).inc())
    });
    let h = Histogram::detached(4, HistogramConfig::latency_seconds());
    g.bench_function("timer (2x Instant::now + record)", |b| {
        b.iter(|| drop(black_box(&h).start_timer()))
    });
    g.bench_function("Instant::now", |b| b.iter(Instant::now));
    g.finish();

    // Scrape cost: 100 counters + 10 histograms, 8 slots each.
    let reg = Registry::with_shard_slots(8);
    let cv = reg.counter_vec("req_total", "", &["route"]).unwrap();
    let hv = reg
        .histogram_vec(
            "lat_seconds",
            "",
            &["route"],
            HistogramConfig::latency_seconds(),
        )
        .unwrap();
    for i in 0..100 {
        cv.with(&[&format!("/r{i}")]).add(i);
    }
    for i in 0..10 {
        let h = hv.with(&[&format!("/r{i}")]);
        (0..1000).for_each(|v| h.record(v * 1000));
    }
    c.bench_function("exposition (100 counters + 10 histograms)", |b| {
        b.iter(|| kernway_metrics::exposition(black_box(&reg)))
    });
}

criterion_group!(benches, concurrent, single_thread);
criterion_main!(benches);
