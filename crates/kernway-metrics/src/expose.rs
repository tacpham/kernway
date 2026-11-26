//! Prometheus text exposition format 0.0.4.

use std::fmt::Write;

use crate::registry::{lock, Family, SeriesMetric};
use crate::Registry;

/// The `Content-Type` of the text exposition format.
pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// Escape a label value: `\` → `\\`, `"` → `\"`, newline → `\n`.
pub fn escape_label_value(value: &str, out: &mut String) {
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

/// Escape `# HELP` text: `\` → `\\`, newline → `\n` (quotes stay as-is).
fn escape_help(help: &str, out: &mut String) {
    for c in help.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

/// `a="x",b="y"`, rendered once per series at registration.
pub(crate) fn render_labels(names: &[String], values: &[&str]) -> String {
    let mut out = String::new();
    for (i, (name, value)) in names.iter().zip(values).enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(name);
        out.push_str("=\"");
        escape_label_value(value, &mut out);
        out.push('"');
    }
    out
}

/// `name{labels} ` or `name ` — the part of a sample line before the value.
fn sample_prefix(out: &mut String, name: &str, suffix: &str, labels: &str, extra: Option<&str>) {
    out.push_str(name);
    out.push_str(suffix);
    if !labels.is_empty() || extra.is_some() {
        out.push('{');
        out.push_str(labels);
        if let Some(extra) = extra {
            if !labels.is_empty() {
                out.push(',');
            }
            out.push_str(extra);
        }
        out.push('}');
    }
    out.push(' ');
}

pub(crate) fn write_header(out: &mut String, name: &str, help: &str, kind: &str) {
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    escape_help(help, out);
    out.push_str("\n# TYPE ");
    out.push_str(name);
    out.push(' ');
    out.push_str(kind);
    out.push('\n');
}

fn write_family(out: &mut String, family: &Family) {
    let series = lock(&family.series);
    if series.is_empty() {
        return; // a vec nobody has resolved a series of yet
    }
    write_header(out, &family.name, &family.help, family.kind.as_str());
    let name = family.name.as_str();
    for s in series.values() {
        match &s.metric {
            SeriesMetric::Counter(c) => {
                sample_prefix(out, name, "", &s.labels, None);
                let _ = writeln!(out, "{}", crate::Counter(c.clone()).get());
            }
            SeriesMetric::Gauge(g) => {
                sample_prefix(out, name, "", &s.labels, None);
                let _ = writeln!(out, "{}", crate::Gauge(g.clone()).get());
            }
            SeriesMetric::Histogram(h) => {
                let (counts, sum) = h.exported();
                let config = &h.layout.config;
                let mut cumulative = 0u64;
                let mut le = String::new();
                for (i, count) in counts.iter().enumerate() {
                    cumulative += count;
                    le.clear();
                    match config.bounds().get(i) {
                        Some(b) => {
                            let _ = write!(le, "le=\"{b}\"");
                        }
                        None => le.push_str("le=\"+Inf\""),
                    }
                    sample_prefix(out, name, "_bucket", &s.labels, Some(&le));
                    let _ = writeln!(out, "{cumulative}");
                }
                sample_prefix(out, name, "_sum", &s.labels, None);
                let _ = writeln!(out, "{}", scaled(sum, config.scale()));
                sample_prefix(out, name, "_count", &s.labels, None);
                let _ = writeln!(out, "{cumulative}");
            }
        }
    }
}

/// `raw * scale`, but divided by `1/scale` when that is (within rounding) a whole number: for
/// the common ns → s scale (`1e-9` is not exactly representable, nor is its
/// reciprocal) dividing by the exact `1e9` prints `1.075839488`, not `1.0758394880000002`.
fn scaled(raw: u64, scale: f64) -> f64 {
    let inverse = (1.0 / scale).round();
    if inverse >= 1.0 && (inverse * scale - 1.0).abs() < 1e-12 {
        raw as f64 / inverse
    } else {
        raw as f64 * scale
    }
}

/// Render every family of `registry` in the Prometheus text format 0.0.4.
///
/// Families are ordered by name and series by label values, so the output is
/// deterministic (diffable, golden-testable). Process metrics, when enabled,
/// follow the registry's families. Cost: one lock per family plus a read of
/// every slot — O(series × shards), done on the scraping thread, never on a
/// recording shard.
pub fn exposition(registry: &Registry) -> String {
    let mut out = String::with_capacity(4096);
    for family in registry.families() {
        write_family(&mut out, &family);
    }
    if registry.exports_process_metrics() {
        crate::process::write(&mut out);
    }
    out
}

/// A `200 OK` [`Response`](kernway_core::response::Response) carrying
/// [`exposition`] with the 0.0.4 content type — the body of a `/metrics` route:
///
/// ```ignore
/// App::new().get("/metrics", |_req: Request, _scope: &RequestScope| async {
///     kernway_metrics::response(kernway_metrics::global())
/// })
/// ```
pub fn response(registry: &Registry) -> kernway_core::response::Response {
    kernway_core::response::Response::new(kernway_core::error::StatusCode::OK)
        .content_type(CONTENT_TYPE)
        .body(exposition(registry))
}
