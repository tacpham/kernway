# Kernway

**A Spring Boot-inspired Rust web framework — its own async runtime, thread-per-core, standards-compliant.**

> **Status: 0.1, pre-release — M1–M4 complete.**
> Built and tested: DI container, routing, HTTP/1.1, sharded async transport,
> static file serving (ETag, Range, precompressed `.br`/`.gz`), htmx support,
> and the `kernleaf` template engine (Thymeleaf dialect) with CSRF and
> security headers.
> Benchmarked ([docs/design/BENCHMARKS.md](docs/design/BENCHMARKS.md)).
> **Not yet built**: async handlers, hot reload, production CLI.
> See [milestones](docs/design/MILESTONES.md) for what is next.

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

// Future: #[cacheable(key = "user_{id}", ttl = 300)] via AOP (M5+)
```

### Static Files

```rust
KernwayApp::builder()
    .static_files("public/")               // serve from a directory
    .precompressed()                        // opt-in: probe .br / .gz next to each file
    .build()
    .run();
// ETag + Cache-Control added automatically
// Conditional GET (If-None-Match → 304) without reading the body
// Range requests (RFC 7233) for large files
// Symlink-escape defence: canonical path re-checked under root
```

### htmx (`features = ["htmx"]`)

```rust
use kernway_htmx::{Htmx, HtmxResponse, Swap};

.get("/greet", |req, _ctx| {
    let htmx = Htmx::from_request(&req);
    let fragment = "<p>Hello!</p>";
    let full    = "<html>…<p>Hello!</p>…</html>";
    HtmxResponse::builder()
        .trigger("greeted")              // HX-Trigger header
        .respond(fragment, full, htmx)   // fragment for htmx, page for browser
                                         // sets Vary: HX-Request automatically
})
```

### Templates — `kernleaf` (`features = ["web"]`)

```html
<!-- templates/users/profile.html — natural template, works as plain HTML -->
<h1 th:text="${user.name}">John Doe</h1>
<ul>
    <li th:each="post : ${user.posts}" th:text="${post.title}">Post title</li>
</ul>
<!-- CSRF token injected automatically into every POST form -->
<form method="POST" action="/profile/update">
    <input type="text" name="name" th:value="${user.name}">
    <button type="submit">Update</button>
</form>
```

```rust
// th:text auto-escapes HTML — a template cannot introduce XSS.
// Raw HTML only through th:utext (explicitly unsafe).
.get("/users/{id}/profile", |req, ctx| {
    let user = ctx.get::<UserService>().unwrap().find(id);
    Template::render("users/profile", context! { user })
})
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
kernway (meta-crate — one dependency, feature-gated)
├── kernway-core          — Request, Response, StatusCode, Body, Layer traits
├── di-core               — AppContext, BeanEntry, Buildable
├── di-macro              — #[derive(Component)], #[inject]
├── kernway-http          — HTTP/1.1 parser + writer (RFC 9112, pure std)
├── kernway-web           — Json<T>, Path<T>, Query<T>, Html<T>, ProblemDetail (RFC 7807)
├── kernway-server        — Router, KernwayApp, Middleware chain, static file serving
├── kernway-static        — ETag, Range, precompressed negotiation (RFC 7233)
├── kernway-htmx          — Htmx extractor, HtmxResponse builder  [feature: htmx]
├── kernleaf              — Template engine (Thymeleaf dialect, compile-to-IR) [feature: web]
├── kernway-security      — CSRF (double-submit), security headers, SecurityContext
├── kernway-orm-core      — Entity, Repository<T>, QueryBuilder<T> traits (JPA-inspired)
├── kernway-orm-macro     — #[entity], #[id], #[column]
├── kernway-orm-memory    — InMemoryRepository<T> for testing
├── kernway-orm-sqlite    — SQLite driver (rusqlite)
├── kernway-cache-core    — Cache<K,V>, Ttl, CacheStats traits
├── kernway-cache-macro   — #[cacheable], #[cache_evict], #[cache_update]
├── kernway-cache-memory  — InMemoryCache<K,V> for testing
├── kernway-openapi       — OpenAPI 3.0 spec generation
├── kernway-sse           — SseEvent, SseStream (W3C EventSource)
├── kernway-multipart     — Multipart/form-data parser (RFC 7578)
├── rt-core               — Reactor, Executor, Task, Waker, timers (mio underneath)
└── rt-net                — AsyncTcpStream, SO_REUSEPORT shards
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
| htmx header read vs axum-htmx 0.8 | 1.39× faster (57.8 ns vs 80.2 ns) |
| kernleaf render vs minijinja | 1.7× faster |
| Precompressed static negotiation overhead | ~200 ns; −50–55% payload |

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

| Example | Status | What it shows |
|---|---|---|
| `hello-di` | ✅ | Manual DI — register/get beans |
| `hello-di-v2` | ✅ | `#[derive(Component)]` auto-wiring |
| `hello-di-v3` | ✅ | `refresh()` auto-ordering + cycle detection, `Arc<dyn Trait>` injection, qualifiers |
| `hello-web` | ✅ | REST API, JSON, path params, 404 Problem Detail |
| `web-docker` | ✅ | Static files + JSON routes + Docker, 34.9 MB distroless image |
| `login-htmx` | ✅ | htmx fragments + kernleaf templates + CSRF |
| `todo-orm` | ✅ | ORM + Middleware (logging, request-id) |
| `todo-sqlite` | ✅ | ORM with SQLite driver |
| `hello-cache` | ✅ | Cache-aside pattern, TTL, hit/miss stats |
| `hello-openapi` | ✅ | OpenAPI docs + SSE + Multipart |
| `hello-log` | ✅ | Structured logging |
| `hello-config` | ✅ | Config profiles |
| `hello-validate` | ✅ | Validation + RFC 7807 error responses |
| `hello-controller` | ✅ | `#[derive(Component)]` controller pattern |
| `todo-app` | ⏳ | Flagship: all features combined |

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

Progress follows the [walking-skeleton milestones](docs/design/MILESTONES.md) — each step must run end-to-end before the next begins.

| Milestone | Status | What it delivers |
|---|---|---|
| **M1** — skeleton in Docker | ✅ 2026-07-24 | Static files, health checks, distroless image (34.9 MB), graceful shutdown |
| **M1a** — close the front door | ✅ 2026-07-24 | `kernway` as a single dependency; `use kernway::prelude::*` |
| **M2a** — conditional GET | ✅ 2026-07-24 | ETag, 304, symlink defence |
| **M2b** — streaming + HEAD + Range | ✅ 2026-07-24 | `Body::File`, Range (206/416), precompressed `.br`/`.gz` |
| **M3** — htmx | ✅ | `features = ["htmx"]`; 1.39× faster than axum-htmx |
| **M4** — templates + security | ✅ | `kernleaf` (Thymeleaf dialect, 1.7× vs minijinja), CSRF, security headers |
| **M5** — hot reload | ⏳ | Template/static changes < 10 ms (no restart); Rust changes via socket handover |
| **M6** — production build | ⏳ | `kernway build` → one static binary, assets embedded, allocator decision |

---

## License

GPL-3.0 — see [LICENSE](LICENSE)