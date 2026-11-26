//! Per-shard slots: which slot the current thread writes, and the padding that
//! keeps two slots off the same cache line.

/// A value alone on its own cache-line pair.
///
/// 128, not 64: Apple Silicon (M1–M4) has 128-byte L1/L2 lines, and on x86 the
/// L2 "adjacent line" prefetcher pulls lines in 128-byte pairs, so two hot
/// counters 64 bytes apart still ping-pong between cores there. 128 is what
/// `crossbeam_utils::CachePadded` picks for x86_64/aarch64 for the same reason.
/// The cost is memory — 128 bytes per slot per metric — which is why slot
/// counts are bounded by the shard count, not by thread count.
#[repr(align(128))]
#[derive(Debug, Default)]
pub(crate) struct Padded<T>(pub(crate) T);

/// Default number of shard slots a [`Registry`](crate::Registry) gives each
/// metric: one per CPU, the same default `rt_net::ShardConfig` uses for its
/// shard count. One extra overflow slot is always added on top.
pub(crate) fn default_shard_slots() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[cfg(test)]
thread_local! {
    /// Test-only stand-in for `rt_net::shard_index()`, so unit tests can play
    /// "shard 3" on a plain thread.
    pub(crate) static FAKE_SHARD: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// The slot the current thread writes, for a metric with `len` slots
/// (`len - 1` shard slots plus the overflow slot at `len - 1`).
///
/// A shard thread gets its own slot — the only writer of it, so its
/// `Relaxed` adds never contend. Every other thread (the main thread, a
/// blocking-pool worker, a shard whose index exceeds the configured slot
/// count, a second server in the same process) shares the overflow slot:
/// still correct, because every write is an atomic add, merely contended.
#[inline(always)]
pub(crate) fn current_slot(len: usize) -> usize {
    #[cfg(test)]
    let index = FAKE_SHARD
        .with(std::cell::Cell::get)
        .or_else(rt_net::shard_index);
    #[cfg(not(test))]
    let index = rt_net::shard_index();

    let overflow = len - 1;
    match index {
        Some(i) if i < overflow => i,
        _ => overflow,
    }
}
