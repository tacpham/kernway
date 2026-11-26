//! [`Histogram`]: log-linear fine buckets per shard, merged into Prometheus
//! `le` buckets on scrape.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::shard::current_slot;
use crate::MetricsError;

/// The internal, fixed bucket layout every histogram records into.
///
/// Log-linear ("HDR-like"): values `0..32` get one bucket each; above that,
/// every power of two `[2^e, 2^(e+1))` is split into 16 (`SUB_BUCKETS`) equal
/// sub-buckets. A bucket's width is therefore at most 1/16 of its lower bound
/// — a **6.25 % relative width**, so reporting a bucket's midpoint is within
/// ~3.1 % of any value in it — across the *whole* `u64` range, with no
/// configuration and no allocation per value.
///
/// Four significant bits rather than three: three would halve the memory
/// (4 KiB instead of 8 KiB per shard per series) but double the error to
/// 12.5 %, which is coarse for latency SLOs; four keeps p99 estimates useful.
///
/// The index is branch-free: `leading_zeros`, a shift, an add.
pub mod buckets {
    /// Significant bits kept below the leading one.
    pub const SUB_BITS: u32 = 4;
    /// Sub-buckets per power of two.
    pub const SUB_BUCKETS: usize = 1 << SUB_BITS;
    /// Total fine buckets covering `0..=u64::MAX`.
    pub const NUM_BUCKETS: usize = (64 - SUB_BITS as usize) * SUB_BUCKETS + SUB_BUCKETS;

    /// The fine bucket holding `v`.
    #[inline(always)]
    pub fn index(v: u64) -> usize {
        // `| SUB_BUCKETS` floors the exponent at SUB_BITS, so values below
        // 2*SUB_BUCKETS fall through with shift 0 and index == v: no branch.
        let exp = 63 - (v | SUB_BUCKETS as u64).leading_zeros();
        let shift = exp - SUB_BITS;
        ((shift as usize) << SUB_BITS) + (v >> shift) as usize
    }

    /// The inclusive value range `(lowest, highest)` of fine bucket `i`.
    ///
    /// # Panics
    /// If `i >= NUM_BUCKETS`.
    pub fn bounds(i: usize) -> (u64, u64) {
        assert!(i < NUM_BUCKETS, "bucket {i} out of range");
        let shift = (i >> SUB_BITS).saturating_sub(1) as u32;
        let mantissa = (i - ((shift as usize) << SUB_BITS)) as u64;
        let low = mantissa << shift;
        // The last bucket's exclusive end is 2^64: wrap to 0, minus 1 = MAX.
        let high = ((mantissa + 1) << shift).wrapping_sub(1);
        (low, high)
    }

    /// The value reported for a sample in bucket `i`: its midpoint.
    pub fn midpoint(i: usize) -> u64 {
        let (low, high) = bounds(i);
        low + (high - low) / 2
    }
}

use buckets::NUM_BUCKETS;

/// The Prometheus histogram buckets of the `prometheus` Go client
/// (`.005 … 10`), in seconds.
pub const DEFAULT_LATENCY_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// `count` exponential bounds starting at `start`, each `factor` times the last.
///
/// # Panics
/// If `start <= 0`, `factor <= 1` or `count == 0`.
pub fn exponential_buckets(start: f64, factor: f64, count: usize) -> Vec<f64> {
    assert!(
        start > 0.0 && factor > 1.0 && count > 0,
        "invalid exponential buckets"
    );
    let mut v = start;
    (0..count)
        .map(|_| {
            let b = v;
            v *= factor;
            b
        })
        .collect()
}

/// How a histogram is exported: its `le` boundaries and the unit scale.
///
/// Values are **recorded as `u64` in a base unit** (nanoseconds, bytes, …) and
/// **exported multiplied by `scale`** — so a latency histogram records
/// nanoseconds (an integer, cheap to bucket) and exports seconds (the
/// Prometheus convention). `bounds` are in exported units; `+Inf` is implicit.
///
/// Every recorded value lands in a fine internal bucket (see [`buckets`]); a
/// scrape adds each fine bucket into the first exported bound that is `>=` the
/// fine bucket's *highest* value. A fine bucket that straddles a bound is
/// therefore counted under the next bound up: an exported bucket can
/// under-count values within 6.25 % below its bound — never over-count.
/// Bounds that are powers of two in base units are exact.
#[derive(Clone, Debug, PartialEq)]
pub struct HistogramConfig {
    bounds: Vec<f64>,
    scale: f64,
}

impl HistogramConfig {
    /// Export with these `le` bounds (strictly increasing, finite; a trailing
    /// `+Inf` is accepted and dropped) and scale 1.
    pub fn new(bounds: &[f64]) -> Result<Self, MetricsError> {
        let mut bounds = bounds.to_vec();
        if bounds.last() == Some(&f64::INFINITY) {
            bounds.pop();
        }
        let increasing = bounds.windows(2).all(|w| w[0] < w[1]);
        if !increasing || bounds.iter().any(|b| !b.is_finite()) {
            return Err(MetricsError::InvalidBuckets(
                "bounds must be finite and strictly increasing".into(),
            ));
        }
        Ok(Self { bounds, scale: 1.0 })
    }

    /// Exported value = recorded value × `scale` (e.g. `1e-9` for ns → s).
    ///
    /// # Panics
    /// If `scale` is not finite and positive.
    pub fn with_scale(mut self, scale: f64) -> Self {
        assert!(scale.is_finite() && scale > 0.0, "scale must be positive");
        self.scale = scale;
        self
    }

    /// Request latency: record **nanoseconds** (e.g. via [`Histogram::start_timer`]),
    /// export **seconds** with [`DEFAULT_LATENCY_BUCKETS`].
    pub fn latency_seconds() -> Self {
        Self {
            bounds: DEFAULT_LATENCY_BUCKETS.to_vec(),
            scale: 1e-9,
        }
    }

    /// Sizes in bytes: `64 B … 16 MiB`, ×4 per bucket.
    pub fn bytes() -> Self {
        Self {
            bounds: exponential_buckets(64.0, 4.0, 10),
            scale: 1.0,
        }
    }

    /// The exported `le` bounds (without `+Inf`).
    pub fn bounds(&self) -> &[f64] {
        &self.bounds
    }

    /// The export scale.
    pub fn scale(&self) -> f64 {
        self.scale
    }
}

/// A config plus the precomputed fine-bucket → exported-bucket map, shared by
/// every series of a family.
#[derive(Debug)]
pub(crate) struct Layout {
    pub(crate) config: HistogramConfig,
    /// `export_of[fine]` = index into `config.bounds`, or `bounds.len()` (+Inf).
    export_of: Box<[u16]>,
}

impl Layout {
    pub(crate) fn new(config: HistogramConfig) -> Self {
        let export_of = (0..NUM_BUCKETS)
            .map(|i| {
                let high = buckets::bounds(i).1 as f64 * config.scale;
                config
                    .bounds
                    .iter()
                    .position(|&b| high <= b)
                    .unwrap_or(config.bounds.len()) as u16
            })
            .collect();
        Self { config, export_of }
    }
}

/// One shard's fine buckets and sum, on its own cache lines.
#[repr(align(128))]
struct ShardBuckets {
    buckets: [AtomicU64; NUM_BUCKETS],
    sum: AtomicU64,
}

impl ShardBuckets {
    fn boxed() -> Box<Self> {
        #[allow(clippy::declare_interior_mutable_const)] // a template for the array, never shared
        const ZERO: AtomicU64 = AtomicU64::new(0);
        Box::new(Self {
            buckets: [ZERO; NUM_BUCKETS],
            sum: ZERO,
        })
    }
}

pub(crate) struct HistogramCore {
    /// Allocated on a shard's first record: ~7.8 KiB each, so a 64-core box
    /// with a labelled histogram per route does not pay for idle shards.
    shards: Box<[OnceLock<Box<ShardBuckets>>]>,
    pub(crate) layout: Arc<Layout>,
}

impl std::fmt::Debug for HistogramCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistogramCore")
            .field("slots", &self.shards.len())
            .field("config", &self.layout.config)
            .finish()
    }
}

impl HistogramCore {
    pub(crate) fn new(slots: usize, layout: Arc<Layout>) -> Self {
        Self {
            shards: (0..slots).map(|_| OnceLock::new()).collect(),
            layout,
        }
    }

    /// Fine buckets and sum, summed over every shard.
    fn merged(&self) -> (Vec<u64>, u64) {
        let mut fine = vec![0u64; NUM_BUCKETS];
        let mut sum = 0u64;
        for shard in self.shards.iter().filter_map(OnceLock::get) {
            for (acc, b) in fine.iter_mut().zip(shard.buckets.iter()) {
                *acc = acc.wrapping_add(b.load(Relaxed));
            }
            sum = sum.wrapping_add(shard.sum.load(Relaxed));
        }
        (fine, sum)
    }

    /// Non-cumulative counts per exported bucket (last = +Inf), and the raw sum.
    pub(crate) fn exported(&self) -> (Vec<u64>, u64) {
        let (fine, sum) = self.merged();
        let mut out = vec![0u64; self.layout.config.bounds.len() + 1];
        for (count, &to) in fine.iter().zip(self.layout.export_of.iter()) {
            out[to as usize] += count;
        }
        (out, sum)
    }
}

/// A distribution of `u64` samples (latencies, sizes).
///
/// A cheap handle (one `Arc`). [`record`](Self::record) computes the fine
/// bucket with `leading_zeros` + shift and does **two `Relaxed` adds** into
/// the calling shard's own bucket array — the bucket and the sum. There is no
/// separate count: it is the sum of the buckets, computed on scrape, which
/// also keeps `_count` exactly equal to the `+Inf` bucket.
#[derive(Clone, Debug)]
pub struct Histogram(pub(crate) Arc<HistogramCore>);

impl Histogram {
    /// A histogram not attached to any registry. See
    /// [`Counter::detached`](crate::Counter::detached).
    pub fn detached(shard_slots: usize, config: HistogramConfig) -> Self {
        Self(Arc::new(HistogramCore::new(
            shard_slots + 1,
            Arc::new(Layout::new(config)),
        )))
    }

    /// Record one sample, in base units (ns for a latency histogram).
    #[inline]
    pub fn record(&self, v: u64) {
        let shards = &self.0.shards;
        let shard = shards[current_slot(shards.len())].get_or_init(ShardBuckets::boxed);
        shard.buckets[buckets::index(v)].fetch_add(1, Relaxed);
        shard.sum.fetch_add(v, Relaxed);
    }

    /// Record a duration as nanoseconds (saturating at `u64::MAX`).
    #[inline]
    pub fn record_duration(&self, d: Duration) {
        self.record(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    }

    /// Start a [`Timer`] that records the elapsed nanoseconds when dropped.
    ///
    /// Costs two `Instant::now()` reads on top of `record` — a vDSO/commpage
    /// clock read, ~10–25 ns each depending on platform. For per-request
    /// latency that is noise; inside a tight inner loop, sample instead.
    #[inline]
    pub fn start_timer(&self) -> Timer<'_> {
        Timer {
            histogram: self,
            start: Instant::now(),
            armed: true,
        }
    }

    /// Number of samples recorded (all shards).
    pub fn count(&self) -> u64 {
        self.0.merged().0.iter().sum()
    }

    /// Sum of samples recorded, in base units (wrapping).
    pub fn sum(&self) -> u64 {
        self.0.merged().1
    }

    /// Estimate the `q`-quantile (`0.0..=1.0`) in base units, or `None` if
    /// empty. Returns the midpoint of the fine bucket holding the sample of
    /// rank `ceil(q * count)`: within ~3.1 % of the true value (exact below 32).
    /// For tests and development dashboards — Prometheus computes quantiles
    /// server-side from the exported buckets.
    pub fn quantile(&self, q: f64) -> Option<u64> {
        let (fine, _) = self.0.merged();
        let total: u64 = fine.iter().sum();
        if total == 0 {
            return None;
        }
        let rank = ((q.clamp(0.0, 1.0) * total as f64).ceil() as u64).max(1);
        let mut seen = 0u64;
        for (i, &c) in fine.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return Some(buckets::midpoint(i));
            }
        }
        None // unreachable: seen ends at total >= rank
    }

    /// This histogram's export config.
    pub fn config(&self) -> &HistogramConfig {
        &self.0.layout.config
    }
}

/// Records the time since [`Histogram::start_timer`] into the histogram, in
/// nanoseconds, when dropped (or at [`stop`](Self::stop)).
#[must_use = "a Timer records when dropped; binding it to `_` drops it immediately"]
#[derive(Debug)]
pub struct Timer<'a> {
    histogram: &'a Histogram,
    start: Instant,
    armed: bool,
}

impl Timer<'_> {
    /// Record now and return the elapsed time.
    pub fn stop(mut self) -> Duration {
        let elapsed = self.start.elapsed();
        self.histogram.record_duration(elapsed);
        self.armed = false;
        elapsed
    }

    /// Drop without recording (e.g. the request was aborted).
    pub fn discard(mut self) {
        self.armed = false;
    }
}

impl Drop for Timer<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.histogram.record_duration(self.start.elapsed());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::buckets::{bounds, index, midpoint, NUM_BUCKETS, SUB_BUCKETS};
    use super::*;
    use crate::shard::FAKE_SHARD;

    #[test]
    fn small_values_are_exact() {
        for v in 0..(2 * SUB_BUCKETS as u64) {
            assert_eq!(index(v), v as usize);
            assert_eq!(bounds(v as usize), (v, v));
        }
    }

    #[test]
    fn buckets_tile_the_u64_range_monotonically() {
        assert_eq!(NUM_BUCKETS, 976);
        assert_eq!(bounds(0).0, 0);
        assert_eq!(bounds(NUM_BUCKETS - 1).1, u64::MAX);
        for i in 1..NUM_BUCKETS {
            let (prev_lo, prev_hi) = bounds(i - 1);
            let (lo, hi) = bounds(i);
            assert!(prev_lo <= prev_hi && lo <= hi);
            assert_eq!(lo, prev_hi + 1, "gap or overlap at bucket {i}");
        }
        assert_eq!(index(u64::MAX), NUM_BUCKETS - 1);
    }

    #[test]
    fn index_and_bounds_agree() {
        for i in 0..NUM_BUCKETS {
            let (lo, hi) = bounds(i);
            assert_eq!(index(lo), i);
            assert_eq!(index(hi), i);
            assert_eq!(index(midpoint(i)), i);
        }
        // and a pseudo-random sweep of values
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..100_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let v = x >> (x % 64);
            let (lo, hi) = bounds(index(v));
            assert!(lo <= v && v <= hi, "{v} not in bucket {}", index(v));
        }
    }

    #[test]
    fn relative_width_is_bounded() {
        for i in 2 * SUB_BUCKETS..NUM_BUCKETS {
            let (lo, hi) = bounds(i);
            let width = (hi - lo) as f64 + 1.0;
            assert!(width / lo as f64 <= 1.0 / 16.0 + 1e-12, "bucket {i}");
            // midpoint error relative to any value in the bucket
            let mid = midpoint(i) as f64;
            for v in [lo, hi] {
                assert!((mid - v as f64).abs() / v as f64 <= 1.0 / 32.0 + 1e-9);
            }
        }
    }

    #[test]
    fn quantiles_of_a_uniform_distribution() {
        let h = Histogram::detached(2, HistogramConfig::latency_seconds());
        assert_eq!(h.quantile(0.5), None);
        for v in 1..=100_000u64 {
            h.record(v);
        }
        assert_eq!(h.count(), 100_000);
        assert_eq!(h.sum(), 100_000 * 100_001 / 2);
        for q in [0.01, 0.1, 0.5, 0.9, 0.99, 0.999] {
            let want = q * 100_000.0;
            let got = h.quantile(q).unwrap() as f64;
            assert!((got - want).abs() / want < 0.035, "q{q}: {got} vs {want}");
        }
        assert_eq!(h.quantile(0.0), Some(1));
        let max = h.quantile(1.0).unwrap() as f64;
        assert!((max - 100_000.0).abs() / 100_000.0 < 0.035);
    }

    #[test]
    fn shards_merge() {
        let h = Histogram::detached(3, HistogramConfig::new(&[10.0, 100.0]).unwrap());
        std::thread::scope(|s| {
            for shard in [Some(0), Some(1), Some(2), None] {
                let h = h.clone();
                s.spawn(move || {
                    FAKE_SHARD.with(|c| c.set(shard));
                    h.record(5);
                    h.record(50);
                    h.record(500);
                });
            }
        });
        let (counts, sum) = h.0.exported();
        assert_eq!(counts, vec![4, 4, 4]);
        assert_eq!(sum, 4 * 555);
    }

    #[test]
    fn straddling_buckets_go_up_never_down() {
        // 100 is inside fine bucket [100, 103]: values there count under le=200.
        let h = Histogram::detached(1, HistogramConfig::new(&[100.0, 200.0]).unwrap());
        h.record(99);
        h.record(100);
        h.record(103);
        h.record(104);
        let (counts, _) = h.0.exported();
        assert_eq!(counts, vec![1, 3, 0]);
    }

    #[test]
    fn timer_records_nanoseconds() {
        let h = Histogram::detached(1, HistogramConfig::latency_seconds());
        {
            let _t = h.start_timer();
            std::thread::sleep(Duration::from_millis(2));
        }
        let d = h.start_timer().stop();
        h.start_timer().discard();
        assert_eq!(h.count(), 2);
        assert!(h.sum() >= 2_000_000 + d.as_nanos() as u64);
    }

    #[test]
    fn invalid_bounds_are_rejected() {
        assert!(HistogramConfig::new(&[1.0, 1.0]).is_err());
        assert!(HistogramConfig::new(&[2.0, 1.0]).is_err());
        assert!(HistogramConfig::new(&[f64::NAN]).is_err());
        let c = HistogramConfig::new(&[1.0, f64::INFINITY]).unwrap();
        assert_eq!(c.bounds(), &[1.0]);
    }
}
