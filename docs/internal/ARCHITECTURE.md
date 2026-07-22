# Kernway — Architecture

## Handler API — Design Decision

```rust
#[route(GET, "/{id}")]
async fn get_user(ctrl: &UserController, id: Path<u64>) -> Json<User> {
    ctrl.service.find(*id).await.into()
}
```

**Decision**: Keep `async fn` and `.await` explicit — do not hide them behind macros.

| Option | Decision | Reason |
|---|---|---|
| Hide `.await` through the `#[route]` macro | ❌ Rejected | The macro has no type information → it cannot know which calls return a Future → confusing error messages, broken IDE support |
| Explicit `async fn` + `.await` | ✅ Chosen | IDE/rust-analyzer works correctly, error messages are clear, and users know where I/O happens |

Kernway hides the real boilerplate (state injection, error mapping, route registration).  
`.await` stays visible — it helps users see where I/O occurs.

---

## Philosophy: Spec first, implementation second

> `kernway-core` contains only trait definitions. Not a single line of implementation. Everything else — including Kernway's own crates — is an implementation.

Similar to Java JSR/JCP:

```
Java                          Kernway
────────────────────          ────────────────────────
javax.sql.DataSource    →     trait DbPool
javax.servlet.Filter    →     trait Layer
ViewResolver (Spring)   →     trait TemplateEngine
HttpMessageConverter    →     trait IntoResponse
HandlerMethodArgument   →     trait FromRequest
ApplicationContext      →     trait KernwayPlugin
```

### kernway-core — spec only

```rust
// Đây là TẤT CẢ những gì kernway-core chứa:

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

// kernway-core KHÔNG import: serde, serde_json, diesel, rustls, hay bất kỳ implementation nào
// Compile time của kernway-core: < 1s
```

### Implementation layers

```
kernway-core      (spec — HTTP traits, stable sau v1.0)
kernway-orm-core  (spec — ORM traits, stable sau v0.6) ← tương đương JPA
│
├── [Kernway reference implementations]
│   ├── kernway-web          Json<T>/Html<T> → IntoResponse; Path<T>/Query<T> → FromRequest
│   ├── kernway-db           PostgresPool/MySqlPool/SqlitePool → DbPool
│   ├── kernway-orm-diesel   Repository<T>/QueryBuilder<T> → diesel + spawn_blocking
│   ├── kernleaf             KernleafEngine → TemplateEngine
│   └── kernway-aop          TransactionLayer/RateLimitLayer → Layer
│
└── [Community có thể build thêm — không cần fork kernway]
    ├── kernway-orm-sqlx     Repository<T> → sqlx native async
    ├── kernway-orm-mongodb  Repository<T> → MongoDB
    ├── kernway-mongodb      MongoPool → DbPool
    ├── kernway-redis        RedisPool → DbPool
    ├── kernway-xml          Xml<T> → IntoResponse
    └── kernway-graphql      GraphQL → FromRequest + IntoResponse
```

**ORM spec-first** — similar to JPA:

| JPA | kernway-orm-core |
|---|---|
| JSR-338 interface | Rust traits |
| `@Entity` `@Id` `@Column` | `#[entity]` `#[id]` `#[column]` |
| `JpaRepository<T, ID>` | `Repository<T>` trait |
| `CriteriaBuilder` | `QueryBuilder<T>` trait |
| Hibernate (impl) | `kernway-orm-diesel` (impl) |
| Swap impl via `pom.xml` | Swap impl via `Cargo.toml` |

See: `docs/internal/modules/kernway-orm-core.md`

---

## Crate dependency graph & module independence

**Principle (Spring-style):** a module depends on another **only** when it genuinely
needs it. Subsystems that *can* stand alone *do* stand alone — you can pull the DI
container, the ORM, or the cache into any project without dragging the web stack in.

### Fully independent crates — **zero** internal dependencies

These compile with only external crates (`thiserror`/`syn`/`serde`…) and pull in
**no** other kernway crate. Use any of them à la carte:

| Crate | Role | External deps only |
|---|---|---|
| `kernway-core` | Web/HTTP spec (traits) | `futures-core`, `thiserror` |
| `kernway-orm-core` | ORM spec (traits) | `thiserror` |
| `kernway-cache-core` | Cache spec (traits) | `thiserror` |
| `di-core` | DI container runtime | `thiserror` |
| `rt-core` | Async runtime (Reactor + Executor) | `mio`, `libc` |
| `di-macro` | `#[derive(Component)]` | `syn`/`quote`/`proc-macro2` |
| `kernway-orm-macro` | `#[entity]`/`#[id]`/`#[column]` | `syn`/`quote`/`proc-macro2` |
| `kernway-cache-macro` | `#[cacheable]` | `syn`/`quote`/`proc-macro2` |
| `kernway-openapi` | OpenAPI 3.0 spec gen | `serde`, `serde_json` |

### Legitimate dependency edges (kept)

```
kernway-core ──┬── kernway-http ── kernway-server ──┐ (server also needs di-core + rt-net)
               ├── kernway-web                       │
               ├── kernway-sse                       │
               └── kernway-multipart                 │
                                                     │
di-core ─────────────────────────────────────────────┘

kernway-orm-core ──┬── kernway-orm-memory
                   └── kernway-orm-sqlite   (+ rusqlite)

kernway-cache-core ── kernway-cache-memory

rt-core ── rt-net                            (+ mio, libc)

kernway (facade) ── kernway-core + di-core + di-macro
```

- **Web crates → `kernway-core`**: legitimate — HTTP handlers cannot exist without the
  `Request`/`Response`/`Layer` spec. (Mirrors `spring-web` → `spring-core`.)
- **`kernway-server` → core + http + di-core**: it is the composition/app layer.
- **`rt-net` → `rt-core`**: a socket is meaningless without the reactor that drives
  it. The runtime pair stays clear of `kernway-core` for the same reason DI does —
  a TCP layer must not know about HTTP types.
- **`kernway-server` → `rt-net`**: the server owns the transport, so it is the
  layer that picks a runtime. `kernway-http` deliberately does **not** depend on
  the runtime: it decodes `&[u8]` (`parse_bytes`) and encodes to `Vec<u8>`
  (`encode_response`), never touching a socket, so the same codec serves the
  async server, a blocking tool, or a future HTTP/2 path.

### Two hard rules that keep independence intact

1. **DI and ORM never depend on `kernway-core`.** `kernway-core` is *web-flavoured*
   (`Request`, `Response`, `Layer`, `template`) — not a neutral utility core. A DI
   container or a data-access layer that imported it would leak HTTP types across a
   boundary. `di-core` and `kernway-orm-core` therefore stay at **zero** kernway deps.
2. **A proc-macro crate never depends on its runtime crate.** `di-macro`,
   `kernway-orm-macro`, `kernway-cache-macro` only *emit* token paths
   (`::kernway_orm_core::…`) inside `quote!{}`; those paths resolve in the **user's**
   crate, not in the macro crate. Declaring the runtime crate as a normal dependency
   there is a phantom dep — forbidden.

> **Audit note (2026-07-22):** three phantom/dead dependencies were removed to satisfy
> the rules above — `di-core → kernway-core`, `kernway-openapi → {kernway-core,
> kernway-server}`, and `kernway-orm-macro → kernway-orm-core` (all had **0** uses in
> `src/`). Verified with `cargo tree -e normal` (0 internal deps) and a green
> `cargo test --workspace`.

### Verifying independence

```bash
# Any of these must show NO other kernway/di crate in the tree:
cargo tree -p di-core          -e normal
cargo tree -p kernway-orm-core -e normal
cargo tree -p kernway-openapi  -e normal

# A domain crate must build without the web stack:
cargo build -p kernway-orm-sqlite   # pulls only kernway-orm-core
cargo build -p kernway-cache-memory # pulls only kernway-cache-core
```

**Next lever for flexibility:** no crate uses Cargo `[features]` yet. Feature-gating
optional backends (e.g. `kernway-server` re-exporting `sse`/`openapi`/`multipart`
behind features, `orm` selecting `sqlite`/`memory`) is the planned way to make the
*dependency* side as à-la-carte as the *independence* side already is. See ROADMAP.

---

## Override System — "Defaults + Override anywhere"

> **Mandatory implementation rule**: Every default framework behavior MUST be overridable. No behavior may be hardcoded without an extension point.

This is why Spring is loved — users are not locked into any framework decision. Kernway must provide the same flexibility.

### Mechanism

**Step 1**: The framework registers a default implementation with `#[default_impl]`:

```rust
// Trong kernway crate — default, tự động đăng ký
#[component]
#[default_impl]  // ← "chỉ dùng nếu user CHƯA cung cấp impl trait này"
struct DefaultErrorHandler;
impl ErrorHandler for DefaultErrorHandler {
    fn handle(&self, err: AppError) -> Response {
        Response::status(500).body(err.to_string())
    }
}
```

**Step 2**: If the user does nothing → the framework uses the default.

**Step 3**: If the user wants to override it → define a struct + trait impl, and the framework automatically uses the user's version:

```rust
// Trong user app — override, thắng DefaultErrorHandler
#[component]
struct MyErrorHandler;
impl ErrorHandler for MyErrorHandler {
    fn handle(&self, err: AppError) -> Response {
        Response::status(err.status_code())
            .json(json!({ "error": err.message(), "trace_id": err.trace_id() }))
    }
}
// di-macro detect: đã có ErrorHandler impl → bỏ #[default_impl] bean
// Kiểm tra tại COMPILE TIME — không phải runtime
```

### `#[primary]` — when multiple impls exist

```rust
// Nếu có 2 impl cùng trait → compile error (không im lặng như Spring):
// error: multiple beans found for trait `AuthExtractor`
// help: add #[primary] to one of them, or use #[qualifier("name")]

#[component]
#[primary]   // ← cái này thắng
struct JwtAuth;
impl AuthExtractor for JwtAuth { ... }

#[component]
struct ApiKeyAuth;
impl AuthExtractor for ApiKeyAuth { ... }
```

### Builder-level override — DI not required

```rust
KernwayApp::builder()
    // Override bất kỳ default nào tại đây:
    .error_handler(MyErrorHandler)
    .auth(JwtAuthLayer::new(secret))
    .json_serializer(SimdJsonSerializer)
    .request_id(|| Snowflake::next().to_string())
    .layer(MyLoggingLayer)                         // thêm middleware
    .layer_before::<CorsLayer>(MyRateLimitLayer)   // chèn vào vị trí cụ thể
    .build()
```

### Mandatory extension points (v0.3+)

Every item below MUST have a default + MUST be overridable:

| Extension point | Trait | Default | Override via |
|---|---|---|---|
| Error handling | `ErrorHandler` | 500 plain text | `#[component]` + impl |
| Auth extraction | `AuthExtractor` | No auth (allow all) | `#[component]` + impl |
| Request ID | `RequestIdGenerator` | UUID v4 | `#[component]` + impl |
| JSON serialize | `JsonSerializer` | serde_json | `#[component]` + impl |
| Content negotiation | `ContentNegotiator` | JSON first | `#[component]` + impl |
| Log format | `LogFormatter` | JSON structured | `#[component]` + impl |
| Not found handler | `NotFoundHandler` | 404 JSON | `#[component]` + impl |
| Method not allowed | `MethodNotAllowedHandler` | 405 JSON | `#[component]` + impl |
| Request size limit | `RequestSizeConfig` | 10MB | `#[component]` + impl |
| CORS policy | `CorsPolicy` | Deny all | `#[component]` + impl |
| Template engine | `TemplateEngine` | None (error if used) | `.plugin(KernleafPlugin)` |
| Database pool | `DbPool` | None (error if used) | `.db(PostgresPool::new(...))` |

### Implementation principles (mandatory)

1. **Do not hardcode behavior** — every behavior must go through a trait
2. **Use `#[default_impl]` for all defaults** — never register a default without this annotation
3. **Compile errors on conflicts** — do not silently choose one like Spring sometimes does
4. **Builder overrides always win over DI** — `.error_handler(X)` in the builder > any `#[component]`

### Comparison with Spring

| | Spring | Kernway |
|---|---|---|
| Default mechanism | Runtime `@ConditionalOnMissingBean` | Compile-time `#[default_impl]` |
| Conflict detection | Runtime exception | **Compile error** |
| Override mechanism | `@Primary` / `@Qualifier` | `#[primary]` / builder |
| Override discovery | Runtime classpath scan | Compile-time macro |
| Incorrect override not used | Runtime — hard to debug | Immediate compile error |

---

## Thread-per-core Architecture

```
Mỗi OS thread = 1 CPU core = 1 Reactor + 1 Executor + 1 Task queue

Core 0: [Reactor] ←→ [Executor] ←→ [Task A, Task B, Task C...]
Core 1: [Reactor] ←→ [Executor] ←→ [Task D, Task E, Task F...]
Core 2: [Reactor] ←→ [Executor] ←→ [Task G, Task H, Task I...]
Core 3: [Reactor] ←→ [Executor] ←→ [Task J, Task K, Task L...]

KHÔNG có cross-core task migration.
KHÔNG có shared task queue.
KHÔNG có global lock trên hot path.
```

**Why is this better than work-stealing (tokio)?**

| | Work-stealing (tokio) | Thread-per-core (Kernway) |
|---|---|---|
| Task migration | Can happen at any time | Never |
| CPU cache | Can be invalidated | Always warm |
| Lock contention | Global task queue with locks | None |
| p99 latency | Unpredictable spikes | Consistent |
| p999 latency | Worse | Better (20-50%) |

**Connection distribution per platform:**

- Linux/macOS: `SO_REUSEPORT` — the kernel distributes connections to separate sockets
- Windows: Shared socket + multiple threads calling `AcceptAsync` — distributed by IOCP

Both achieve the same result: each thread accepts connections and handles them fully independently.

---

## Plugin System

```rust
// Thêm tính năng = implement trait + đăng ký
// Core không bao giờ thay đổi

// Thêm template engine:
.plugin(KernleafPlugin::default())

// Đổi database:
.db(MySqlPool::new(env!("DATABASE_URL")))

// Custom response type — không cần hỏi Kernway team:
struct CsvResponse<T>(Vec<T>);
impl<T: ToCsv> IntoResponse for CsvResponse<T> { /* ... */ }

// Custom middleware:
impl Layer for MyAuthLayer { /* ... */ }
```

**Feature flags** — compile only what is used:

```toml
kernway = { version = "0.3", features = ["json"] }              # ~3MB
kernway = { version = "0.5", features = ["json", "templates"] } # ~4MB
kernway = { version = "0.5", features = ["full"] }              # ~6MB
```

---

## Workspace Structure

```
kernway/
├── Cargo.toml
├── crates/
│   ├── kernway-core/     STABLE traits (IntoResponse, FromRequest, TemplateEngine, DbPool, Layer)
│   ├── rt-core/          Reactor (mio), Executor, Waker, Task system
│   ├── rt-net/           TCP, AsyncTcpStream, Shard bootstrap
│   ├── http-proto/       HTTP/1.1 parser + writer (RFC 9112)
│   ├── web-router/       Radix tree router (RFC 3986)
│   ├── di-core/          AppContext, bean registry
│   ├── di-macro/         #[component] #[inject] #[route] #[controller]
│   ├── web-core/         Extractors, response types, #[kernway::main]
│   ├── aop-layer/        #[transactional] #[require_role] #[validated]
│   ├── tx-context/       Task-local transaction context
│   ├── tls-adapter/      rustls integration (RFC 8446)
│   ├── http2-proto/      HTTP/2 (RFC 9113) + HPACK (RFC 7541)
│   ├── kernway-db/       spawn_blocking bridge + diesel/r2d2
│   ├── kernleaf/         Thymeleaf-inspired template engine
│   ├── kernway-abi/      Stable ABI cho Dynamic .so plugin
│   ├── kernway-server/   Pre-compiled binary + hot reload host
│   ├── kernway-cli/      `kernway dev` + `kernway build`
│   └── kernway/          Meta-crate: `use kernway::prelude::*`
├── docs/
│   ├── ARCHITECTURE.md   (file này)
│   ├── ROADMAP.md
│   ├── STANDARDS.md
│   ├── PLATFORM.md
│   ├── DEVELOPMENT.md
│   ├── FEATURES.md
│   └── modules/          Per-module detailed docs
├── examples/
│   ├── echo-server/
│   ├── hello-world/
│   ├── todo-app/
│   └── todo-app-plugin/  hot reload demo
└── benches/
```

---

## App Project Structure

```
my-app/
├── Cargo.toml
└── src/
    ├── main.rs            #[kernway::main]
    ├── lib.rs             module declarations
    ├── config/            #[configuration] beans
    ├── controller/        #[controller] #[route]
    ├── service/           #[component] business logic
    ├── repository/        #[component] + spawn_blocking + diesel
    ├── model/
    │   └── dto/           request/response types
    └── exception/         #[exception_handler]
```

**Spring → Kernway mapping:**

| Spring | Kernway |
|---|---|
| `@SpringBootApplication` | `#[kernway::main]` |
| `@RestController` | `#[controller("/path")]` |
| `@GetMapping` | `#[route(GET, "/path")]` |
| `@Service` | `#[component]` |
| `@Repository` | `#[component]` + `spawn_blocking` |
| `@Autowired` | `#[inject]` |
| `@Transactional` | `#[transactional]` |
| `@PreAuthorize` | `#[require_role("ROLE")]` |
| `@Valid` | `#[validated]` |
| `@ControllerAdvice` | `#[exception_handler]` |
| `application.yml` | `config/app_config.rs` + env |
