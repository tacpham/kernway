#![allow(missing_docs)] // a test binary, not public API
//! Recording from real rt-net shard threads: each writes its own slot, the
//! scrape sums them.

use std::net::TcpListener;
use std::sync::mpsc;
use std::time::Duration;

use kernway_metrics::{HistogramConfig, Registry};
use rt_core::Shutdown;
use rt_net::{run_shards_with_init, shard_thread_count, ShardConfig};

#[test]
fn metrics_recorded_on_real_shards_are_summed() {
    let reg: &'static Registry = Box::leak(Box::new(Registry::with_shard_slots(4)));
    let hits = reg.counter("hits_total", "Hits.").unwrap();
    let open = reg.gauge("open", "Open things.").unwrap();
    let sizes = reg
        .histogram("size_bytes", "Sizes.", HistogramConfig::bytes())
        .unwrap();

    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    let config = ShardConfig::new(addr).shards(3).pin_threads(false);
    let shards = shard_thread_count(&config);

    let (done_tx, done_rx) = mpsc::channel::<Option<usize>>();
    let shutdown = Shutdown::new();
    let init = {
        let (hits, open, sizes) = (hits.clone(), open.clone(), sizes.clone());
        move |_index: usize| {
            for i in 0..10_000u64 {
                hits.inc();
                sizes.record(i);
            }
            open.add(7);
            open.dec();
            let _ = done_tx.send(rt_net::shard_index());
        }
    };
    let server = {
        let shutdown = shutdown.clone();
        std::thread::spawn(move || run_shards_with_init(config, shutdown, init, |_s| async {}))
    };
    let mut seen: Vec<_> = (0..shards)
        .map(|_| done_rx.recv_timeout(Duration::from_secs(10)).unwrap())
        .collect();
    seen.sort();
    assert_eq!(seen, (0..shards).map(Some).collect::<Vec<_>>());

    hits.add(5); // from this non-shard thread: the overflow slot
    assert_eq!(hits.get(), shards as u64 * 10_000 + 5);
    assert_eq!(open.get(), shards as i64 * 6);
    assert_eq!(sizes.count(), shards as u64 * 10_000);
    assert_eq!(sizes.sum(), shards as u64 * (10_000 * 9_999 / 2));

    let text = kernway_metrics::exposition(reg);
    assert!(text.contains(&format!("hits_total {}\n", shards * 10_000 + 5)));
    assert!(text.contains(&format!("size_bytes_count {}\n", shards * 10_000)));

    shutdown.trigger();
    server.join().unwrap().unwrap();
}
