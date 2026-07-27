#![allow(missing_docs)] // a benchmark binary, not public API
//! Head-to-head: kernway-htmx vs the Rust incumbents.
//!
//! The question this answers is the one that decides whether the crate earns its
//! place (KEP-0000 §2): is kernway's native `HX-*` handling actually faster than
//! what you would write on axum today, or is it just different?
//!
//! Three contenders, doing the *same* work each round:
//!
//! * **kernway** — [`Htmx`] over kernway's one-buffer `Headers`, and
//!   [`HtmxResponse`] building the reply.
//! * **axum-htmx** — the dedicated crate (`axum-htmx` 0.8), the real incumbent.
//!   Its extractors are `async fn from_request_parts`; we drive them to
//!   completion with a no-op waker so we measure the extractor's own work, not
//!   an executor.
//! * **http (substrate)** — hand-written `http::HeaderMap` access, i.e. what you
//!   write on axum/actix/warp *without* a helper crate. This is also what
//!   axum-htmx's response side compiles down to internally, so it doubles as the
//!   fair floor for the response comparison.
//!
//! Run with: `cargo bench -p kernway-htmx`

use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll};

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use kernway_core::request::Request;
use kernway_core::response::IntoResponse;
use kernway_htmx::{Htmx, HtmxResponse, Swap};

use axum_core::extract::FromRequestParts;
use axum_htmx::{HxBoosted, HxRequest, HxTarget, HxTrigger};
use http::header::{CONTENT_TYPE, VARY};
use http::{HeaderValue, Response as HttpResponse};

/// A realistic htmx request: the six `HX-*` headers htmx sends, plus the two
/// ordinary headers every browser adds. Eight headers — the same profile the
/// parse benches in `kernway-http` use.
const HEADERS: &[(&str, &str)] = &[
    ("host", "example.com"),
    ("accept", "text/html"),
    ("hx-request", "true"),
    ("hx-boosted", "true"),
    ("hx-target", "user-list"),
    ("hx-trigger", "save-btn"),
    ("hx-trigger-name", "save"),
    ("hx-current-url", "https://example.com/users"),
];

/// Drive a `Ready` future to its value with a no-op waker. The axum-htmx
/// extractors never actually suspend (no I/O), so this is exactly their work —
/// no thread-parking executor tax added on top.
fn now<F: Future>(fut: F) -> F::Output {
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    let mut fut = pin!(fut);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("htmx extractor unexpectedly pended"),
    }
}

fn kernway_request() -> Request {
    let mut req = Request::new("GET", "/users");
    for (k, v) in HEADERS {
        req.headers.insert(k, v);
    }
    req
}

fn http_parts() -> http::request::Parts {
    let mut builder = http::Request::builder().method("GET").uri("/users");
    for (k, v) in HEADERS {
        builder = builder.header(*k, *v);
    }
    builder.body(()).unwrap().into_parts().0
}

// ------------------------------------------------------------------
// Request side: extract is_request + boosted + target + trigger
// ------------------------------------------------------------------

fn bench_extract(c: &mut Criterion) {
    let mut g = c.benchmark_group("htmx/extract");

    let req = kernway_request();
    g.bench_function("kernway", |b| {
        b.iter(|| {
            let hx = Htmx::from(&req);
            black_box(hx.is_request());
            black_box(hx.is_boosted());
            black_box(hx.target());
            black_box(hx.trigger());
        })
    });

    // Substrate: mirror axum-htmx's own semantics — presence for the bool flags,
    // `to_str()` for the string values.
    let parts = http_parts();
    g.bench_function("http_substrate", |b| {
        b.iter(|| {
            let h = &parts.headers;
            black_box(h.contains_key("hx-request"));
            black_box(h.contains_key("hx-boosted"));
            black_box(h.get("hx-target").and_then(|v| v.to_str().ok()));
            black_box(h.get("hx-trigger").and_then(|v| v.to_str().ok()));
        })
    });

    let mut parts = http_parts();
    g.bench_function("axum_htmx", |b| {
        b.iter(|| {
            black_box(now(HxRequest::from_request_parts(&mut parts, &())).unwrap().0);
            black_box(now(HxBoosted::from_request_parts(&mut parts, &())).unwrap().0);
            black_box(now(HxTarget::from_request_parts(&mut parts, &())).unwrap().0);
            black_box(now(HxTrigger::from_request_parts(&mut parts, &())).unwrap().0);
        })
    });

    g.finish();
}

// ------------------------------------------------------------------
// Response side: HTML body + content-type + 3 HX-* headers + Vary
// ------------------------------------------------------------------

fn bench_respond(c: &mut Criterion) {
    let mut g = c.benchmark_group("htmx/respond");

    g.bench_function("kernway", |b| {
        b.iter(|| {
            let resp = HtmxResponse::new("<div>ok</div>")
                .trigger("saved")
                .retarget("#status")
                .reswap(Swap::InnerHtml)
                .vary_on_request()
                .into_response();
            black_box(resp)
        })
    });

    // The axum/actix/warp response path: build an http::Response with the same
    // headers. axum-htmx's responders do exactly these HeaderMap inserts.
    g.bench_function("http_substrate", |b| {
        b.iter(|| {
            let resp = HttpResponse::builder()
                .header(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"))
                .header("hx-trigger", HeaderValue::from_static("saved"))
                .header("hx-retarget", HeaderValue::from_static("#status"))
                .header("hx-reswap", HeaderValue::from_static("innerHTML"))
                .header(VARY, HeaderValue::from_static("HX-Request"))
                .body(Vec::from(&b"<div>ok</div>"[..]))
                .unwrap();
            black_box(resp)
        })
    });

    g.finish();
}

// ------------------------------------------------------------------
// End to end: one htmx turn — extract the request, build the reply.
// The number that answers "what does htmx handling cost per request?"
// ------------------------------------------------------------------

fn bench_turn(c: &mut Criterion) {
    let mut g = c.benchmark_group("htmx/turn");

    let req = kernway_request();
    g.bench_function("kernway", |b| {
        b.iter(|| {
            let hx = Htmx::from(&req);
            let resp = hx
                .respond(
                    || "<tr><td>row</td></tr>".to_string(),
                    || "<html>page</html>".to_string(),
                )
                .trigger("saved")
                .into_response();
            black_box(resp)
        })
    });

    let mut parts = http_parts();
    g.bench_function("axum_htmx_plus_substrate", |b| {
        b.iter(|| {
            let is_htmx = now(HxRequest::from_request_parts(&mut parts, &())).unwrap().0;
            let body = if is_htmx { "<tr><td>row</td></tr>" } else { "<html>page</html>" };
            let resp = HttpResponse::builder()
                .header(CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"))
                .header("hx-trigger", HeaderValue::from_static("saved"))
                .header(VARY, HeaderValue::from_static("HX-Request"))
                .body(Vec::from(body.as_bytes()))
                .unwrap();
            black_box(resp)
        })
    });

    g.finish();
}

criterion_group!(benches, bench_extract, bench_respond, bench_turn);
criterion_main!(benches);
