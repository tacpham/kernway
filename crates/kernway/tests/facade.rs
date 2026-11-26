//! The one-dependency promise: an app written against `kernway` alone —
//! `use kernway::prelude::*`, the re-exported derives, no `kernway_*` /
//! `di_*` crate named anywhere — builds, starts, and serves over a real
//! socket: DI beans in handlers, path / query / JSON extraction, validation
//! and RFC 7807 errors, static files, routing by method, 404/405.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};

use kernway::prelude::*;
use serde::{Deserialize, Serialize};

/// A singleton bean: counts greetings across requests.
#[derive(Component)]
pub struct Counter {
    hits: AtomicU64,
}

impl Default for Counter {
    fn default() -> Self {
        Self { hits: AtomicU64::new(0) }
    }
}

/// A bean with an injected dependency.
#[derive(Component)]
pub struct Greeter {
    #[inject]
    counter: Arc<Counter>,
}

impl Greeter {
    fn greet(&self, name: &str) -> String {
        let n = self.counter.hits.fetch_add(1, Ordering::SeqCst) + 1;
        format!("hello {name} #{n}")
    }
}

#[derive(Deserialize)]
struct Search {
    q: String,
    // Query values arrive as strings (see `Query`'s docs).
    #[serde(default)]
    limit: Option<String>,
}

#[derive(Serialize, Deserialize, Validate)]
struct NewUser {
    #[validate(not_blank, length(min = 3, max = 20))]
    name: String,
    #[validate(email)]
    email: String,
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// One request over a fresh connection; returns (status, lower-cased head, body).
fn call(port: u16, method: &str, path: &str, body: Option<&str>) -> (u16, String, String) {
    let mut stream = None;
    for _ in 0..500 {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
            stream = Some(s);
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let mut s = stream.expect("server never came up");
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    let status = head.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    (status, head.to_ascii_lowercase(), body.to_string())
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body:?}"))
}

#[test]
fn a_kernway_only_app_serves_over_a_socket() {
    let root = std::env::temp_dir().join(format!("kw-facade-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("hello.txt"), "static hello").unwrap();

    let mut ctx = AppContext::new();
    ctx.build::<Counter>().unwrap();
    ctx.build::<Greeter>().unwrap();

    let port = free_port();
    let app = KernwayApp::builder()
        .bind(&format!("127.0.0.1:{port}"))
        .context(ctx)
        .static_files(&root)
        .get("/health", |_req: Request, _ctx: &RequestScope| async {
            Json(serde_json::json!({ "status": "UP" })).into_response()
        })
        .get("/greet/{name}", |req: Request, ctx: &RequestScope| {
            let greeter = ctx.get::<Greeter>().unwrap();
            async move {
                match Path::<String>::from_request(&req, "name") {
                    Ok(name) => Json(serde_json::json!({ "msg": greeter.greet(&name) })).into_response(),
                    Err(e) => ProblemDetail::bad_request(e),
                }
            }
        })
        .get("/square/{n}", |req: Request, _ctx: &RequestScope| async move {
            match Path::<i64>::from_request(&req, "n") {
                Ok(n) => Json(*n * *n).into_response(),
                Err(e) => ProblemDetail::bad_request(e),
            }
        })
        .get("/search", |req: Request, _ctx: &RequestScope| async move {
            match Query::<Search>::from_request(&req) {
                Ok(q) => Json(serde_json::json!({ "q": q.0.q, "limit": q.0.limit.and_then(|l| l.parse::<u32>().ok()).unwrap_or(10) })).into_response(),
                Err(e) => ProblemDetail::bad_request(e),
            }
        })
        .post("/users", |req: Request, _ctx: &RequestScope| async move {
            match Validated::<NewUser>::from_request(&req) {
                Ok(user) => {
                    let mut r = Json(user.0).into_response();
                    r.status = StatusCode::CREATED;
                    r
                }
                Err(problem) => problem,
            }
        })
        .delete("/users/{id}", |_req: Request, _ctx: &RequestScope| async {
            Response::new(StatusCode::NO_CONTENT)
        })
        .build();

    let stop = app.shutdown_handle();
    let server = std::thread::spawn(move || app.run_until_shutdown());

    // JSON handler.
    let (st, head, body) = call(port, "GET", "/health", None);
    assert_eq!(st, 200, "{head}");
    assert!(head.contains("content-type: application/json"), "{head}");
    assert_eq!(json(&body)["status"], "UP");

    // DI: the same singleton bean across requests.
    assert_eq!(json(&call(port, "GET", "/greet/ann", None).2)["msg"], "hello ann #1");
    assert_eq!(json(&call(port, "GET", "/greet/bob", None).2)["msg"], "hello bob #2");

    // Path extraction, typed: a non-number is a 400 problem.
    assert_eq!(json(&call(port, "GET", "/square/12", None).2), 144);
    let (st, head, body) = call(port, "GET", "/square/twelve", None);
    assert_eq!(st, 400);
    assert!(head.contains("application/problem+json"), "{head}");
    assert_eq!(json(&body)["status"], 400);

    // Query string, percent-decoded (UTF-8 and `+`), with a default.
    let v = json(&call(port, "GET", "/search?q=Nguy%E1%BB%85n+nail%20salon", None).2);
    assert_eq!((v["q"].as_str(), v["limit"].as_u64()), (Some("Nguyễn nail salon"), Some(10)));
    assert_eq!(json(&call(port, "GET", "/search?q=x&limit=3", None).2)["limit"], 3);
    assert_eq!(call(port, "GET", "/search?limit=3", None).0, 400, "q is required");

    // Validated JSON body: good → 201 echo; bad fields → 400 listing each; bad JSON → 400.
    let (st, _, body) = call(port, "POST", "/users", Some(r#"{"name":"Ann","email":"ann@a.io"}"#));
    assert_eq!(st, 201);
    assert_eq!(json(&body)["email"], "ann@a.io");
    let (st, head, body) = call(port, "POST", "/users", Some(r#"{"name":"A","email":"nope"}"#));
    assert_eq!(st, 400);
    assert!(head.contains("application/problem+json"));
    let errors = json(&body)["errors"].to_string();
    assert!(errors.contains("name") && errors.contains("email"), "{body}");
    assert_eq!(call(port, "POST", "/users", Some("{not json")).0, 400);

    // Routing by method, and the misses.
    assert_eq!(call(port, "DELETE", "/users/7", None).0, 204);
    let (st, ..) = call(port, "GET", "/users", None);
    assert!(st == 404 || st == 405, "GET on a POST-only route: {st}");
    assert_eq!(call(port, "GET", "/no/such/route", None).0, 404);

    // Static files from the configured root.
    let (st, _, body) = call(port, "GET", "/hello.txt", None);
    assert_eq!((st, body.as_str()), (200, "static hello"));

    stop.trigger();
    server.join().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&root);
}
