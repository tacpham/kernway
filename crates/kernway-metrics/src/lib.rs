//! # kernway-metrics
//!
//! Prometheus-compatible metrics built for Kernway's thread-per-core runtime:
//! recording is one uncontended `Relaxed` atomic add into the calling shard's
//! own cache line; all the summing happens on scrape.
//!
//! - [`Counter`] (`u64`, monotonic), [`Gauge`] (`i64`, up/down) and
//!   [`Histogram`] (log-linear fine buckets, exported as Prometheus `le`
//!   buckets) — cheap `Arc` handles, created once at startup.
//! - [`Registry`] holds families (name, help, type, label names) and their
//!   series; labels are resolved at registration ([`CounterVec::with`]), never
//!   per call — no hashing or allocation on the hot path.
//! - [`exposition`] renders the text format 0.0.4; [`response`] wraps it in a
//!   kernway [`Response`](kernway_core::response::Response).
//!
//! ```
//! use kernway_metrics::{HistogramConfig, Registry};
//!
//! let reg = Registry::new();
//! let requests = reg.counter_vec("http_requests_total", "Requests served.", &["code"])?;
//! let ok = requests.with(&["200"]);           // resolve once, keep the handle
//! let latency = reg.histogram(
//!     "http_request_duration_seconds",
//!     "Request latency.",
//!     HistogramConfig::latency_seconds(),     // records ns, exports seconds
//! )?;
//!
//! // hot path, on any shard:
//! let timer = latency.start_timer();
//! ok.inc();
//! drop(timer);
//!
//! let text = kernway_metrics::exposition(&reg);
//! assert!(text.contains("http_requests_total{code=\"200\"} 1\n"));
//! # Ok::<(), kernway_metrics::MetricsError>(())
//! ```
//!
//! Serving it from a kernway app:
//!
//! ```ignore
//! App::new().get("/metrics", |_req: Request, _scope: &RequestScope| async {
//!     kernway_metrics::response(kernway_metrics::global())
//! })
//! ```
//!
//! See the crate README for the design and benchmark numbers.

mod expose;
pub mod histogram;
mod process;
mod registry;
mod scalar;
mod shard;

use std::sync::OnceLock;

pub use expose::{escape_label_value, exposition, response, CONTENT_TYPE};
pub use histogram::{
    exponential_buckets, Histogram, HistogramConfig, Timer, DEFAULT_LATENCY_BUCKETS,
};
pub use registry::{CounterVec, GaugeVec, HistogramVec, MetricKind, Registry};
pub use scalar::{Counter, Gauge};

/// Why a registration was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum MetricsError {
    /// Not a valid Prometheus metric name (`[a-zA-Z_:][a-zA-Z0-9_:]*`).
    #[error("invalid metric name {0:?}")]
    InvalidName(String),
    /// Not a valid label name (`[a-zA-Z_][a-zA-Z0-9_]*`, no `__` prefix, no
    /// duplicates, no `le` on a histogram).
    #[error("invalid label name {0:?}")]
    InvalidLabelName(String),
    /// The name is already registered with a different kind, help, label
    /// names or histogram config.
    #[error("metric {name:?} conflicts with an existing registration: {reason}")]
    Conflict {
        /// The metric name.
        name: String,
        /// What differs.
        reason: String,
    },
    /// [`CounterVec::with`] (and friends) got the wrong number of label values.
    #[error("metric {name:?} has {expected} labels, got {got} values")]
    LabelCount {
        /// The metric name.
        name: String,
        /// Label names declared at registration.
        expected: usize,
        /// Values supplied.
        got: usize,
    },
    /// Histogram bounds that are not finite and strictly increasing.
    #[error("invalid histogram buckets: {0}")]
    InvalidBuckets(String),
}

/// The process-wide default registry (one slot per CPU, process metrics on).
///
/// Explicit [`Registry`] values work the same way; this is for apps that want
/// one shared registry without threading it through their state.
pub fn global() -> &'static Registry {
    static GLOBAL: OnceLock<Registry> = OnceLock::new();
    GLOBAL.get_or_init(|| Registry::new().with_process_metrics(true))
}
