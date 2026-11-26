//! [`Registry`]: metric families by name, series by label values.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::histogram::{HistogramCore, Layout};
use crate::scalar::{CounterCore, GaugeCore};
use crate::shard::default_shard_slots;
use crate::{Counter, Gauge, Histogram, HistogramConfig, MetricsError};

/// What a family exports as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricKind {
    /// `# TYPE … counter`
    Counter,
    /// `# TYPE … gauge`
    Gauge,
    /// `# TYPE … histogram`
    Histogram,
}

impl MetricKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum SeriesMetric {
    Counter(Arc<CounterCore>),
    Gauge(Arc<GaugeCore>),
    Histogram(Arc<HistogramCore>),
}

#[derive(Debug)]
pub(crate) struct Series {
    /// `a="x",b="y"` — escaped and joined once, at registration.
    pub(crate) labels: String,
    pub(crate) metric: SeriesMetric,
}

#[derive(Debug)]
pub(crate) struct Family {
    pub(crate) name: String,
    pub(crate) help: String,
    pub(crate) kind: MetricKind,
    pub(crate) label_names: Vec<String>,
    pub(crate) layout: Option<Arc<Layout>>,
    slots: usize,
    /// Keyed by label values, so exposition order is deterministic.
    pub(crate) series: Mutex<BTreeMap<Vec<String>, Series>>,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while holding a registry lock leaves the maps consistent (every
    // mutation is a single insert), so poisoning is not a reason to stop exporting.
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Family {
    fn series(&self, values: &[&str]) -> Result<SeriesMetric, MetricsError> {
        if values.len() != self.label_names.len() {
            return Err(MetricsError::LabelCount {
                name: self.name.clone(),
                expected: self.label_names.len(),
                got: values.len(),
            });
        }
        let key: Vec<String> = values.iter().map(|v| (*v).to_owned()).collect();
        let mut series = lock(&self.series);
        if let Some(existing) = series.get(&key) {
            return Ok(existing.metric.clone());
        }
        let metric = match self.kind {
            MetricKind::Counter => SeriesMetric::Counter(Arc::new(CounterCore::new(self.slots))),
            MetricKind::Gauge => SeriesMetric::Gauge(Arc::new(GaugeCore::new(self.slots))),
            MetricKind::Histogram => SeriesMetric::Histogram(Arc::new(HistogramCore::new(
                self.slots,
                Arc::clone(self.layout.as_ref().expect("histogram family has a layout")),
            ))),
        };
        let labels = crate::expose::render_labels(&self.label_names, values);
        series.insert(
            key,
            Series {
                labels,
                metric: metric.clone(),
            },
        );
        Ok(metric)
    }
}

/// A set of metric families, rendered together by [`exposition`](crate::exposition).
///
/// Registration is the cold path — it takes a lock and allocates — and returns
/// handles ([`Counter`], [`Gauge`], [`Histogram`]) the app keeps; recording
/// through a handle never touches the registry.
///
/// # Duplicate registration
///
/// Registering a name again **with the same kind, help, label names (and, for
/// histograms, the same [`HistogramConfig`]) returns the existing metric** — the
/// same underlying storage, so it is safe to register from every shard's
/// `on_shard_start`, or from two modules that agree on a metric. Any mismatch
/// is a [`MetricsError::Conflict`]: two metrics silently sharing a name would
/// export garbage.
///
/// # Slots
///
/// Every metric gets one write slot per shard (`shard_slots`, default one per
/// CPU — matching `rt_net::ShardConfig`'s default shard count) plus one shared
/// overflow slot for non-shard threads. A shard whose index is `>=
/// shard_slots` also uses the overflow slot: correct, just contended. Size it
/// to the shard count with [`Registry::with_shard_slots`] if you run more
/// shards than CPUs.
#[derive(Debug)]
pub struct Registry {
    families: Mutex<BTreeMap<String, Arc<Family>>>,
    slots: usize,
    process: bool,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// An empty registry with one slot per CPU and no process metrics.
    pub fn new() -> Self {
        Self::with_shard_slots(default_shard_slots())
    }

    /// An empty registry with `shard_slots` per-shard slots (plus overflow).
    pub fn with_shard_slots(shard_slots: usize) -> Self {
        Self {
            families: Mutex::new(BTreeMap::new()),
            slots: shard_slots + 1,
            process: false,
        }
    }

    /// Also export `process_*` metrics (resident memory, open fds, CPU time),
    /// read from the OS on each scrape. On by default for [`global`](crate::global).
    pub fn with_process_metrics(mut self, on: bool) -> Self {
        self.process = on;
        self
    }

    /// Per-shard slots each metric gets (excluding the overflow slot).
    pub fn shard_slots(&self) -> usize {
        self.slots - 1
    }

    pub(crate) fn exports_process_metrics(&self) -> bool {
        self.process
    }

    pub(crate) fn families(&self) -> Vec<Arc<Family>> {
        lock(&self.families).values().cloned().collect()
    }

    fn family(
        &self,
        name: &str,
        help: &str,
        kind: MetricKind,
        labels: &[&str],
        config: Option<HistogramConfig>,
    ) -> Result<Arc<Family>, MetricsError> {
        validate_metric_name(name)?;
        for l in labels {
            validate_label_name(l)?;
            if kind == MetricKind::Histogram && *l == "le" {
                return Err(MetricsError::InvalidLabelName((*l).to_owned()));
            }
        }
        for (i, l) in labels.iter().enumerate() {
            if labels[..i].contains(l) {
                return Err(MetricsError::InvalidLabelName(format!("{l} (duplicate)")));
            }
        }

        let mut families = lock(&self.families);
        if let Some(existing) = families.get(name) {
            let conflict = |reason: &str| {
                Err(MetricsError::Conflict {
                    name: name.to_owned(),
                    reason: reason.to_owned(),
                })
            };
            if existing.kind != kind {
                return conflict(&format!(
                    "already registered as a {}",
                    existing.kind.as_str()
                ));
            }
            if existing.label_names != labels {
                return conflict(&format!(
                    "already registered with labels {:?}",
                    existing.label_names
                ));
            }
            if existing.help != help {
                return conflict("already registered with different help text");
            }
            if existing.layout.as_ref().map(|l| &l.config) != config.as_ref() {
                return conflict("already registered with a different histogram config");
            }
            return Ok(Arc::clone(existing));
        }
        let family = Arc::new(Family {
            name: name.to_owned(),
            help: help.to_owned(),
            kind,
            label_names: labels.iter().map(|l| (*l).to_owned()).collect(),
            layout: config.map(|c| Arc::new(Layout::new(c))),
            slots: self.slots,
            series: Mutex::new(BTreeMap::new()),
        });
        families.insert(name.to_owned(), Arc::clone(&family));
        Ok(family)
    }

    /// Register (or fetch — see the type docs) an unlabelled counter.
    pub fn counter(&self, name: &str, help: &str) -> Result<Counter, MetricsError> {
        self.counter_vec(name, help, &[])?.try_with(&[])
    }

    /// Register (or fetch) an unlabelled gauge.
    pub fn gauge(&self, name: &str, help: &str) -> Result<Gauge, MetricsError> {
        self.gauge_vec(name, help, &[])?.try_with(&[])
    }

    /// Register (or fetch) an unlabelled histogram.
    pub fn histogram(
        &self,
        name: &str,
        help: &str,
        config: HistogramConfig,
    ) -> Result<Histogram, MetricsError> {
        self.histogram_vec(name, help, &[], config)?.try_with(&[])
    }

    /// Register (or fetch) a counter family with these label names.
    pub fn counter_vec(
        &self,
        name: &str,
        help: &str,
        labels: &[&str],
    ) -> Result<CounterVec, MetricsError> {
        self.family(name, help, MetricKind::Counter, labels, None)
            .map(CounterVec)
    }

    /// Register (or fetch) a gauge family with these label names.
    pub fn gauge_vec(
        &self,
        name: &str,
        help: &str,
        labels: &[&str],
    ) -> Result<GaugeVec, MetricsError> {
        self.family(name, help, MetricKind::Gauge, labels, None)
            .map(GaugeVec)
    }

    /// Register (or fetch) a histogram family with these label names.
    pub fn histogram_vec(
        &self,
        name: &str,
        help: &str,
        labels: &[&str],
        config: HistogramConfig,
    ) -> Result<HistogramVec, MetricsError> {
        self.family(name, help, MetricKind::Histogram, labels, Some(config))
            .map(HistogramVec)
    }
}

macro_rules! vec_handle {
    ($(#[$doc:meta])* $vec:ident, $handle:ident, $variant:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug)]
        pub struct $vec(Arc<Family>);

        impl $vec {
            #[doc = concat!("The [`", stringify!($handle), "`] for these label values (in label-name order), created on first use.")]
            ///
            /// Takes a lock and may allocate: call it once per label set at
            /// startup and keep the handle, not once per request.
            ///
            /// # Panics
            /// If the number of values differs from the number of label names.
            pub fn with(&self, values: &[&str]) -> $handle {
                self.try_with(values).unwrap_or_else(|e| panic!("{e}"))
            }

            /// [`with`](Self::with), returning an error instead of panicking.
            pub fn try_with(&self, values: &[&str]) -> Result<$handle, MetricsError> {
                match self.0.series(values)? {
                    SeriesMetric::$variant(core) => Ok($handle(core)),
                    _ => unreachable!("a family only holds series of its own kind"),
                }
            }
        }
    };
}

vec_handle!(
    /// A labelled counter family; [`with`](CounterVec::with) resolves one series.
    CounterVec,
    Counter,
    Counter
);
vec_handle!(
    /// A labelled gauge family; [`with`](GaugeVec::with) resolves one series.
    GaugeVec,
    Gauge,
    Gauge
);
vec_handle!(
    /// A labelled histogram family; [`with`](HistogramVec::with) resolves one series.
    HistogramVec,
    Histogram,
    Histogram
);

/// `[a-zA-Z_:][a-zA-Z0-9_:]*`
fn validate_metric_name(name: &str) -> Result<(), MetricsError> {
    let ok = name.bytes().enumerate().all(|(i, b)| {
        b.is_ascii_alphabetic() || b == b'_' || b == b':' || (i > 0 && b.is_ascii_digit())
    });
    if ok && !name.is_empty() {
        Ok(())
    } else {
        Err(MetricsError::InvalidName(name.to_owned()))
    }
}

/// `[a-zA-Z_][a-zA-Z0-9_]*`, not starting with the reserved `__`.
fn validate_label_name(name: &str) -> Result<(), MetricsError> {
    let ok = name
        .bytes()
        .enumerate()
        .all(|(i, b)| b.is_ascii_alphabetic() || b == b'_' || (i > 0 && b.is_ascii_digit()));
    if ok && !name.is_empty() && !name.starts_with("__") {
        Ok(())
    } else {
        Err(MetricsError::InvalidLabelName(name.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_registration_returns_the_same_storage() {
        let reg = Registry::with_shard_slots(2);
        let a = reg.counter("hits_total", "Hits.").unwrap();
        let b = reg.counter("hits_total", "Hits.").unwrap();
        a.inc();
        b.inc();
        assert!(Arc::ptr_eq(&a.0, &b.0));
        assert_eq!(a.get(), 2);

        let v1 = reg.counter_vec("req_total", "Req.", &["code"]).unwrap();
        let v2 = reg.counter_vec("req_total", "Req.", &["code"]).unwrap();
        v1.with(&["200"]).inc();
        assert_eq!(v2.with(&["200"]).get(), 1);
        assert_eq!(v2.with(&["500"]).get(), 0);
    }

    #[test]
    fn conflicting_registration_is_an_error() {
        let reg = Registry::new();
        reg.counter("x", "X.").unwrap();
        let conflict = |r: Result<(), MetricsError>| {
            assert!(matches!(r, Err(MetricsError::Conflict { .. })), "{r:?}");
        };
        conflict(reg.gauge("x", "X.").map(drop));
        conflict(reg.counter("x", "Other help.").map(drop));
        conflict(reg.counter_vec("x", "X.", &["a"]).map(drop));
        reg.histogram("h", "H.", HistogramConfig::latency_seconds())
            .unwrap();
        reg.histogram("h", "H.", HistogramConfig::latency_seconds())
            .unwrap();
        conflict(reg.histogram("h", "H.", HistogramConfig::bytes()).map(drop));
    }

    #[test]
    fn names_and_labels_are_validated() {
        let reg = Registry::new();
        for bad in ["", "1abc", "a-b", "a b", "é"] {
            assert!(
                matches!(reg.counter(bad, ""), Err(MetricsError::InvalidName(_))),
                "{bad}"
            );
        }
        assert!(reg.counter("ns:sub_total", "").is_ok());
        for bad in ["__x", "a:b", "1a", ""] {
            assert!(matches!(
                reg.counter_vec("c", "", &[bad]),
                Err(MetricsError::InvalidLabelName(_))
            ));
        }
        assert!(reg.counter_vec("c", "", &["a", "a"]).is_err());
        assert!(reg
            .histogram_vec("h", "", &["le"], HistogramConfig::bytes())
            .is_err());
    }

    #[test]
    fn wrong_label_count() {
        let reg = Registry::new();
        let v = reg.counter_vec("c", "", &["a", "b"]).unwrap();
        assert!(matches!(
            v.try_with(&["x"]),
            Err(MetricsError::LabelCount {
                expected: 2,
                got: 1,
                ..
            })
        ));
    }
}
