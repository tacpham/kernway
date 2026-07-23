# Kernway — Rust Web Framework (Spring-inspired)

> Build Rust web apps in the style of Spring Boot. Compile. Run.

---

## This is the goal — a complete app built with Kernway

```rust
use kernway::prelude::*;

// --- Domain ---
#[derive(Serialize, Deserialize)]
struct User { id: u64, name: String }

// --- Service layer ---
#[component]
struct UserService {
    store: std::sync::Mutex<Vec<User>>,
}

impl UserService {
    fn new() -> Self {
        Self { store: std::sync::Mutex::new(vec![]) }
    }

    async fn find(&self, id: u64) -> Option<User> {
        self.store.lock().unwrap().iter().find(|u| u.id == id).cloned()
    }

    #[transactional]
    async fn create(&self, user: User) -> User {
        self.store.lock().unwrap().push(user.clone());
        user
    }
}

// --- Controller layer ---
#[controller("/users")]
struct UserController {
    #[inject]
    service: Arc<UserService>,
}

#[route(GET, "/{id}")]
async fn get_user(ctrl: &UserController, id: Path<u64>) -> Json<User> {
    ctrl.service.find(*id).await.unwrap().into()
}

#[route(POST, "/")]
#[require_role("ADMIN")]
async fn create_user(ctrl: &UserController, body: Json<User>) -> Json<User> {
    ctrl.service.create(body.into_inner()).await.into()
}

// --- Entry point ---
#[kernway::main]
async fn main() {
    KernwayApp::builder()
        .bind("0.0.0.0:8080")
        .workers(num_cpus::get())   // thread-per-core
        .run()
        .await;
}
```

**This is the destination.** Every technical decision below serves this developer experience.

---

## Design philosophy — Spec first, implementation second

> **Kernway principle #1**: `kernway-core` may only contain **spec definitions** (traits, types, contracts). Not a single line of implementation is allowed in core. Everything else is implementation — including Kernway's own implementation.

This is the model Java has gotten right for the last 20 years:

```
Java Spec (JSR)          Kernway Spec              Implementation
─────────────────        ─────────────────         ──────────────────────
javax.sql.DataSource  →  trait DbPool           →  PostgresPool, MySqlPool
javax.servlet.Filter  →  trait Layer            →  CorsLayer, AuthLayer
ViewResolver (Spring) →  trait TemplateEngine   →  KernleafEngine, TeraAdapter
HttpMessageConverter  →  trait IntoResponse     →  Json<T>, Html<T>, Csv<T>
HandlerMethodArg...   →  trait FromRequest      →  Path<T>, Query<T>, Json<T>
ApplicationContext    →  trait KernwayPlugin     →  KernleafPlugin, DbPlugin
```

### `kernway-core` — spec only, no implementation

```rust
// kernway-core CONTAINS this — all of it, and nothing more:

pub trait IntoResponse: Send {
    fn into_response(self) -> Response;
}

pub trait FromRequest: Sized {
    fn from_request(req: &Request) -> Result<Self, Error>;
}

pub trait TemplateEngine: Send + Sync {
    fn render(&self, template: &str, ctx: &dyn TemplateContext) -> Result<String>;
}

pub trait DbPool: Send + Sync {
    fn acquire(&self) -> BoxFuture<Result<Box<dyn Connection>>>;
}

pub trait Layer: Send + Sync {
    fn handle<'a>(&'a self, req: Request, next: &'a dyn Next) -> BoxFuture<'a, Response>;
}

pub trait KernwayPlugin: Send + Sync {
    fn register(&self, app: &mut AppBuilder);
}

// kernway-core does NOT contain serde, serde_json, diesel, rustls,
//                           or any implementation at all.
// kernway-core compile time: < 1s
```

### Implementation layer — Kernway's own + Community's

```
kernway-core  (spec — stable forever)
│
├── [Kernway reference implementations]
│   ├── kernway-web    → Json<T>, Html<T> impl IntoResponse
│   │                    Path<T>, Query<T> impl FromRequest
│   ├── kernway-db     → PostgresPool, MySqlPool impl DbPool
│   ├── kernleaf       → KernleafEngine impl TemplateEngine
│   └── kernway-aop    → TransactionLayer impl Layer
│
└── [Community implementations — anyone can build one]
    ├── kernway-mongodb  → MongoPool impl DbPool
    ├── kernway-redis    → RedisPool impl DbPool
    ├── kernway-tera     → TeraAdapter impl TemplateEngine
    ├── kernway-xml      → Xml<T> impl IntoResponse
    └── kernway-graphql  → GraphQL handler impl FromRequest + IntoResponse
```

### Practical consequences

```rust
// Say a user wants MongoDB instead of PostgreSQL:
// Change one line in main.rs — the Repository code is untouched

// BEFORE:
.db(PostgresPool::new(env!("DATABASE_URL")))

// SAU:
.db(MongoPool::new(env!("MONGO_URL")))

// Repository code — COMPLETELY UNCHANGED:
#[component]
struct UserRepository {
    #[inject] pool: Arc<dyn DbPool>,  // ← a trait object, not a concrete type
}
```

```rust
// Say the community wants to build kernway-xml:
// Implement one trait — no fork of kernway, no PR into the core

pub struct Xml<T>(pub T);

impl<T: Serialize> IntoResponse for Xml<T> {
    fn into_response(self) -> Response {
        Response::builder()
            .header("Content-Type", "application/xml")
            .body(to_xml(&self.0))
    }
}

// Then use it straight away in a handler:
#[route(GET, "/{id}")]
async fn get_user(...) -> Xml<User> { ... }
```

---

## Why Kernway?

| What you use today | Problem | What Kernway solves |
|---|---|---|
| **Spring Boot** and want to try Rust | No Rust framework provides familiar DI + AOP | `#[component]` `#[inject]` `#[transactional]` |
| **Axum/Actix** | You have to wire DI manually and there is no `@Transactional` | Compile-time DI, zero runtime reflection |
| **Any tokio-based framework** | p99 latency is affected by the work-stealing scheduler | Thread-per-core: each core runs independently with optimal cache locality |
| **Serverless / Edge** | Large binaries (~10MB), slow cold start because of tokio overhead | Small binaries (~3MB), fast cold start |

---

## Comparison with existing frameworks

| Feature | **Kernway** | Axum | Actix-web | Spring Boot | Rocket |
|---|---|---|---|---|---|
| Built-in DI | ✅ Compile-time | ❌ | ❌ | ✅ Runtime | ❌ |
| AOP (`#[transactional]`) | ✅ | ❌ | ❌ | ✅ | ❌ |
| Scheduler | ✅ Thread-per-core | ⚠️ Work-stealing | ⚠️ Work-stealing | ⚠️ Work-stealing | ⚠️ Work-stealing |
| p99 latency | ✅ Best | ⚠️ | ⚠️ | ❌ JVM overhead | ⚠️ |
| Binary size | ✅ ~3MB | ⚠️ ~8MB | ⚠️ ~10MB | ❌ JVM 200MB+ | ⚠️ ~8MB |
| Cold start | ✅ ~20ms | ✅ ~50ms | ✅ ~50ms | ❌ 3-10s | ✅ ~50ms |
| Template engine | ✅ kernleaf (v0.6) | ❌ | ❌ | ✅ Thymeleaf | ✅ Tera |
| Hot reload (dev) | ✅ `.so` plugin | ❌ | ❌ | ✅ DevTools | ❌ |
| Validation | ✅ `#[validated]` (v0.4) | ⚠️ External | ⚠️ External | ✅ `@Valid` | ⚠️ External |
| Observability | ✅ tracing + metrics (v0.4) | ⚠️ Manual | ⚠️ Manual | ✅ Actuator | ❌ |
| OpenAPI | ✅ (v0.6) | ⚠️ utoipa | ⚠️ utoipa | ✅ springdoc | ⚠️ |
| Spring DX | ✅ Native | ❌ | ❌ | ✅ | ❌ |

> **Benchmark target** (TechEmpower Plain Text, Linux, 16-core): Kernway's goal is ≥ Actix-web p50 and better than Actix-web p99 thanks to thread-per-core. Benchmarks will be published in `benches/` and CI once v0.3 is stable.

---

## Complete feature set of a production web framework

### ✅ Already in the current roadmap (v0.1–v0.5)
- Compile-time DI + AOP macros
- HTTP/1.1, TLS, HTTP/2
- Thread-per-core reactor
- Database (spawn_blocking + diesel)
- Template engine (kernleaf)
- Hot reload dev mode

### 📋 Features that should be added to the roadmap

**Validation (v0.4) — Equivalent to Spring `@Valid`:**
```rust
#[derive(Deserialize, Validate)]
struct CreateUserRequest {
    #[validate(length(min = 2, max = 50))]
    name: String,
    #[validate(email)]
    email: String,
    #[validate(range(min = 18, max = 120))]
    age: u32,
}

#[route(POST, "/users")]
async fn create_user(body: Json<Validated<CreateUserRequest>>) -> Result<Json<User>, ValidationError> {
    // body is already validated; on failure it returns RFC 7807 Problem Details automatically
}
```

**Observability (v0.4) — Production monitoring:**
```rust
// Integrates the tracing crate (Rust's de facto standard)
// Instruments every request automatically
KernwayApp::builder()
    .tracing(TracingConfig::json_stdout())   // structured logging
    .metrics(MetricsConfig::prometheus("/metrics"))  // Prometheus endpoint
    .request_id(RequestIdConfig::uuid_v4())  // X-Request-ID header
```

**Configuration (v0.3) — dev/staging/prod profiles:**
```rust
// config/app_config.rs
#[configuration]
#[config_source(env, file = "config/{profile}.toml")]
struct AppConfig {
    #[config(env = "DATABASE_URL")]
    database_url: String,
    #[config(default = "8080")]
    port: u16,
    #[config(env = "KERNWAY_PROFILE", default = "dev")]
    profile: String,
}
// KERNWAY_PROFILE=prod → load config/prod.toml
```

**Testing (v0.3) — unit + integration tests:**
```rust
#[kernway::test]
async fn test_get_user() {
    let app = TestApp::new()
        .mock_bean::<UserRepository>(MockUserRepo::new())  // mock DI bean
        .build();

    let resp = app.get("/users/1").await;
    assert_eq!(resp.status(), 200);
    assert_json!(resp.body(), { "id": 1, "name": "Alice" });
}
```

**WebSocket (v0.6) — RFC 6455:**
```rust
#[route(GET, "/ws/chat")]
async fn chat_ws(ws: WebSocket) -> impl WsHandler {
    ws.on_message(|msg, ctx| async move {
        ctx.broadcast(msg).await;
    })
}
```

**Security Middleware (v0.4):**
```rust
KernwayApp::builder()
    .layer(CorsLayer::new()
        .allow_origins(["https://myapp.com"])
        .allow_methods([Method::GET, Method::POST]))
    .layer(CsrfLayer::new())
    .layer(SecurityHeadersLayer::default())  // HSTS, CSP, X-Frame-Options
    .layer(RateLimitLayer::per_ip(100, Duration::from_secs(60)))
    .layer(RequestTimeoutLayer::new(Duration::from_secs(30)))
    .layer(RequestSizeLimitLayer::new(10 * 1024 * 1024))  // 10MB
```

**Health Check + Graceful Shutdown (v0.3):**
```rust
KernwayApp::builder()
    .health_check("/health", || async { HealthStatus::Up })
    .ready_check("/ready", |ctx| async move {
        ctx.get::<DbPool>().ping().await.is_ok()
    })
    .shutdown_timeout(Duration::from_secs(30))  // drain in-flight requests
```

**Static Files (v0.3):**
```rust
KernwayApp::builder()
    .static_files("/assets", "public/")  // serve public/ at /assets/*
    .spa_fallback("public/index.html")   // SPA mode: unknown routes → index.html
```

**OpenAPI / Swagger (v0.6):**
```rust
#[route(GET, "/{id}")]
#[openapi(summary = "Get user by ID", tag = "users")]
async fn get_user(
    id: Path<u64>,
    /// User ID to fetch
) -> Json<UserResponse> { ... }
// → /openapi.json generated automatically
// → /swagger-ui available automatically
```

---

## Full roadmap — updated

| Version | User-facing features | Production-ready? |
|---|---|---|
| **v0.1** | DI (`#[component]` `#[inject]`) | Partial |
| **v0.2** | TCP runtime, cross-platform | No |
| **v0.3** | REST API, Config, Static files, Health check, Testing | Dev-ready |
| **v0.4** | AOP, Validation, Observability, Security middleware, CORS | Production-ready |
| **v0.5** | TLS, HTTP/2, Hot reload `.so` plugin | Production + DX |
| **v0.6** | WebSocket, OpenAPI, kernleaf templates, Multipart upload | Full-featured |
| **v1.0** | Stable API, published benchmarks, migration guide | ✅ |



```rust
// Template engine: swap kernleaf for Tera — one line
.plugin(TeraAdapter::new("templates/**/*"))

// Database: swap Postgres for MySQL — one line
.db(MySqlPool::new(env!("DATABASE_URL")))

// A new response type (CSV) — implement one trait, no need to ask Kernway
impl<T: ToCsv> IntoResponse for CsvResponse<T> { /* ... */ }

// New middleware (custom auth) — implement one trait
impl Layer for MyAuthLayer { /* ... */ }
```

**Feature flags — compile only what you use:**
```toml
kernway = { version = "0.3", features = ["json"] }               # API only, ~3MB
kernway = { version = "0.5", features = ["json", "templates"] }  # +SSR, ~4MB
kernway = { version = "0.5", features = ["full"] }               # everything, ~6MB
```

---

## Structure of a Kernway app — layered architecture

Like Spring Boot, a Kernway app has a clear layered structure:

```
├── Cargo.toml
└── src/
    ├── main.rs                    # Entry point — #[kernway::main]
    ├── lib.rs                     # Module declarations, App bootstrap
    │
    ├── config/
    │   ├── mod.rs
    │   ├── app_config.rs          # #[configuration] — app config (port, db url, ...)
    │   └── security_config.rs     # #[configuration] — JWT secret, CORS, roles
    │
    ├── controller/                # HTTP layer — takes requests, returns responses
    │   ├── mod.rs
    │   ├── user_controller.rs     # #[controller("/users")] #[route]
    │   └── auth_controller.rs     # #[controller("/auth")]
    │
    ├── service/                   # Business logic layer
    │   ├── mod.rs
    │   ├── user_service.rs        # #[component] — business logic
    │   └── email_service.rs       # #[component]
    │
    ├── repository/                # Data access layer
    │   ├── mod.rs
    │   ├── user_repository.rs     # #[component] — DB queries (uses spawn_blocking + diesel)
    │   └── traits.rs              # trait UserRepo — so tests can mock it
    │
    ├── model/                     # Domain entities + DTOs
    │   ├── mod.rs
    │   ├── user.rs                # struct User (domain entity)
    │   └── dto/
    │       ├── mod.rs
    │       ├── create_user_req.rs # #[derive(Deserialize)] — request body
    │       └── user_response.rs   # #[derive(Serialize)] — response body
    │
    └── exception/
        ├── mod.rs
        ├── app_error.rs           # enum AppError (business errors)
        └── handler.rs             # #[exception_handler] — global error mapping
```

**Spring → Kernway mapping:**

| Spring | Kernway | Notes |
|---|---|---|
| `@SpringBootApplication` | `#[kernway::main]` | Entry point |
| `@RestController` | `#[controller("/path")]` | HTTP handler |
| `@GetMapping` | `#[route(GET, "/path")]` | Route definition |
| `@Service` | `#[component]` | Business-logic bean |
| `@Repository` | `#[component]` + `spawn_blocking` | Data access |
| `@Configuration` | `#[configuration]` | Config beans |
| `@Autowired` | `#[inject]` | Dependency injection |
| `@Transactional` | `#[transactional]` | Transaction boundary |
| `@PreAuthorize` | `#[require_role("ADMIN")]` | Authorization |
| `@ControllerAdvice` | `#[exception_handler]` | Global error handler |
| `application.properties` | `config/app_config.rs` + env vars | App configuration |

**Complete example of a controller layer:**

```rust
// src/controller/user_controller.rs

#[controller("/users")]
pub struct UserController {
    #[inject] service: Arc<UserService>,
}

#[route(GET, "/")]
async fn list_users(
    ctrl: &UserController,
    query: Query<PaginationQuery>,
) -> Json<PageResponse<UserResponse>> {
    ctrl.service.list(query.into_inner()).await.into()
}

#[route(GET, "/{id}")]
async fn get_user(
    ctrl: &UserController,
    id: Path<u64>,
) -> Result<Json<UserResponse>, AppError> {
    ctrl.service.find(*id).await
        .map(Json)
        .ok_or(AppError::NotFound("User"))
}

#[route(POST, "/")]
#[require_role("ADMIN")]
async fn create_user(
    ctrl: &UserController,
    body: Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<UserResponse>), AppError> {
    let user = ctrl.service.create(body.into_inner()).await?;
    Ok((StatusCode::CREATED, Json(user)))
}

#[route(DELETE, "/{id}")]
#[require_role("ADMIN")]
async fn delete_user(
    ctrl: &UserController,
    id: Path<u64>,
) -> Result<StatusCode, AppError> {
    ctrl.service.delete(*id).await?;
    Ok(StatusCode::NO_CONTENT)
}
```

**main.rs:**

```rust
// src/main.rs
use kernway::prelude::*;
mod config;
mod controller;
mod service;
mod repository;
mod model;
mod exception;

#[kernway::main]
async fn main() {
    KernwayApp::builder()
        .bind("0.0.0.0:8080")
        .workers(num_cpus::get())
        .run()
        .await;
}
```

---

## Build time — concrete strategy

**Targets:**
```
cargo build (clean, dev profile):  15-20s  ← competitive with Axum
cargo check:                        3-5s   ← the everyday edit loop
incremental build (one file changed): 1-3s
```

**Required techniques:**

| Technique | Estimated savings | Where to apply |
|---|---|---|
| `syn` minimal features | ~3-5s | All proc-macro crates |
| `dyn Trait` instead of generics in the framework layer | ~2-3s | Router, DI wiring (already in the plan) |
| Split macros into small crates | ~2-3s | Separate `di-macro`, `route-macro`, `aop-macro` |
| `cargo-hakari` workspace-hack | ~2-4s | workspace root |
| Dev profile (already in the plan) | ~5-8s | `.cargo/config.toml` |
| `mold`/`lld` linker (already in the plan) | ~2-3s | linking stage |

**Required rules when writing proc macros:**
```toml
# di-macro/Cargo.toml — enable ONLY the features actually needed
syn = { version = "2", default-features = false, features = ["derive", "parsing", "printing"] }
# Do NOT use features = ["full"] unless you genuinely need to parse function bodies
```

**`cargo-hakari` setup** (add it to the workspace once there are at least 3 crates):
```toml
# workspace-hack/Cargo.toml — generated automatically by cargo hakari
# Avoids compiling the same dependency repeatedly under different feature sets
```

---

## Database — `spawn_blocking` bridge strategy

**Problem**: SQLx, sea-orm, and diesel-async assume a tokio runtime internally → incompatible.

**Solution**: `kernway-rt` provides `spawn_blocking` — run synchronous DB calls on a dedicated **blocking thread pool** and bridge results back to the async executor via a channel.

```
kernway executor (async, thread-per-core)
  │
  │ spawn_blocking(|| diesel_query())  ← non-blocking from the executor's view
  ↓
blocking thread pool (4 × cpu_count threads)
  │  runs sync diesel + an r2d2 connection pool
  │
  channel → waker → executor receives the result
```

**Planned API (`kernway-db` crate, v0.3+):**
```rust
use kernway::db::{DbPool, spawn_blocking};

// Setup (trong #[configuration])
#[configuration]
struct DatabaseConfig;
impl DatabaseConfig {
    #[bean]
    fn db_pool() -> DbPool {
        DbPool::postgres(std::env::var("DATABASE_URL").unwrap())
            .max_connections(20)
            .build()
    }
}

// Used inside a repository
#[component]
pub struct UserRepository {
    #[inject] pool: Arc<DbPool>,
}

impl UserRepository {
    pub async fn find_by_id(&self, id: u64) -> Result<Option<User>, AppError> {
        let pool = self.pool.clone();
        spawn_blocking(move || {
            // diesel ORM — sync, battle-tested, production-proven
            use crate::schema::users::dsl::*;
            let conn = &mut pool.get()?;
            users.find(id as i64).first::<UserRow>(conn).optional()
        })
        .await
        .map_err(AppError::Database)?
        .map(UserRow::into_domain)
    }
}
```

**Why diesel + `spawn_blocking` instead of a native async DB?**
- Diesel: 5+ years in production, with a complete migration toolchain (`diesel_cli`)
- The `spawn_blocking` pattern matches reality — DB queries are CPU/I/O blocking, not async I/O
- Java Project Loom and Go goroutines use the same mechanism
- It avoids writing the Postgres wire protocol from scratch (1+ years of work, many edge cases)

**DB roadmap:**
- **v0.3**: `spawn_blocking` in `kernway-rt`, documented diesel + r2d2 usage pattern
- **v0.4**: `kernway-db` crate — a more convenient wrapper with integrated connection pooling
- **v1.0+**: Research a native async DB driver if there is real demand



> **Architecture**: **thread-per-core (shard-per-core)** — DO NOT use tokio/hyper/async-std.
> **Cross-platform**: Linux / macOS / Windows.

---

## Release milestones — from the user's point of view

### v0.1 — `kernway-di`: DI on any runtime
> **What users can do**: Add `kernway-di` to an Axum, Rocket, or any existing framework project and use `#[component]` `#[inject]` immediately.

```rust
// Cargo.toml: kernway-di = "0.1"
#[component]
struct EmailService { api_key: String }

#[component]
struct UserService {
    #[inject] email: Arc<EmailService>,
}

fn main() {
    let ctx = AppContext::build(); // compile-time error when a bean is missing
    let svc = ctx.get::<UserService>();
}
```
**Deliverable**: Publish the `kernway-di` crate to crates.io. Axum users can adopt it immediately.

---

### v0.2 — `kernway-rt`: working runtime + TCP server
> **What users can do**: Run a TCP echo server and a raw HTTP server on the custom executor, without tokio.

```rust
// Cargo.toml: kernway-rt = "0.2"
use kernway_rt::net::TcpListener;

kernway_rt::block_on(async {
    let listener = TcpListener::bind("0.0.0.0:8080").await?;
    // accept connections...
});
```
**Deliverable**: Echo server benchmarks running on Linux/macOS/Windows, with p99 within about 20% of tokio.

---

### v0.3 — `kernway-web` MVP: the first working app ✨
> **What users can do**: Build REST APIs with DI + routing. **This is the most important milestone.**

```rust
// Cargo.toml: kernway = "0.3"
#[component]
struct UserService { /* ... */ }

#[controller("/users")]
struct UserController {
    #[inject] service: Arc<UserService>,
}

#[route(GET, "/{id}")]
async fn get_user(ctrl: &UserController, id: Path<u64>) -> Json<User> {
    ctrl.service.find(*id).await.into()
}

#[route(POST, "/")]
async fn create_user(ctrl: &UserController, body: Json<CreateUserReq>) -> Json<User> {
    ctrl.service.create(body.into_inner()).await.into()
}

#[kernway::main]
async fn main() {
    KernwayApp::builder().bind("0.0.0.0:8080").run().await;
}
```

**Deliverable**: `examples/todo-app` with full CRUD backed by an in-memory store. The README includes a Getting Started guide.

**Request extractors required in v0.3**:
- `Path<T>` — path parameter `/users/{id}`
- `Query<T>` — query string `?page=1&size=10`
- `Json<T>` — request-body deserialization
- `Header<T>` — single header value

**Response types required in v0.3**:
- `Json<T>` — `Content-Type: application/json`
- `Text(String)` — plain text
- `Status(u16)` — empty response with a status code
- `(Status, Json<T>)` — tuple response

---

### v0.4 — AOP + middleware: production-ready logic
> **What users can do**: Add declarative cross-cutting concerns without modifying business logic.

```rust
#[transactional]
async fn transfer(&self, from: u64, to: u64, amount: f64) -> Result<()> { /* ... */ }

#[require_role("ADMIN")]
#[rate_limit(requests = 100, per = "1m")]
async fn delete_user(&self, id: u64) -> Result<()> { /* ... */ }

#[exception_handler(AppError)]
async fn handle_error(err: AppError) -> (StatusCode, Json<ErrorResponse>) { /* ... */ }
```

**Deliverable**: `examples/todo-app` upgraded with auth + error handling.

---

### v0.5 — TLS + HTTP/2: production deployment
> **What users can do**: Serve HTTPS and deploy directly without a reverse proxy.

```rust
KernwayApp::builder()
    .bind("0.0.0.0:443")
    .tls(TlsConfig::from_pem("cert.pem", "key.pem"))
    .workers(num_cpus::get())
    .run()
    .await;
```

---

### v0.5 — Hot reload + plugin mode: complete developer experience
> **What users can do**: Develop with an edit-save-refresh workflow. No server restarts required.

**Two deployment modes:**

```
Mode 1 — Compiled-in (default, v0.3+):
  cargo build --release → a single binary
  Use this for production

Mode 2 — Plugin (v0.5+):
  kernway-server (pre-compiled, downloaded once)
  + your-app.so  (only this rebuilds on each change, ~2-5s)
  Use this for development with hot reload
```

**Hot reload workflow:**

```bash
# Install the CLI once
cargo install kernway-cli

# In your project — switch crate-type to cdylib
# Cargo.toml: crate-type = ["cdylib"]

# Run the dev server with hot reload
kernway dev
```

```
🚀 Kernway dev server  →  http://localhost:8080
👀 Watching src/ for changes...

[11:32:10] Changed: src/controller/user_controller.rs
[11:32:10] Building...
[11:32:13] ✅ Built in 2.8s — reloading
[11:32:13] 🔄 App reloaded (0 in-flight requests drained)
           ^ the server never restarts, and connections are never dropped
```

**Graceful hot reload mechanism:**
```
In-flight Request A (before reload):
  keeps the old .so Arc<Library> → continues running until completion
  
New Request B (after reload):
  receives the new .so Arc<Library> → runs the new code

When Request A completes:
  old Arc<Library> refcount = 0 → old .so is dlclosed automatically
```

**State during reload**: Dev mode fully resets component state (acceptable for development). Production uses external state (DB/Redis), so it is unaffected.

**Additional dependencies for `kernway-server`:**
- `libloading` — cross-platform dlopen/dlclose wrapper
- `notify` — file system watcher (inotify/kqueue/ReadDirectoryChanges)
- `abi_stable` — ensures `.so` ABI stability across minor versions

---

### Backlog — DB Compatibility
> **Decision**: Use a `spawn_blocking` bridge with diesel + r2d2 (v0.3). Do not let this block v0.1-v0.3.



## 0. Mandatory principles (read before writing code for any module)

### 0.0. Industry-standard compliance — highest principle

> **Every module must clearly declare which spec/standard it follows when it is designed.** Implementation follows from the spec, not the other way around. If an RFC says behavior X, the code must implement X — there are no exceptions because something is "more convenient" or "faster." The test suite must cover RFC compliance cases, not just happy paths.

**Why?** Spec compliance is why major frameworks (Nginx, Tomcat, Netty) remain trusted over the long term. Kernway must be built to the same standard.

#### Industry-standard matrix by module

| Module | Spec compliance | Source |
|---|---|---|
| **kernway-core** | `std::future::Future`, `std::error::Error`, `serde::Serialize/Deserialize` | Rust std + de facto |
| **rt-core** (reactor/executor) | POSIX async I/O semantics, `std::future::Future` contract | POSIX, Rust std |
| **rt-net** (TCP layer) | RFC 793 (TCP), RFC 791 (IPv4), RFC 2460 (IPv6) | IETF |
| **http-proto** (HTTP parser) | RFC 9110 (HTTP Semantics), RFC 9112 (HTTP/1.1), RFC 7230-7235 | IETF |
| **web-router** | RFC 3986 (URI Syntax), RFC 9110 §3 (method semantics) | IETF |
| **di-core** | JSR-330 (`javax.inject`) design patterns, compile-time variant | JCP (inspirational) |
| **web-core** (extractors/response) | RFC 9110 (status codes), RFC 8259 (JSON), `serde` traits | IETF + de facto |
| **aop-layer** | OWASP Top 10, RFC 6265 (Cookies), RFC 7617 (Basic Auth), RFC 6750 (Bearer Token) | OWASP + IETF |
| **tls-adapter** | RFC 8446 (TLS 1.3), RFC 5246 (TLS 1.2 legacy) | IETF |
| **http2-proto** | RFC 9113 (HTTP/2), RFC 7541 (HPACK header compression) | IETF |
| **kernleaf** (template engine) | WHATWG HTML Living Standard, OWASP XSS Prevention, RFC 6265 (CSRF/Cookie) | WHATWG + OWASP |
| **kernway-db** | ACID properties (database transactions), RFC 7807 (Problem Details) | ISO + IETF |

#### Mandatory rules when coding each module

```
1. Read the spec BEFORE writing code
   → http-proto: read RFC 9112 before writing the HTTP parser
   → tls-adapter: read RFC 8446 before integrating rustls

2. Every relevant RFC section = at least one test case
   → RFC 9112 §4: request line parsing → its own test case
   → RFC 9112 §6: chunked transfer → its own test case

3. An edge case in the spec = must be handled, never skipped
   → "SHOULD" in an RFC = document the reason when not implemented
   → "MUST" in an RFC = implement it, no exceptions

4. Security specs (OWASP, TLS RFCs) = the highest priority
   → Never release a module with a known security spec violation
```

#### Rust ecosystem de facto specs — must align

```rust
// Every error type MUST implement std::error::Error (Rust std spec)
// Every serializable type MUST support serde (de facto spec, ~200M downloads)
// Every async type MUST use std::future::Future (never define your own Future)
// kernway-core traits MUST build on top of std traits, never replace them

// RIGHT:
pub trait KernwayError: std::error::Error + Send + Sync { ... }
impl<T: serde::Serialize> IntoResponse for Json<T> { ... }

// WRONG:
pub trait KernwayError { fn message(&self) -> &str; } // ignores std::error::Error
```

---

### 0.1. Dependency policy — whitelist (only the following crates are allowed)

| Crate | Why it is allowed | Notes |
|---|---|---|
| `std` + `core::future` | Rust standard | Always allowed |
| `libc` | FFI bindings to OS syscalls — **zero runtime overhead**, avoids UB from ABI mismatches when manually declaring `extern "C"` for structs with complex padding rules | Required for correctness |
| `mio` | Cross-platform I/O event notification (epoll → Linux, kqueue → macOS, IOCP → Windows) — **zero-cost abstraction**, with no executor/scheduler | Required for the cross-platform reactor |
| `httparse` | Zero-copy HTTP/1.1 parser, security-audited, fuzz-tested — used only for the raw parsing layer; business logic is still written by us | Allowed instead of a hand-written parser |
| `rustls` | TLS — writing TLS ourselves is a severe security risk and not worth the tradeoff | Phase 3 only |
| `libloading` | Cross-platform dlopen/dlclose wrapper — zero runtime overhead | `kernway-server` only (v0.5) |
| `notify` | File system watcher (inotify/kqueue/RDCW) — hot reload | `kernway-server` only (v0.5) |
| `abi_stable` | Stable ABI types for the cross-.so boundary | `kernway-abi` + `kernway-server` only (v0.5) |

### 0.2. Dependency policy — blacklist (must never be added)

| Crate | Why it is forbidden |
|---|---|
| `tokio` | Async runtime — Kernway writes its own executor instead |
| `hyper` | HTTP stack that depends on tokio |
| `async-std` | Alternative async runtime |
| `actix-*` | Runtime + actor model, incompatible with the architecture |
| `futures` (crate) | Use only `core::future::Future` from std; do not pull in the `futures` crate |
| `tower` | Middleware framework — Kernway replaces it with a minimal custom `Layer` trait |
| Any other crate | Ask before adding it |

> **Core principle**: The executor, reactor, and task scheduler are Kernway code. Foundation bindings (libc, mio) and security-critical libraries (rustls) may be outsourced. No other exceptions without an equally strong justification.

### 0.3. Design principles

3. **Resolve all DI/routing wiring at compile time via proc macros**. DO NOT use runtime reflection or `Any` downcasts to discover beans. **All singleton beans use `Arc<T>`** for clear ownership — resolved once at bootstrap.
4. **Concurrency architecture: shard-per-core**, not work-stealing. Each OS thread = 1 independent reactor + 1 independent executor, bound to 1 CPU core (best-effort, see Platform Notes), sharing the listening socket via `SO_REUSEPORT`. No cross-core locks on the hot path.
5. **Build time is a technical priority on par with performance.** Every decision (generics vs dyn Trait, macro codegen size, workspace layout) must account for compile-time cost.

### 0.4. Platform notes — cross-platform implementation

> **Philosophy**: Like libuv (Node.js) and .NET Kestrel — implement each platform correctly, do not emulate. Same API, different implementation. User code does not know and does not need to know the difference.

#### Feature matrix

| Feature | Linux | macOS | Windows |
|---|---|---|---|
| I/O event loop | epoll (native, via mio 0.8) | kqueue (native, via mio 0.8) | IOCP (native, via mio 0.8) ✅ |
| Connection distribution | `SO_REUSEPORT` — kernel-level | `SO_REUSEPORT` — kernel-level | Shared socket + multiple `AcceptAsync` threads — distributed by IOCP ✅ |
| Thread-per-core benefit | ✅ Full (pinning + independent executor) | ✅ Partial (independent executor, best-effort pinning) | ✅ Partial (independent executor, pinning works) |
| CPU core affinity | `sched_setaffinity` — guaranteed | `thread_policy_set` — hint; the OS usually respects it | `SetThreadAffinityMask` — works |
| Core count (container-aware) | `std::thread::available_parallelism()` | `std::thread::available_parallelism()` | `std::thread::available_parallelism()` |
| Graceful shutdown | POSIX `SIGTERM`/`SIGINT` | POSIX `SIGTERM`/`SIGINT` | `SetConsoleCtrlHandler` WinAPI |

#### Connection distribution solution — no `SO_REUSEPORT` required on Windows

This is how Kestrel (ASP.NET Core) solves it — Kernway applies the same pattern:

```
Linux/macOS: SO_REUSEPORT           Windows: Shared Socket + IOCP
─────────────────────────           ──────────────────────────────
Core 0: socket_0.accept()           Core 0: shared_socket.accept_async() ─┐
Core 1: socket_1.accept()   kernel  Core 1: shared_socket.accept_async() ─┤ IOCP
Core 2: socket_2.accept()  ──────►  Core 2: shared_socket.accept_async() ─┤ distributes
Core 3: socket_3.accept()  balance  Core 3: shared_socket.accept_async() ─┘

Result: IDENTICAL from the application's point of view
  → Each thread accepts and handles connections entirely independently
  → No cross-core task migration
  → The full cache-locality benefit
  → Measured performance difference: < 2% (Kestrel benchmarks)
```

```rust
// rt-core/src/sys/mod.rs — unified interface
pub fn create_acceptor(addr: SocketAddr, threads: usize) -> Acceptor {
    // Linux/macOS: create N sockets with SO_REUSEPORT
    // Windows: create one socket, N threads calling accept
    // → The same Acceptor API, a different implementation underneath
    sys_impl::create_acceptor(addr, threads)
}

pub fn worker_count() -> usize {
    // Container-aware on every platform (handles cgroups)
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}
```

#### Thread-per-core benefits still apply fully across platforms

Thread-per-core provides **two independent benefits**:

```
Benefit 1: CPU cache locality
  → Requires CPU pinning + SO_REUSEPORT / same-socket-same-thread
  → Linux: ✅ full | macOS: ~80% | Windows: ~90%

Benefit 2: Zero cross-core lock contention (THE MORE IMPORTANT ONE)
  → All it needs: each thread owns its executor and shares no task queue
  → Linux/macOS/Windows: ✅ 100% — a consequence of the independent executor design
  → This is why Glommio, Monoio, and Seastar outperform tokio's work-stealing
```

Node.js cluster, Kestrel, and Nginx all follow the same principle — and all are production-grade on Windows. Kernway can do the same.

#### Mandatory `#[cfg]` rule

```
rt-core/src/sys/
├── linux.rs      # SO_REUSEPORT, sched_setaffinity, SIGTERM handling
├── macos.rs      # SO_REUSEPORT, thread_policy_set, SIGTERM handling
├── windows.rs    # Shared socket accept, SetThreadAffinityMask, SetConsoleCtrlHandler
└── mod.rs        # Re-export unified API — this is the only interface outside code may use
```

**There must never be any `#[cfg(target_os)]` outside the `sys/` directory.** This follows libuv's approach of hiding platform complexity behind a single API.

---

## 1. Workspace structure

```
kernway/
├── Cargo.toml                  # workspace root
├── crates/
│   ├── kernway-core/           # STABLE traits: IntoResponse, FromRequest, TemplateEngine, DbPool, Layer, KernwayPlugin
│   ├── rt-core/                # Phase 0 / v0.2: reactor + executor + Future runtime
│   ├── rt-net/                 # Phase 0 / v0.2: TCP (mio wrap qua rt-core)
│   ├── http-proto/             # Phase 1 / v0.3: HTTP/1.1 parser + writer
│   ├── web-router/             # Phase 1 / v0.3: radix tree routing
│   ├── di-core/                # Phase 1 / v0.1: AppContext, bean registry
│   ├── di-macro/               # Phase 1 / v0.1: proc-macro #[component] #[inject] #[route]
│   ├── web-core/               # Phase 1 / v0.3: Json<T>/Html<T>/Text implement IntoResponse; Path/Query implement FromRequest
│   ├── aop-layer/              # Phase 2 / v0.4: TransactionLayer, RateLimitLayer implement Layer
│   ├── tx-context/             # Phase 2 / v0.4: transaction propagation qua task-local
│   ├── tls-adapter/            # Phase 3 / v0.5: rustls integration
│   ├── http2-proto/            # Phase 3 / v0.5: HTTP/2 HPACK + multiplexing
│   ├── kernway-db/             # v0.3+: PostgresPool/MySqlPool/SqlitePool implement DbPool
│   ├── kernleaf/               # v0.6: KernleafEngine implement TemplateEngine (kw:text, kw:if, kw:each)
│   ├── kernway-abi/            # v0.5: stable ABI definitions for the dynamic .so plugin
│   ├── kernway-server/         # v0.5: standalone pre-compiled binary (hot reload host)
│   ├── kernway-cli/            # v0.5: `kernway dev` hot reload + `kernway build`
│   └── kernway/                # Meta-crate: re-exports everything, `use kernway::prelude::*`
├── examples/
│   ├── echo-server/            # v0.2: benchmark baseline
│   ├── hello-world/            # v0.3: minimal REST API (compiled-in mode)
│   ├── todo-app/               # v0.3-v0.4: CRUD + auth + error handling
│   └── todo-app-plugin/        # v0.5: the same app, but as a cdylib + hot reload
└── benches/                    # comparisons against tokio/hyper (dev-dependencies only)
```

**Crate → release milestone mapping**:
| Crate | Release | Publish to crates.io |
|---|---|---|
| `kernway-core` (trait definitions) | v0.1 | ✅ Foundation, stable after v1.0 |
| `kernway-di` (di-core + di-macro) | v0.1 | ✅ Earliest |
| `kernway-rt` (rt-core + rt-net) | v0.2 | ✅ |
| `kernway-web` (http-proto + web-router + web-core) | v0.3 | ✅ |
| `kernway-aop` (aop-layer + tx-context) | v0.4 | ✅ |
| `kernway-db` (blocking DB bridge) | v0.3+ | ✅ diesel + r2d2 wrapper |
| `kernway-abi` (stable ABI for plugins) | v0.5 | ✅ |
| `kernway-server` (pre-compiled host binary) | v0.5 | ✅ GitHub Releases |
| `kernway-cli` (`kernway dev` hot reload) | v0.5 | ✅ `cargo install kernway-cli` |
| `kernleaf` (Thymeleaf-like template engine) | v0.6 | ✅ `kw:text`, `kw:if`, `kw:each`, security |
| `kernway` (meta-crate, full stack) | v0.3+ | ✅ `use kernway::prelude::*` |

**Dev workflow to keep build times low:**
- Use `mold` or `lld` as the linker (configured in `.cargo/config.toml`).
- Dev profile: `opt-level = 0`, `debug = "line-tables-only"`, `incremental = true`, increased `codegen-units`.
- Use `cargo check` for the regular coding loop, and `cargo build`/`run` only when you need to actually run the code.

---

## 2. Phase 0 — Reactor + Executor + Raw TCP

> **Standards**: RFC 793 (TCP), RFC 791 (IPv4), RFC 2460 (IPv6), POSIX async I/O, `std::future::Future`

### 2.1. Goal
Run a single-core TCP echo server first, then scale to multi-core via `SO_REUSEPORT`. Write the executor/waker/task system ourselves — use `mio` as the cross-platform I/O event source, and DO NOT use any async runtime crate.

> **Why use `mio` for the reactor?** `mio` is only a thin wrapper around epoll/kqueue/IOCP — it has NO executor, NO scheduler, and NO task system. Kernway writes all of that itself. `mio` solves the cross-platform event-notification problem that would otherwise require 3 separate implementations (epoll/kqueue/IOCP) with hundreds of lines of complex `#[cfg]` code.

### 2.2. Task list

- [ ] **Platform sys layer** (`rt-core/src/sys/{linux,macos,windows}.rs`):
  - Use `libc` for socket options (`SO_REUSEPORT`, `SO_REUSEADDR`) and CPU affinity (Linux: `sched_setaffinity`, macOS: `pthread_mach_thread_np`).
  - Windows: `extern "system" { fn SetThreadAffinityMask(...) }` — WinAPI ABI is stable, the only approved exception for manual `extern "system"` declarations.
  - Export a unified interface via `rt-core/src/sys/mod.rs`: `fn set_cpu_affinity(core_id: usize) -> Result<()>`, `fn set_reuse_port(fd: RawFd) -> Result<()>`.
  - **Rule**: There must never be any `#[cfg(target_os)]` outside the `sys/` directory.

- [ ] **Reactor** (`rt-core/src/reactor.rs`):
  - Wrap `mio::Poll` + `mio::Events` (cross-platform event loop).
  - API: `register(source: &mut impl mio::event::Source, interest) -> Token`, `deregister(source)`.
  - Map `Token -> Waker` (use `HashMap<Token, Waker>`, no lock needed — single-thread per core).
  - The `poll_once(timeout)` function calls `mio::Poll::poll()`, and for each ready event it `.wake()`s the corresponding waker.

- [ ] **Executor** (`rt-core/src/executor.rs`):
  - Task queue: `VecDeque<Rc<Task>>` — no need for `Arc`/atomics on a single-thread-per-core design (**why**: avoid unnecessary atomic CAS cost on the hot path).
  - `Task` holds `Pin<Box<dyn Future<Output=()>>>`, using a hand-written `RawWakerVTable` (do not use the `futures` crate).
  - Main loop: `loop { drain task queue → poll() ; reactor.poll_once() → requeue woken tasks }`.
  - Public API: `spawn_local(fut: impl Future<Output=()> + 'static)`.

- [ ] **Custom waker** (`rt-core/src/waker.rs`):
  - `RawWaker` points to `Rc<Task>` (via `Rc::into_raw`), with hand-written `clone/wake/wake_by_ref/drop` implementations.
  - **Why `Rc` instead of `Arc`**: single-thread per core → no atomic refcount needed. The compiler automatically prevents cross-thread Waker sends because `Rc` is not `Send` — the type system replaces a runtime check.

- [ ] **Shard bootstrap** (`rt-net/src/shard.rs`):
  - Function `spawn_shard(core_id: usize, listener: mio::net::TcpListener, app: F)`:
    1. `sys::set_cpu_affinity(core_id)` — best-effort, log a warning if it fails instead of panicking.
    2. Create a dedicated `Reactor` + `Executor` for this thread.
    3. Register `listener` with the reactor and accept connections in the async loop.
    4. For each accepted connection, wrap it in `AsyncTcpStream` and spawn a handling task.
  - Set up `SO_REUSEPORT` on the main thread before spawning shards (via `sys::set_reuse_port`).

- [ ] **AsyncTcpStream** (`rt-net/src/tcp.rs`):
  - Wrap `mio::net::TcpStream`, implementing self-defined `AsyncRead`/`AsyncWrite` traits (do not copy tokio signatures).
  - When read/write returns `WouldBlock`, register interest with the reactor and return `Poll::Pending`.

### 2.3. Phase 0 completion criteria
- The echo server runs on Linux, macOS, and Windows from the same codebase.
- Echo server benchmarks versus the `tokio` echo example show throughput tail latency (`p99`) within 20% (acceptable before optimization).
- Multi-core testing verifies that `SO_REUSEPORT` distributes connections evenly across cores (using `wrk`/`hey`, checking per-core CPU usage).

---

## 3. Phase 1 — HTTP/1.1 + Router + DI Macro

> **Standards**: RFC 9110 (HTTP Semantics), RFC 9112 (HTTP/1.1), RFC 3986 (URI), RFC 8259 (JSON), JSR-330 (DI patterns), `serde` traits

### 3.1. HTTP/1.1 parser (`http-proto`)
- [ ] **Choice**: Use `httparse` as the raw parsing engine (zero-copy, security-audited) and write our own business-logic layer on top (validation, routing context, header normalization).
  - **Reason**: `httparse` has already been fuzz-tested and correctly handles attack vectors such as header injection, request smuggling, and CRLF injection. Writing an HTTP parser from scratch is a common source of CVEs, not a framework differentiator.
  - **What Kernway writes itself**: Request/Response abstraction, header map, body streaming, chunked decode state machine.
- [ ] Use zero-copy where possible: return structs containing `Range<usize>` references into the original buffer instead of cloned `String`s.
- [ ] Limit header sizes to prevent DoS (configurable, default 8KB per header, 64KB total headers).

### 3.2. Router (`web-router`)
- [ ] Data structure: Trie/Radix tree for path matching (Spring-style `@PathVariable`, such as `/users/{id}`).
- [ ] Runtime API: `Router::register(method, path, handler: Arc<dyn Handler>)`.
- [ ] Use a trait object for `Handler` to avoid monomorphization blow-up (keeping build times low) — use generics only in the response-serialization layer.

### 3.3. DI macro (`di-macro` + `di-core`)
- [ ] `#[component]` attribute macro: generate code that registers constructors into `AppContext` in `main()`, with NO reflection.
- [ ] `#[inject]` field attribute: parse the struct field type and generate resolution code from `AppContext` (based on a `TypeId` map, resolved once at bootstrap — not per request).
- [ ] Detect missing beans / circular dependencies at **macro expansion time** by building a dependency graph in a build script or dedicated lint (do not force complex const-eval — keep it simple so build times stay low).
- [ ] `#[route(GET, "/users/{id}")]`: generate registration code that calls `Router::register`.

### 3.4. Phase 1 completion criteria
- A simple CRUD REST API (in-memory store) runs with DI + routing, built from `#[component]`/`#[route]`.
- Measure clean build time (`cargo clean && cargo build`) as the baseline for comparison as more modules are added.

---

## 4. Phase 2 — AOP + Transaction Propagation

> **Standards**: OWASP Top 10, RFC 6265 (Cookies/CSRF), RFC 6750 (Bearer Token), RFC 7617 (Basic Auth), RFC 7807 (Problem Details for HTTP APIs — error response format), ACID transaction properties

- [ ] `aop-layer`: design a custom `Layer` trait (similar to `tower::Layer` but minimal, without pulling in `tower`). The `#[transactional]` and `#[require_role("ADMIN")]` macros wrap function bodies into closures that run through the layer.
- [ ] `tx-context`: propagate transaction context using a **self-implemented task-local** mechanism (do not use `tokio::task_local!`).
  - **Warning to address**: because tasks run on the custom executor, task-local state must be attached to the `Task` struct (each task has its own context slot), and it must be explicitly forwarded when spawning child tasks — document this clearly to avoid lost-context bugs.

---

## 5. Phase 3 — TLS + HTTP/2

> **Standards**: RFC 8446 (TLS 1.3), RFC 5246 (TLS 1.2 legacy), RFC 9113 (HTTP/2), RFC 7541 (HPACK), RFC 7301 (ALPN — TLS extension used to negotiate HTTP/2)

- [ ] Integrate `rustls` through `tls-adapter` (exception rationale: writing a TLS record layer ourselves is a severe security risk and not an acceptable tradeoff).
- [ ] HTTP/2: HPACK + multiplexing — lowest priority, only do it once Phases 0-2 are stable and there is real demand.

---

## 6. Notes for AI when generating code

- **Standards first**: Before generating code for any module, identify the spec/RFC that module must follow (see the table in section 0.0). Generated code must reflect the spec — do not optimize before it is spec-correct.
- **Rust ecosystem alignment**: Every error type implements `std::error::Error`. Every data type supports `serde`. All async uses `std::future::Future`. Do not redefine what std/serde already provides.
- **Ownership model for DI**: All singleton beans use `Arc<T>`. Request-scoped context uses `Rc<T>` (not `Send`, exists only inside one task). Never mix the two — if unsure, ask first.
- Always prefer `Arc<dyn Trait>`/`Rc<dyn Trait>` in the wiring layer (DI, router); generic monomorphization should only be used in hot paths that truly need zero-cost behavior (buffer parsing, serialization).
- Code comments should explain the "why" (design rationale) and **cite the relevant RFC section** when applicable.
- Every module needs dedicated test files for **RFC compliance cases** — not just happy paths.
- **Allowed crate list in `Cargo.toml`**: `libc`, `mio`, `httparse`, `rustls`, `libloading`, `notify`, `abi_stable` (v0.5 only). Do not add any other crate without approval first.
- **Platform**: All platform-specific code must live in `rt-core/src/sys/`. Outside code must not contain `#[cfg(target_os)]`.

---

## 7. Implementation order — aligned with release milestones

### v0.1 — kernway-di (target: 6-8 weeks)
1. `di-core`: `AppContext`, `TypeId`-based bean registry, circular dependency detection
2. `di-macro`: `#[component]`, `#[inject]` attribute macros
3. `di-macro`: compile-time dependency graph validation
4. Tests: unit tests + integration tests with a sample app
5. Publish `kernway-di` to crates.io + write README/docs

### v0.2 — kernway-rt (target: 8-10 weeks after v0.1)
6. `rt-core/sys/`: platform layer (libc bindings, CPU affinity, cross-platform tests)
7. `rt-core`: Reactor wrapping `mio::Poll` + custom Waker
8. `rt-core`: Executor + task system
9. `rt-net`: AsyncTcpStream + shard bootstrap + `SO_REUSEPORT`
10. `examples/echo-server`: benchmark vs tokio, running on Linux/macOS/Windows
11. Publish `kernway-rt`

### v0.3 — kernway-web MVP ✨ (target: 10-12 weeks after v0.2)
12. `http-proto`: Request/Response abstraction on top of `httparse`, body streaming
13. `web-router`: Radix-tree router
14. `web-core`: Request extractors (`Path<T>`, `Query<T>`, `Json<T>`, `Header<T>`)
15. `web-core`: Response types (`Json<T>`, `Text`, `Status`, tuples)
16. `di-macro`: `#[controller]`, `#[route]` macros — generate registration code
17. `kernway` meta-crate: `use kernway::prelude::*`, `#[kernway::main]`, `KernwayApp::builder()`
18. `examples/hello-world`: minimal GET/POST
19. `examples/todo-app`: full CRUD with an in-memory store
20. Publish `kernway-web` + `kernway` — **this is when users can build working apps**

### v0.4 — kernway-aop (target: 6-8 weeks after v0.3)
21. `aop-layer`: custom `Layer` trait, middleware chain
22. `tx-context`: task-local transaction context
23. `di-macro`: `#[transactional]`, `#[require_role]`, `#[exception_handler]`
24. `di-macro`: `#[rate_limit]` middleware integration
25. `examples/todo-app` upgrade: JWT auth + error handling + rate limiting

### v0.5 — Production (target: 4-6 weeks after v0.4)
26. `tls-adapter`: rustls integration, `TlsConfig` builder
27. `http2-proto`: HPACK + multiplexing (if there is real demand)
28. Production checklist: graceful shutdown, health check endpoint, structured logging
