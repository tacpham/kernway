//! The full per-request CPU path, across every module it touches.
//!
//! Routing, parsing, and encoding are each measured alone elsewhere. This is
//! the one that runs them together the way a real request does: bytes in →
//! `kernway-http` parse → `kernway-server` route → the handler builds a
//! `kernway-core` `Response` → `kernway-http` encode → bytes out. No socket and
//! no file I/O — just the CPU cost that every request pays no matter how fast
//! the network is, so a regression in any one module shows up here.
//!
//! What this is *not*: an end-to-end throughput number. That needs a load test
//! against a running server (a milestone, not a micro-benchmark), and until it
//! exists this measures the floor, not the ceiling.

use std::sync::Arc;

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use di_core::AppContext;
use kernway_core::{error::StatusCode, request::Request, response::Response};
use kernway_http::{encode_response_with, parse_bytes, writer::Connection, Parsed};
use kernway_server::router::{Handler, Router};

const STATIC_GET: &[u8] = b"GET /health HTTP/1.1\r\nHost: localhost:8080\r\n\r\n";

const PARAM_GET: &[u8] =
    b"GET /users/42/posts/99 HTTP/1.1\r\nHost: localhost:8080\r\nAccept: application/json\r\n\r\n";

fn app() -> (Router, AppContext) {
    let mut router = Router::new();
    let ok: Handler = Arc::new(|_req, _ctx| {
        Response::new(StatusCode::OK)
            .content_type("application/json; charset=utf-8")
            .body(br#"{"status":"ok"}"#.to_vec())
    });
    let echo: Handler = Arc::new(|req: &Request, _ctx| {
        // Touch a path param, the way a real handler does.
        let id = req.path_params.get("id").cloned().unwrap_or_default();
        Response::new(StatusCode::OK)
            .content_type("application/json; charset=utf-8")
            .body(format!(r#"{{"user":"{id}"}}"#).into_bytes())
    });
    router.add("GET", "/health", ok);
    router.add("GET", "/users/{id}/posts/{post}", echo);
    (router, AppContext::new())
}

/// One request, start to finish, in process: parse → route → handle → encode.
fn run_once(router: &Router, ctx: &AppContext, raw: &[u8]) -> Vec<u8> {
    let mut request = match parse_bytes(raw).expect("valid request") {
        Parsed::Complete { request, .. } => request,
        Parsed::Incomplete => unreachable!("the fixture is a whole request"),
    };
    let (handler, params) = router
        .find(&request.method, &request.path)
        .expect("a route matches the fixture");
    request.path_params = params;
    let response = handler(&request, ctx);
    encode_response_with(&response, Connection::KeepAlive)
}

fn pipeline(c: &mut Criterion) {
    let (router, ctx) = app();
    let mut group = c.benchmark_group("pipeline");

    // A static route: the common case, and the one with no per-request
    // allocation in the router.
    group.bench_function("static_get", |b| {
        b.iter(|| black_box(run_once(black_box(&router), black_box(&ctx), black_box(STATIC_GET))));
    });

    // A parameterised route: pays the param map plus a JSON body build.
    group.bench_function("param_get", |b| {
        b.iter(|| black_box(run_once(black_box(&router), black_box(&ctx), black_box(PARAM_GET))));
    });

    group.finish();
}

criterion_group!(benches, pipeline);
criterion_main!(benches);
