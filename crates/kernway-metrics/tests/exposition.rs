#![allow(missing_docs)] // a test binary, not public API
//! Golden tests for the text exposition format 0.0.4.

use kernway_metrics::{exposition, response, HistogramConfig, Registry, CONTENT_TYPE};

#[test]
fn golden_exposition() {
    let reg = Registry::with_shard_slots(2);
    // Registered out of order: output is sorted by family name, then label values.
    let lat = reg
        .histogram_vec(
            "rpc_duration_seconds",
            "RPC latency.",
            &["method"],
            HistogramConfig::new(&[0.001, 0.01])
                .unwrap()
                .with_scale(1e-9),
        )
        .unwrap();
    let req = reg
        .counter_vec(
            "http_requests_total",
            "Requests served.",
            &["method", "code"],
        )
        .unwrap();
    let inflight = reg.gauge("in_flight", "In-flight requests.").unwrap();
    let _unused = reg
        .counter_vec("never_used_total", "No series yet.", &["x"])
        .unwrap();

    req.with(&["POST", "500"]).inc();
    req.with(&["GET", "200"]).add(3);
    inflight.add(2);
    inflight.dec();
    let get = lat.with(&["get"]);
    get.record(512); // 512 ns  -> le=0.001
    get.record(2_097_152); // ~2.1 ms -> le=0.01
    get.record(1 << 30); // ~1.07 s -> +Inf
    lat.with(&["put"]); // registered, empty

    let expected = "\
# HELP http_requests_total Requests served.
# TYPE http_requests_total counter
http_requests_total{method=\"GET\",code=\"200\"} 3
http_requests_total{method=\"POST\",code=\"500\"} 1
# HELP in_flight In-flight requests.
# TYPE in_flight gauge
in_flight 1
# HELP rpc_duration_seconds RPC latency.
# TYPE rpc_duration_seconds histogram
rpc_duration_seconds_bucket{method=\"get\",le=\"0.001\"} 1
rpc_duration_seconds_bucket{method=\"get\",le=\"0.01\"} 2
rpc_duration_seconds_bucket{method=\"get\",le=\"+Inf\"} 3
rpc_duration_seconds_sum{method=\"get\"} 1.075839488
rpc_duration_seconds_count{method=\"get\"} 3
rpc_duration_seconds_bucket{method=\"put\",le=\"0.001\"} 0
rpc_duration_seconds_bucket{method=\"put\",le=\"0.01\"} 0
rpc_duration_seconds_bucket{method=\"put\",le=\"+Inf\"} 0
rpc_duration_seconds_sum{method=\"put\"} 0
rpc_duration_seconds_count{method=\"put\"} 0
";
    assert_eq!(exposition(&reg), expected);
}

#[test]
fn label_values_and_help_are_escaped() {
    let reg = Registry::with_shard_slots(1);
    reg.counter_vec(
        "c_total",
        "Line one\nback\\slash \"quotes\" kept.",
        &["path"],
    )
    .unwrap()
    .with(&["a\"b\\c\nd"])
    .inc();
    let text = exposition(&reg);
    assert_eq!(
        text,
        "# HELP c_total Line one\\nback\\\\slash \"quotes\" kept.\n\
         # TYPE c_total counter\n\
         c_total{path=\"a\\\"b\\\\c\\nd\"} 1\n"
    );
}

#[test]
fn response_has_the_exposition_content_type() {
    let reg = Registry::with_shard_slots(1);
    reg.counter("x_total", "X.").unwrap().inc();
    let resp = response(&reg);
    assert_eq!(resp.status.0, 200);
    assert_eq!(resp.headers.get("content-type"), Some(CONTENT_TYPE));
    assert_eq!(
        resp.body_bytes(),
        b"# HELP x_total X.\n# TYPE x_total counter\nx_total 1\n"
    );
}

#[test]
fn process_metrics_are_appended_when_enabled() {
    let reg = Registry::with_shard_slots(1).with_process_metrics(true);
    let text = exposition(&reg);
    if cfg!(any(target_os = "macos", target_os = "linux")) {
        assert!(
            text.contains("# TYPE process_resident_memory_bytes gauge\n"),
            "{text}"
        );
        assert!(text.contains("\nprocess_open_fds "));
        assert!(text.contains("\nprocess_cpu_seconds_total "));
    }
    assert!(!exposition(&Registry::with_shard_slots(1)).contains("process_"));
}
