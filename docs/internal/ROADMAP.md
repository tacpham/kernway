# Kernway — Roadmap

## Release Milestones

| Version | Key features | Production-ready? | Target |
|---|---|---|---|
| **v0.1** | `kernway-di`: `#[component]` `#[inject]` — usable on Axum immediately | ❌ | 6-8 weeks |
| **v0.2** | `kernway-rt`: cross-platform TCP runtime, echo server benchmark | ❌ | +8-10 weeks |
| **v0.3** | `kernway-web` MVP: REST API, Config, Static files, Health checks, Testing, Logging | ✅ Dev | +10-12 weeks |
| **v0.4** | AOP, Validation, Security, **`kernway-orm-core` + `kernway-orm-sqlx`** (PG, MySQL, SQLite) | ✅ Production | +8-10 weeks |
| **v0.5** | TLS/HTTP2, Hot reload, CLI, **`kernway-cache`** (Redis: Cache trait, `#[cacheable]`, pub/sub, lock) | ✅ + DX | +6-8 weeks |
| **v0.6** | WebSocket, OpenAPI, `kernleaf`, ORM relationships, **`kernway-orm-mssql`** (SQL Server), **`kernway-orm-mongo`** (MongoDB) | ✅ Full | +8-10 weeks |
| **v1.0** | Stable API, `kernway-orm-core` spec frozen, benchmark, migration guide | ✅ Stable | TBD |
| **v1.x** | `kernway-orm-oracle` (Oracle via ODBC) — community maintained | ✅ Enterprise | Community |

---

## v0.1 — kernway-di

**Goal**: Spring developers can use DI in an existing Axum project immediately.

- [ ] `di-core`: `AppContext`, `TypeId`-based registry, circular dependency detection
- [ ] `di-macro`: `#[component]`, `#[inject]` attribute macros
- [ ] Compile-time dependency graph validation
- [ ] Publish `kernway-di` to crates.io + write the README

**Deliverable**:
```toml
# Thêm vào project Axum hiện tại
kernway-di = "0.1"
```

---

## v0.2 — kernway-rt

**Goal**: The custom async runtime runs on Linux/macOS/Windows.

- [ ] `rt-core/sys/`: platform layer (libc bindings, CPU affinity, graceful shutdown)
- [ ] Reactor wrapping `mio::Poll` + custom Waker
- [ ] Executor + Task system (`VecDeque<Rc<Task>>`, `RawWakerVTable`)
- [ ] `rt-net`: `AsyncTcpStream` wrapping `mio::net::TcpStream`
- [ ] Shard bootstrap: `SO_REUSEPORT` (Linux/macOS) + shared socket (Windows)
- [ ] `examples/echo-server`: benchmark vs tokio

**Criterion**: p99 must not deviate by more than 20% from the tokio echo example.

---

## v0.3 — kernway-web MVP ✨

**Goal**: Users can build and run a complete REST API.

- [ ] `http-proto`: Request/Response on `httparse` (RFC 9112), body streaming
- [ ] `web-router`: Radix tree router (RFC 3986)
- [ ] `web-core`: Extractors (`Path<T>`, `Query<T>`, `Json<T>`, `Header<T>`)
- [ ] `web-core`: Response types (`Json<T>`, `Text`, `StatusCode`, tuples)
- [ ] `di-macro`: `#[controller]`, `#[route]` — generate registration code
- [ ] `kernway`: meta-crate, `use kernway::prelude::*`, `#[kernway::main]`
- [ ] Config system: `#[configuration]`, env vars, dev/staging/prod profiles
- [ ] **`kernway-log`**: `LogPlugin`, `#[logged]`, `info!/debug!/warn!/error!` macros, JSON + Pretty format, access logs, request context (MDC)
- [ ] Static file serving: `.static_files("/assets", "public/")`
- [ ] Health checks: `/health`, `/ready` endpoints
- [ ] Graceful shutdown with drain timeout
- [ ] Testing: `TestApp`, mock beans, `#[kernway::test]`
- [ ] `examples/todo-app`: full CRUD

**Deliverable**: Developers can build and run an app without needing anything else.

---

## v0.4 — Production-ready

**Goal**: Kernway apps are ready for production deployment + ORM (SQL databases).

- [ ] `aop-layer`: `Layer` trait, middleware chain
- [ ] `di-macro`: `#[transactional]`, `#[require_role]`, `#[exception_handler]`
- [ ] Validation: `#[validated]`, `Validated<T>` extractor, RFC 7807 error format
- [ ] Observability: `tracing` integration, structured JSON logs, request ID
- [ ] **`kernway-log`** v0.4: file output, rotation/archive, per-module level, sensitive field masking, OpenTelemetry
- [ ] Metrics: Prometheus endpoint `/metrics` (OpenMetrics spec)
- [ ] Security layers: CORS, CSRF, HSTS, CSP, X-Frame-Options
- [ ] Rate limiting: per-IP, per-user, configurable windows
- [ ] Request timeout + request size limit
- [ ] **`kernway-orm-core`**: spec — `Entity`, `Repository<T>`, `QueryBuilder<T>`, `OrmTransaction` traits + `#[entity]` `#[repository]` macro specs
- [ ] **`kernway-orm-sqlx`**: reference implementation — PostgreSQL + MySQL + SQLite (1 crate, native async, no `spawn_blocking` required)
- [ ] `#[entity]` macro: field → column mapping, auto `impl Entity`, index hints
- [ ] `#[repository]` macro: auto-generate `find_by_*`, `exists_by_*`, `count_by_*`, `delete_by_*`
- [ ] Lambda query: `.filter(|u| u.email == email).order_by_desc(|u| u.created_at).fetch_page(0, 20)`
- [ ] `#[transactional]` + `OrmTransaction`: automatic commit/rollback

---

## v0.5 — Developer Experience + Redis

**Goal**: Hot reload, TLS, HTTP/2, cache layer.

- [ ] `tls-adapter`: rustls (RFC 8446 TLS 1.3)
- [ ] `http2-proto`: HPACK + multiplexing (RFC 9113)
- [ ] **`kernway-cache`** (Redis): `Cache<K,V>` trait, `fred` driver, `#[cacheable]`, `#[cache_evict]`
- [ ] Redis extras: pub/sub, distributed lock, rate limiting via Lua script
- [ ] `kernway-abi`: stable ABI definitions
- [ ] `kernway-cli`: `kernway dev` (watch + rebuild + reload), `kernway build`
- [ ] `examples/todo-app-plugin`: same app, cdylib + hot reload

---

## v0.6 — Full-featured

**Goal**: Feature-complete enough to build any web app.

- [ ] WebSocket: `WebSocket` extractor, `WsHandler` trait (RFC 6455)
- [ ] `kernleaf`: template engine (`kw:text`, `kw:if`, `kw:each`, `kw:authorize`, CSRF)
- [ ] OpenAPI 3.0: `#[openapi]` macro, `/openapi.json`, `/swagger-ui`
- [ ] Multipart / file upload (RFC 7578)
- [ ] i18n: message bundles, `Accept-Language` header
- [ ] SSE (Server-Sent Events): `SseStream` response type (W3C EventSource)

---

## v1.0 — Stable

- [ ] Stable API (semver guarantee)
- [ ] TechEmpower benchmark submission
- [ ] Migration guide: Spring Boot → Kernway
- [ ] Production deployment guide (Docker, Kubernetes)
- [ ] `cargo-kernway` project scaffold: `cargo kernway new my-app`
