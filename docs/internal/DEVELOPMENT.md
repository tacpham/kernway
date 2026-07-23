# Kernway — Development Guide

## Dependency Policy

### Whitelist (allowed)

| Crate | Reason |
|---|---|
| `libc` | Zero-cost FFI bindings for platform syscalls — no runtime overhead |
| `mio` | Cross-platform I/O event notification (native epoll/kqueue/IOCP) |
| `httparse` | RFC-compliant HTTP/1.1 parser, no-alloc, battle-tested |
| `rustls` | Memory-safe TLS 1.3 (RFC 8446) — does not use OpenSSL |
| `libloading` | Dynamic library loading for hot reload (.so/.dll/.dylib) |
| `notify` | Cross-platform file system watching for hot reload |
| `abi_stable` | Stable ABI representation across dynamic library boundaries |
| `socket2` | Portable socket configuration (SO_REUSEPORT, SO_REUSEADDR) |
| `syn` | Procedural macro parsing — use only minimal features |
| `quote` | Procedural macro code generation |
| `proc-macro2` | Procedural macro token stream |
| `serde` | Serialization framework (features = ["derive"]) |
| `serde_json` | JSON support |
| `diesel` | Sync ORM, battle-tested, compile-time query checking |
| `r2d2` | Connection pooling for diesel |
| `tracing` | Spans + structured logging — async-aware |
| `tracing-subscriber` | Log output formatting (JSON, pretty) |
| `rolling-file` | Log file rotation + compression |

### Blacklist (not allowed)

| Crate | Prohibited because |
|---|---|
| `tokio` | Kernway builds its own runtime — conflict + binary bloat |
| `async-std` | Same as tokio |
| `hyper` | Built on tokio — conflict |
| `futures` (crate) | Use only `core::future::Future` and `core::task::*` from std |
| `tower` | Tokio ecosystem |
| `actix-*` | Different ecosystem |
| `num_cpus` | Not container-aware — use `std::thread::available_parallelism()` |
| `openssl` | Replaced by rustls |

### Rules for adding new dependencies

1. Does the dependency actually solve a platform problem or an RFC requirement?
2. How many extra seconds does it add to compilation? (target: total clean build < 30s)
3. Is it part of the tokio/async-std ecosystem?
4. Could a smaller in-house implementation be built in under 1 day?

---

## Build Time Strategy

Target: **clean build < 20s**, incremental rebuild < 3s.

### Structure for reducing build time

```toml
# Don't:
syn = { version = "2", features = ["full"] }   # parses the entire Rust AST

# Do:
syn = { version = "2", features = ["derive"] } # only what is needed
```

**Rule for di-macro and other macro crates:**
- `syn` must use only the required features
- Split the macro crate (`di-macro`) from the runtime crate (`di-core`)
- `di-macro` compile time must be < 3s on its own

**Rule for framework layers:**
- Use `dyn Trait` instead of `impl Trait` / generic `<T: Trait>` where monomorphization is not needed
- Register handlers through trait objects, not generics

```rust
// Don't (compiles N copies):
fn handle<H: Handler>(&mut self, handler: H) { ... }

// Do (one copy, vtable dispatch):
fn handle(&mut self, handler: Box<dyn Handler>) { ... }
```

**Workspace setup:**
- `cargo-hakari` workspace-hack crate — avoid duplicate dependencies
- Linker: `mold` (Linux), `lld` (Windows/macOS)
- Separate profiles: `[profile.dev]` vs `[profile.release]`

```toml
# Cargo.toml workspace root
[profile.dev]
opt-level = 0
debug = 1    # not 2 — keeps debug info smaller
split-debuginfo = "unpacked"

[profile.release]
opt-level = 3
lto = "thin"     # not "fat" — faster, and nearly as good
codegen-units = 1
strip = "symbols"
```

---

## AI Coding Guide

Kernway uses AI (Claude) to generate code. These rules help the AI produce correct code:

### Patterns the AI should follow

```rust
// 1. Platform code always lives in sys/ — no #[cfg] in business logic
#[cfg(target_os = "linux")]
pub fn pin_thread(core: usize) -> io::Result<()> { /* ... */ }
// → In sys/linux.rs, not reactor.rs

// 2. Trait objects cho extensibility
pub struct Router {
    routes: Vec<(Method, String, Box<dyn Handler>)>,
}
// → Box<dyn Handler>, not Vec<(Method, String, fn(Request) -> Response)>

// 3. Concrete error types
#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("route not found: {0}")]
    NotFound(String),
    #[error("method not allowed")]
    MethodNotAllowed,
}
// → Not Box<dyn Error> or String

// 4. Doc comments carry an RFC reference
/// Parse HTTP request line.
///
/// Implements RFC 9112 §3: Request Line
/// Format: `method SP request-target SP HTTP-version CRLF`
pub fn parse_request_line(input: &[u8]) -> Result<RequestLine, ParseError> { /* ... */ }
```

### Common AI mistakes — verify these

1. **Using `tokio::` or `async_std::`** — check imports after every generated code block
2. **Using `num_cpus`** — replace with `std::thread::available_parallelism()`
3. **Using `syn` with `features = ["full"]`** — include only the required features
4. **Putting `#[cfg]` in reactor/executor** — move it down into `sys/`
5. **Using generic handlers instead of trait objects** — check whether `Handler` is dyn or generic

---

## Workspace Cargo.toml

```toml
[workspace]
members = [
    "crates/kernway-core",
    "crates/rt-core",
    "crates/rt-net",
    "crates/http-proto",
    "crates/web-router",
    "crates/di-core",
    "crates/di-macro",
    "crates/web-core",
    "crates/aop-layer",
    "crates/tx-context",
    "crates/tls-adapter",
    "crates/http2-proto",
    "crates/kernway-db",
    "crates/kernleaf",
    "crates/kernway-abi",
    "crates/kernway-server",
    "crates/kernway-cli",
    "crates/kernway",    # meta-crate
]

[workspace.dependencies]
kernway-core = { path = "crates/kernway-core" }
libc         = "0.2"
mio          = { version = "0.8", features = ["net", "os-poll"] }
httparse     = "1"
rustls       = "0.23"
serde        = { version = "1", features = ["derive"] }
serde_json   = "1"
tracing      = "0.1"
syn          = { version = "2", features = ["derive"] }
quote        = "1"
proc-macro2  = "1"

[profile.dev]
opt-level = 0
debug = 1

[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
strip = "symbols"
```

---

## Testing Strategy

```rust
// Unit test — no network needed
#[test]
fn router_finds_correct_handler() { /* ... */ }

// Integration test — TestApp
#[kernway::test]
async fn test_get_user() {
    let app = TestApp::new(app_config()).await;
    let res = app.get("/users/1").await;
    assert_eq!(res.status(), 200);
}

// Mock beans
#[kernway::test]
async fn test_service_with_mock_repo() {
    let app = TestApp::builder()
        .mock::<UserRepository>(MockUserRepository::new())
        .build()
        .await;
    // ...
}
```

---

## Benchmarking

Run benchmarks:

```bash
cargo bench                    # all benchmarks
cargo bench -- router          # router only
cargo bench -- http_parse      # HTTP parser only
```

Compare against:
- `axum` (tokio)
- `actix-web`
- `hyper` bare

Target: within 10% of axum for throughput, better than axum for p99/p999 latency.
