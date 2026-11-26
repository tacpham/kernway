//! A log-linear latency histogram in the spirit of HdrHistogram.
//!
//! Values (nanoseconds) below `2^SUB_BITS` get one bucket each; above that,
//! every power-of-two range `[2^e, 2^(e+1))` is split into `2^SUB_BITS` equal
//! sub-buckets. The relative error of any reported percentile is therefore at
//! most `1 / 2^SUB_BITS` (0.78 % with 7 bits) over the whole `u64` range, with a
//! fixed 7 424-slot array: recording is one `leading_zeros`, a shift and an
//! increment, and two histograms merge by adding slots.
//!
//! Percentiles report the **upper edge** of the bucket holding the rank, so a
//! reported p99 is never *better* than the true one. `min`/`max`/`mean` are
//! kept exactly.

const SUB_BITS: u32 = 7;
const SUB: usize = 1 << SUB_BITS;
/// Buckets needed to cover every `u64`: one linear block, then one block per
/// exponent from `SUB_BITS` to 63.
const BUCKETS: usize = (64 - SUB_BITS as usize + 1) * SUB;

/// A mergeable log-linear histogram of `u64` samples.
#[derive(Clone)]
pub struct Histogram {
    counts: Vec<u64>,
    total: u64,
    min: u64,
    max: u64,
    sum: u128,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    /// An empty histogram.
    #[must_use]
    pub fn new() -> Self {
        Self {
            counts: vec![0; BUCKETS],
            total: 0,
            min: u64::MAX,
            max: 0,
            sum: 0,
        }
    }

    fn index(v: u64) -> usize {
        if v < SUB as u64 {
            return v as usize;
        }
        let e = 63 - v.leading_zeros(); // >= SUB_BITS
        let shift = e - SUB_BITS;
        let mantissa = (v >> shift) as usize; // in [SUB, 2*SUB)
        ((shift as usize + 1) << SUB_BITS) + (mantissa - SUB)
    }

    /// The largest value that lands in bucket `idx`.
    fn upper(idx: usize) -> u64 {
        if idx < SUB {
            return idx as u64;
        }
        let shift = (idx >> SUB_BITS) - 1;
        let mantissa = ((idx & (SUB - 1)) + SUB) as u128;
        let hi = ((mantissa + 1) << shift) - 1;
        u64::try_from(hi).unwrap_or(u64::MAX)
    }

    /// Record one sample.
    pub fn record(&mut self, v: u64) {
        self.counts[Self::index(v)] += 1;
        self.total += 1;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
        self.sum += u128::from(v);
    }

    /// Add every sample of `other` into `self`.
    pub fn merge(&mut self, other: &Histogram) {
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += b;
        }
        self.total += other.total;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.sum += other.sum;
    }

    /// Samples recorded.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.total
    }

    /// Smallest sample (0 when empty).
    #[must_use]
    pub fn min(&self) -> u64 {
        if self.total == 0 {
            0
        } else {
            self.min
        }
    }

    /// Largest sample (exact).
    #[must_use]
    pub fn max(&self) -> u64 {
        self.max
    }

    /// Arithmetic mean (exact).
    #[must_use]
    pub fn mean(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.sum as f64 / self.total as f64
        }
    }

    /// The value at quantile `q` in `[0, 1]`: the upper edge of the bucket
    /// holding the `ceil(q * count)`-th smallest sample, capped at `max`.
    #[must_use]
    pub fn quantile(&self, q: f64) -> u64 {
        if self.total == 0 {
            return 0;
        }
        let rank = ((q.clamp(0.0, 1.0) * self.total as f64).ceil() as u64).max(1);
        let mut seen = 0u64;
        for (idx, &c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= rank {
                return Self::upper(idx).min(self.max);
            }
        }
        self.max
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_and_upper_bound_agree_everywhere() {
        let probes = (0..2000u64)
            .chain((0..64).map(|e| 1u64 << e))
            .chain((1..64).map(|e| (1u64 << e) - 1))
            .chain([u64::MAX, 123_456_789, 999_999_999_999]);
        for v in probes {
            let idx = Histogram::index(v);
            assert!(idx < BUCKETS, "{v}");
            assert!(Histogram::upper(idx) >= v, "{v}");
            if idx > 0 {
                assert!(Histogram::upper(idx - 1) < v, "{v}");
            }
        }
    }

    #[test]
    fn percentiles_are_within_relative_error() {
        let mut h = Histogram::new();
        for v in 1..=100_000u64 {
            h.record(v * 1000); // 1 µs .. 100 ms
        }
        for (q, exact) in [
            (0.5, 50_000_000u64),
            (0.99, 99_000_000),
            (0.999, 99_900_000),
        ] {
            let got = h.quantile(q);
            let err = (got as f64 - exact as f64) / exact as f64;
            assert!(
                (0.0..=1.0 / 128.0).contains(&err),
                "q={q} got={got} err={err}"
            );
        }
        assert_eq!(h.quantile(1.0), 100_000_000);
        assert_eq!(h.min(), 1000);
        assert_eq!(h.count(), 100_000);
    }

    #[test]
    fn merge_equals_recording_together() {
        let (mut a, mut b, mut all) = (Histogram::new(), Histogram::new(), Histogram::new());
        for v in 0..10_000u64 {
            let x = v * v;
            if v % 2 == 0 {
                a.record(x)
            } else {
                b.record(x)
            }
            all.record(x);
        }
        a.merge(&b);
        for q in [0.1, 0.5, 0.9, 0.99, 0.999] {
            assert_eq!(a.quantile(q), all.quantile(q));
        }
        assert_eq!(a.max(), all.max());
    }
}
