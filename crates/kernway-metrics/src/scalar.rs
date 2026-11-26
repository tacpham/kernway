//! [`Counter`] and [`Gauge`]: one padded atomic per shard slot.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;

use crate::shard::{current_slot, Padded};

/// A monotonically increasing `u64` counter.
///
/// A cheap handle (one `Arc`): clone it into every shard, store it in app
/// state, call [`inc`](Self::inc) on the hot path. Each shard adds into its own
/// cache-line-padded slot with a single `Relaxed` atomic add — no lock, no CAS
/// loop, no cross-core traffic. A scrape sums the slots.
///
/// The total wraps at `u64::MAX` (2^64 increments — not reachable in practice).
#[derive(Clone, Debug)]
pub struct Counter(pub(crate) Arc<CounterCore>);

#[derive(Debug)]
pub(crate) struct CounterCore {
    slots: Box<[Padded<AtomicU64>]>,
}

impl CounterCore {
    pub(crate) fn new(slots: usize) -> Self {
        Self {
            slots: (0..slots).map(|_| Padded::default()).collect(),
        }
    }
}

impl Counter {
    /// A counter not attached to any registry, with `shard_slots` shard slots
    /// (plus one overflow slot). Useful for tests and benches; app metrics come
    /// from a [`Registry`](crate::Registry) so they are exported.
    pub fn detached(shard_slots: usize) -> Self {
        Self(Arc::new(CounterCore::new(shard_slots + 1)))
    }

    /// Add one.
    #[inline]
    pub fn inc(&self) {
        self.add(1);
    }

    /// Add `n`.
    #[inline]
    pub fn add(&self, n: u64) {
        let slots = &self.0.slots;
        slots[current_slot(slots.len())].0.fetch_add(n, Relaxed);
    }

    /// The current total across every shard.
    ///
    /// Not a snapshot: shards keep adding while it sums, so the result is
    /// somewhere between the totals at the start and end of the call.
    pub fn get(&self) -> u64 {
        self.0
            .slots
            .iter()
            .fold(0u64, |acc, s| acc.wrapping_add(s.0.load(Relaxed)))
    }
}

/// An `i64` value that goes up and down.
///
/// Like [`Counter`], each shard writes its own padded slot and the exported
/// value is the **sum of the slots**. That suits thread-per-core well: in-flight
/// requests or open connections are `inc`'d and `dec`'d on the shard that owns
/// the connection, and the total is exact.
///
/// [`set`](Self::set) is per-slot: it sets *this shard's contribution*. Each
/// shard reporting its own local quantity (its queue depth, its cache size)
/// with `set` therefore exports the process total. A value owned by one
/// non-shard thread (a background refresher) lands in the overflow slot, so
/// `set` from there behaves as a plain global set — as long as shards do not
/// also write the same gauge.
#[derive(Clone, Debug)]
pub struct Gauge(pub(crate) Arc<GaugeCore>);

#[derive(Debug)]
pub(crate) struct GaugeCore {
    slots: Box<[Padded<AtomicI64>]>,
}

impl GaugeCore {
    pub(crate) fn new(slots: usize) -> Self {
        Self {
            slots: (0..slots).map(|_| Padded::default()).collect(),
        }
    }
}

impl Gauge {
    /// A gauge not attached to any registry. See [`Counter::detached`].
    pub fn detached(shard_slots: usize) -> Self {
        Self(Arc::new(GaugeCore::new(shard_slots + 1)))
    }

    #[inline]
    fn slot(&self) -> &AtomicI64 {
        let slots = &self.0.slots;
        &slots[current_slot(slots.len())].0
    }

    /// Add one.
    #[inline]
    pub fn inc(&self) {
        self.add(1);
    }

    /// Subtract one.
    #[inline]
    pub fn dec(&self) {
        self.add(-1);
    }

    /// Add `n` (negative to subtract).
    #[inline]
    pub fn add(&self, n: i64) {
        self.slot().fetch_add(n, Relaxed);
    }

    /// Subtract `n`.
    #[inline]
    pub fn sub(&self, n: i64) {
        self.add(n.wrapping_neg());
    }

    /// Set **this shard's contribution** to `v` (see the type docs).
    #[inline]
    pub fn set(&self, v: i64) {
        self.slot().store(v, Relaxed);
    }

    /// The current value: the sum of every slot.
    pub fn get(&self) -> i64 {
        self.0
            .slots
            .iter()
            .fold(0i64, |acc, s| acc.wrapping_add(s.0.load(Relaxed)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shard::FAKE_SHARD;

    fn on_shard<R: Send>(index: Option<usize>, f: impl FnOnce() -> R + Send) -> R {
        std::thread::scope(|s| {
            s.spawn(move || {
                FAKE_SHARD.with(|c| c.set(index));
                f()
            })
            .join()
            .unwrap()
        })
    }

    #[test]
    fn counter_sums_across_shards_and_overflow() {
        let c = Counter::detached(4);
        std::thread::scope(|s| {
            for shard in [Some(0), Some(1), Some(2), Some(3), Some(99), None] {
                let c = c.clone();
                s.spawn(move || {
                    FAKE_SHARD.with(|cell| cell.set(shard));
                    for _ in 0..10_000 {
                        c.inc();
                    }
                    c.add(5);
                });
            }
        });
        assert_eq!(c.get(), 6 * 10_005);
        // Shards 0..4 each wrote only their own slot; 99 and None shared overflow.
        for i in 0..4 {
            assert_eq!(c.0.slots[i].0.load(Relaxed), 10_005, "slot {i}");
        }
        assert_eq!(c.0.slots[4].0.load(Relaxed), 2 * 10_005);
    }

    #[test]
    fn gauge_add_sub_and_per_shard_set() {
        let g = Gauge::detached(2);
        on_shard(Some(0), || {
            g.inc();
            g.inc();
            g.dec();
        });
        on_shard(Some(1), || g.add(10));
        on_shard(None, || g.sub(3));
        assert_eq!(g.get(), 1 + 10 - 3);
        // set replaces shard 1's contribution only.
        on_shard(Some(1), || g.set(4));
        assert_eq!(g.get(), 1 + 4 - 3);
    }

    #[test]
    fn slots_are_cache_line_isolated() {
        assert_eq!(std::mem::align_of::<Padded<AtomicU64>>(), 128);
        assert_eq!(std::mem::size_of::<Padded<AtomicU64>>(), 128);
    }
}
