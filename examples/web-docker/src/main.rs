//! web-docker — the M1 walking skeleton.
//!
//! The smallest thing that runs end to end in a container: static files from
//! `public/`, a JSON route, and the two health checks an orchestrator needs.
//! Deliberately almost nothing — its job is to prove the front door works in
//! Docker and to surface what breaks there, not to show off features.
//!
//! Run locally:  cargo run -p web-docker
//!   http://localhost:8080/            → public/index.html
//!   http://localhost:8080/style.css   → public/style.css
//!   http://localhost:8080/api/ping    → {"message":"pong"}
//!   http://localhost:8080/htmx/greet  → an htmx fragment, or a full page
//!   http://localhost:8080/health      → 200 (liveness)
//!   http://localhost:8080/ready       → 200 (readiness)

use kernway::prelude::*;

fn main() -> std::io::Result<()> {
    // Cloud Run, Heroku, and most PaaS assign the port at runtime. Honour $PORT,
    // fall back to 8080. Always bind 0.0.0.0 — 127.0.0.1 inside a container is
    // reachable by nobody.
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    let addr = format!("0.0.0.0:{port}");

    KernwayApp::builder()
        .bind(&addr)
        // Serve the bundled public/ directory. In this M1 slice the files are
        // read from disk at request time; embedding them into the binary for
        // release is M6.
        .static_files("public")
        // Serve precompressed `.br`/`.gz` next to a compressible file when the
        // client accepts it — see `public/style.css.br`. No CPU spent
        // compressing per request; the variants are produced at build time.
        .precompressed()
        // A JSON API route, to prove routing and static coexist: the router is
        // tried first, so /api/ping is dynamic and everything else falls through
        // to the filesystem.
        .get("/api/ping", |_req: Request, _ctx: &RequestScope| async {
            Response::new(StatusCode::OK)
                .content_type("application/json; charset=utf-8")
                .body(br#"{"message":"pong"}"#.to_vec())
        })
        // One htmx endpoint, to prove the typed HX-* API works end to end. htmx
        // sends `HX-Request: true`; the handler returns just the fragment for it
        // and a full page for a plain browser hit — the same URL, two shapes,
        // and `respond` sets `Vary: HX-Request` so a cache never mixes them up.
        // It also fires a client-side `greeted` event via `HX-Trigger`.
        .get(
            "/htmx/greet",
            |req: Request, _ctx: &RequestScope| async move {
                Htmx::from(&req)
                    .respond(
                        || "<div id=\"greeting\">Hello from an htmx fragment 👋</div>".to_string(),
                        || {
                            "<!doctype html><title>Greet</title>\
                        <button hx-get=\"/htmx/greet\" hx-target=\"#greeting\">Greet</button>\
                        <div id=\"greeting\"></div>"
                                .to_string()
                        },
                    )
                    .trigger("greeted")
                    .into_response()
            },
        )
        // Liveness: "the process is up". Kubernetes restarts the pod if this
        // fails. It must stay trivially true and touch nothing external.
        .get("/health", |_req: Request, _ctx: &RequestScope| async {
            Response::new(StatusCode::OK)
                .content_type("application/json; charset=utf-8")
                .body(br#"{"status":"ok"}"#.to_vec())
        })
        // Readiness: "send me traffic". Distinct from liveness on purpose — a
        // dependency being slow should stop traffic, not restart the pod. In M1
        // there are no dependencies, so it is a constant; it grows a real check
        // when there is something to check.
        .get("/ready", |_req: Request, _ctx: &RequestScope| async {
            Response::new(StatusCode::OK)
                .content_type("application/json; charset=utf-8")
                .body(br#"{"ready":true}"#.to_vec())
        })
        .build()
        .run()
}
