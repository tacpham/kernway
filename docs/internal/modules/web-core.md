# web-core — Extractors & Response Types

## Purpose

`FromRequest` implementations (extractors) and `IntoResponse` implementations.  
Entry point macro: `#[kernway::main]`.

## Extractors (FromRequest)

```rust
// Path parameters — RFC 3986 §3.3
#[route(GET, "/users/{id}")]
async fn get_user(Path(id): Path<u64>) -> impl IntoResponse { ... }

// Query string — RFC 3986 §3.4
#[route(GET, "/search")]
async fn search(Query(params): Query<SearchParams>) -> impl IntoResponse { ... }
// URL: /search?q=rust&page=2&limit=10

// JSON body — RFC 7159
#[route(POST, "/users")]
async fn create_user(Json(body): Json<CreateUserRequest>) -> impl IntoResponse { ... }

// Headers
#[route(GET, "/protected")]
async fn protected(Header(auth): Header<Authorization>) -> impl IntoResponse { ... }

// Multiple extractors:
#[route(GET, "/users/{id}/posts")]
async fn user_posts(
    Path(user_id): Path<u64>,
    Query(pagination): Query<Pagination>,
    Header(auth): Header<Authorization>,
) -> impl IntoResponse { ... }
```

## Response Types (IntoResponse)

```rust
// JSON — Content-Type: application/json
Json(UserResponse { id: 1, name: "Alice" })

// Plain text — Content-Type: text/plain
Text("Hello, World!")

// HTML — Content-Type: text/html
Html("<h1>Hello</h1>")

// Status code only
StatusCode::NO_CONTENT  // 204

// Tuple: (status, body)
(StatusCode::CREATED, Json(user))

// Tuple: (status, headers, body)
(StatusCode::OK, [("X-Custom", "value")], Json(user))

// Redirect — RFC 9110 §15.4
Redirect::to("/new-location")              // 302
Redirect::permanent("/new-location")       // 301

// Error — auto-converts to appropriate status
#[derive(Debug, IntoResponse)]
#[response(status = 404)]
struct NotFoundError { message: String }
```

## `#[kernway::main]`

```rust
// Input:
#[kernway::main]
async fn main() {
    KernwayApp::builder()
        .bind("0.0.0.0:8080")
        .build()
        .run()
        .await
}

// Generated:
fn main() {
    // Set up thread-per-core executor
    let num_cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let handles: Vec<_> = (0..num_cores).map(|core_id| {
        std::thread::spawn(move || {
            pin_current_thread_to_core(core_id).ok();
            let mut executor = Executor::new();
            executor.run(async { /* main body */ });
        })
    }).collect();
    for h in handles { h.join().unwrap(); }
}
```

## KernwayApp Builder

```rust
KernwayApp::builder()
    .bind("0.0.0.0:8080")
    .bind_tls("0.0.0.0:8443", tls_config)
    .db(PostgresPool::new(env!("DATABASE_URL")))
    .plugin(KernleafPlugin::default())
    .plugin(TracingPlugin::json_logs())
    .plugin(MetricsPlugin::prometheus())
    .layer(CorsLayer::new().allow_origins(["https://example.com"]))
    .layer(RateLimitLayer::new().per_ip(100, Duration::from_secs(60)))
    .static_files("/assets", "public/assets/")
    .shutdown_timeout(Duration::from_secs(30))
    .build()
```
