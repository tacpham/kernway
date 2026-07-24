# Kernway

**A Spring Boot-inspired Rust web framework — its own async runtime, thread-per-core, standards-compliant.**

> **Status: 0.1, pre-release.** The DI container, routing, HTTP/1.1, and the
> sharded async transport work and are benchmarked
> ([docs/design/BENCHMARKS.md](docs/design/BENCHMARKS.md)). Async handlers,
> static-file serving, templates, and htmx are planned, not built — see the
> [milestones](docs/design/MILESTONES.md). This README describes where the
> project is going; sections below are marked when they describe a target rather
> than today.

[![Rust](https://img.shields.io/badge/rust-1.78%2B-orange.svg)](https://www.rust-lang.org)
[![License](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)

---

## Why Kernway?

| Feature | Spring Boot | Axum/Actix | **Kernway** |
|---|---|---|---|
| DI / IoC container | ✅ `@Autowired` | ❌ manual | ✅ `#[inject]` |
| Annotation-driven | ✅ | ❌ | ✅ |
| ORM spec (JPA-like) | ✅ JPA/Hibernate | ❌ | ✅ kernway-orm-core |
| Cache abstraction | ✅ `@Cacheable` | ❌ | ✅ `#[cacheable]` |
| OpenAPI generation | ✅ SpringDoc | plugin | ✅ built-in |
| Own async runtime | ❌ (JVM) | ❌ (tokio) | ✅ (`rt-core`) |
| Thread-per-core scheduling | ❌ | optional | ✅ default |
| Cross-platform | ✅ (JVM) | ✅ | ✅ |
| Native binary | ❌ | ✅ | ✅ |

> Kernway ships its own async runtime (`rt-core`: reactor, executor, waker,
> timers) rather than depending on tokio. It is not "no async" — it is async on a
> thread-per-core scheduler, where a task stays on the core that accepted it.
> The one third-party primitive underneath is `mio`, for portable
> epoll/kqueue/IOCP.

---

## Quick Start

### Prerequisites
- [Docker Desktop](https://www.docker.com/products/docker-desktop/) (no Rust installation needed)
- PowerShell 5.1+

### New project
```powershell
# Scaffold a new project
.\kw.ps1 new my-api
cd my-api

# Run
..\kw.ps1 run
```

### Hello World
```rust
use di_core::AppContext;
use di_macro::Component;
use kernway_core::response::IntoResponse;
use kernway_server::{middleware::LoggingMiddleware, KernwayApp};
use kernway_web::Json;

#[derive(Component)]
struct HelloService;

impl HelloService {
    fn greet(&self) -> &str { "Hello from Kernway!" }
}

fn main() {
    let mut ctx = AppContext::new();
    ctx.build::<HelloService>().unwrap();

    KernwayApp::builder()
        .bind("0.0.0.0:8080")
        .context(ctx)
        .layer(LoggingMiddleware)
        .get("/hello", |_req, ctx| {
            let msg = ctx.get::<HelloService>().unwrap().greet();
            Json(serde_json::json!({"message": msg})).into_response()
        })
        .build()
        .run();
}
```

---

## Features

### Dependency Injection
```rust
trait UserRepo: Send + Sync { fn find(&self, id: u64) -> Option<User>; }

#[derive(Component)]
#[provides(dyn UserRepo)]                 // register concrete under the interface
pub struct PgUserRepo { /* ... */ }

#[derive(Component)]
pub struct UserService {
    #[inject]
    repo: Arc<dyn UserRepo>,              // inject by interface (Spring-style)

    #[inject(qualifier = "db_url")]
    db_url: Arc<String>,                  // pick a named bean
}

// Register in ANY order — refresh() topologically wires the whole graph
// (and returns DiError on missing/circular deps instead of panicking).
ctx.register_component::<UserService>()
   .register_component::<PgUserRepo>();
ctx.refresh()?;
```
> Interfaces injected as `Arc<dyn Trait>` must declare `Send + Sync` supertraits.
> The manual path (`ctx.build::<T>()` in dependency order) still works.

### ORM (spec + in-memory impl)
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[entity(table = "users")]
pub struct User {
    #[id(strategy = "auto")]
    pub id: u64,
    pub name: String,
}

// InMemoryRepository implements Repository<User> — swap with diesel/sqlx in production
let repo = InMemoryRepository::<User>::new();
repo.save(User { id: 0, name: "Alice".into() }).unwrap();
```

### Caching
```rust
// Manual cache-aside (v0.5)
let cache = InMemoryCache::<u64, User>::new();
let user = cache.get_or_load(id, Ttl::minutes(5), || {
    Ok(repo.find_by_id(&id)?.unwrap())
})?;

// Future: #[cacheable(key = "user_{id}", ttl = 300)] via AOP (v0.6+)
```

### Middleware
```rust
KernwayApp::builder()
    .layer(RequestIdMiddleware)  // adds X-Request-Id
    .layer(LoggingMiddleware)    // [200] GET /users - 2ms
    .layer(MyCustomMiddleware)   // implement Middleware trait
```

### OpenAPI 3.0
```rust
let mut api = OpenApiRegistry::new("My API", "1.0.0");
api.add_route(
    RouteDoc::new("Get user")
        .path_param("id", "User ID", "integer")
        .response_json(200, "User", "#/components/schemas/User"),
    "GET", "/users/{id}",
);
// Serves at GET /openapi.json
```

### Server-Sent Events
```rust
.get("/events", |_req, _ctx| {
    SseStream::new(vec![
        SseEvent::data("connected"),
        SseEvent::with_id("1", "update", r#"{"type":"ping"}"#),
    ]).into_response()
})
```

---

## Architecture

```
kernway (meta-crate)
├── kernway-core          — Request, Response, StatusCode, Layer traits
├── di-core               — AppContext, BeanEntry, Buildable
├── di-macro              — #[derive(Component)], #[inject]
├── kernway-http          — HTTP/1.1 parser + writer (RFC 9112, pure std)
├── kernway-web           — Json<T>, Path<T>, Query<T>, ProblemDetail (RFC 7807)
├── kernway-server        — Router, KernwayApp, Middleware chain
├── kernway-orm-core      — Entity, Repository<T>, QueryBuilder<T> traits (JPA-inspired)
├── kernway-orm-macro     — #[entity], #[id], #[column]
├── kernway-orm-memory    — InMemoryRepository<T> for testing
├── kernway-cache-core    — Cache<K,V>, Ttl, CacheStats traits
├── kernway-cache-macro   — #[cacheable], #[cache_evict], #[cache_update]
├── kernway-cache-memory  — InMemoryCache<K,V> for testing
├── kernway-openapi       — OpenAPI 3.0 spec generation
├── kernway-sse           — SseEvent, SseStream (W3C EventSource)
└── kernway-multipart     — Multipart/form-data parser (RFC 7578)
```

**API docs**: `cargo doc --workspace --no-deps --open`. Every crate has a
`//!` header covering what it does, the flow through it, and why it is shaped
that way — start with `di-core`, `kernway-core`, or `kernway-orm-core`.

**Why it is shaped that way**: the decisions that are expensive to reverse are
written down as [KEPs](docs/kep/) — Kernway Enhancement Proposals, modelled on
Rust's RFC process. Each one records what was rejected and what the choice costs,
not only what was chosen.

---

## Performance

Measured on an Apple M2 Max, macOS, release build. Full table and reproduction
steps in [docs/design/BENCHMARKS.md](docs/design/BENCHMARKS.md).

| What | Measured |
|---|---|
| Full request pipeline (parse→route→handle→encode) | 363 ns |
| DI bean lookup (`#[inject]`) | 4.2 ns |
| `TypeId` hasher vs SipHash | 5.8× faster |
| Parse a browser GET (8 headers) | 705 ns |
| Encode a small JSON response | 44 ns |
| Spawn a task | ~65 ns at 1000 tasks |

Measured against the incumbent, not just ourselves. The router is a radix trie
tuned over three rounds against `matchit` (axum's): **static routing is within
1.5×** (21 ns vs 14 ns), and parameterised routing went from 77× behind to 5.8×
and is now flat in route count instead of O(n). The remaining param gap is an
owned-vs-borrowed-parameters API choice, not the trie — recorded, with its
reason, in [BENCHMARKS.md](docs/design/BENCHMARKS.md). We write down where we
still lose, not only where we win.

Routing does not get slower as the application grows: every class — static hit,
param hit, and miss — is flat in route count, because a radix trie is O(path
length), not O(routes).

**Not yet measured**, and so not claimed: requests/sec, p99 latency, or any
comparison against another framework's throughput. Those need a load test that
does not exist yet — the table above is in-process micro-benchmarks only, and the
[benchmarks doc](docs/design/BENCHMARKS.md) says so explicitly.

---

## Examples

| Example | Milestone | What it shows |
|---|---|---|
| `hello-di` | v0.1 | Manual DI — register/get beans |
| `hello-di-v2` | v0.2 | `#[derive(Component)]` auto-wiring |
| `hello-di-v3` | v0.3 | `refresh()` auto-ordering + cycle detection, `Arc<dyn Trait>` injection, qualifiers |
| `hello-web` | v0.3 | REST API, JSON, path params, 404 Problem Detail |
| `todo-orm` | v0.4 | ORM + Middleware (logging, request-id) |
| `hello-cache` | v0.5 | Cache-aside pattern, TTL, hit/miss stats |
| `hello-openapi` | v0.6 | OpenAPI docs + SSE + Multipart |
| **`todo-app`** | **v1.0** | **Full-featured flagship: all features combined** |

Run any example:
```powershell
.\kw.ps1 run todo-app
```

---

## `kw.ps1` Commands

| Command | Description |
|---|---|
| `.\kw.ps1 build` | Build all workspace crates |
| `.\kw.ps1 test` | Run all tests |
| `.\kw.ps1 run <name>` | Run an example |
| `.\kw.ps1 check` | Type-check without building |
| `.\kw.ps1 clippy` | Lint |
| `.\kw.ps1 fmt` | Format code |
| `.\kw.ps1 new <name>` | Scaffold a new Kernway project |
| `.\kw.ps1 shell` | Open interactive shell in build container |
| `.\kw.ps1 clean-cache` | Clear Cargo registry cache |

---

## Spring Boot → Kernway Mapping

| Spring Boot | Kernway |
|---|---|
| `@Component` / `@Service` | `#[derive(Component)]` |
| `@Autowired` | `#[inject]` on field |
| `@Qualifier("name")` | `#[inject(qualifier = "name")]` |
| inject by interface | `#[inject] Arc<dyn Trait>` + `#[provides(dyn Trait)]` |
| `@Autowired(required=false)` | `#[inject] Option<Arc<T>>` |
| inject `List<T>` / `Map` | `#[inject] Vec<Arc<dyn Trait>>` |
| `@PostConstruct` | `#[post_construct(method)]` |
| `@PreDestroy` / `DisposableBean` | `impl Drop` (deterministic RAII) |
| `ApplicationContext.refresh()` | `ctx.refresh()` (topological auto-wiring) |

> Full DI reference & Spring comparison: [docs/DEPENDENCY_INJECTION.md](docs/DEPENDENCY_INJECTION.md)
| `@RestController` | `#[derive(Component)]` + route registration |
| `@GetMapping("/path")` | `.get("/path", handler)` |
| `@RequestBody` | `serde_json::from_slice(&req.body)` |
| `@PathVariable` | `Path::<T>::from_request(req, "name")` |
| `@RequestParam` | `req.query.get("name")` |
| `@Entity` | `#[entity(table = "name")]` |
| `@Id` | `#[id(strategy = "auto")]` |
| `@Column` | `#[column(name = "col")]` |
| `JpaRepository` | `impl Repository<T>` via `#[repository]` |
| `@Cacheable` | `#[cacheable(key, ttl)]` (v0.6 marker, AOP in v1.x) |
| `@CacheEvict` | `#[cache_evict(key)]` |
| `ResponseEntity` | `(StatusCode, Json<T>).into_response()` |
| `@ResponseStatus(404)` | `ProblemDetail::not_found(...)` (RFC 7807) |
| `@ControllerAdvice` | custom `Middleware` impl |
| `HandlerInterceptor` | `impl Middleware for MyMiddleware` |
| `@Transactional` | `#[transactional]` marker (impl in v1.x) |

---

## Roadmap

- **v1.0** ✅ Stable API, full feature set, flagship example
- **v1.1** Real DB drivers: `kernway-orm-diesel` (PostgreSQL, MySQL, SQLite)
- **v1.2** `kernway-cache-redis` (Redis via `redis-rs`)
- **v1.3** TLS (`rustls`), HTTP/2
- **v2.0** AOP codegen (full `#[cacheable]` / `#[transactional]` implementation)

---

## License

GPL-3.0 — see [LICENSE](LICENSE)