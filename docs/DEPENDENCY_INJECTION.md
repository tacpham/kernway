# Kernway Dependency Injection

This document describes Kernway's DI system after the upgrade: goals, what was
built, how to use it, and — most importantly — a **comparison with Spring**:
where Kernway is *equivalent*, where it does *better*, and where it **deliberately
does not** copy Spring because Rust has a more effective idiom.

> Philosophy: keep the familiar Spring-style *syntax* (`#[derive(Component)]` +
> `#[inject]`) but with **zero runtime magic** — no reflection, no runtime
> proxies. Everything is compile-time codegen + static types.

---

## 1. Overview of what was built

| Group | Feature | Main API | Status |
|---|---|---|---|
| Wiring | Automatic dependency ordering + cycle detection | `register_component::<T>()` + `refresh()` | ✅ |
| Safety | Missing bean → `DiError` instead of a panic | `Buildable::build → Result` | ✅ |
| Polymorphism | Inject by interface `Arc<dyn Trait>` | `#[provides(dyn T)]`, `register_as`/`get_as` | ✅ |
| Identity | Qualifier / named injection | `#[inject(qualifier = "…")]`, `register_qualified` | ✅ |
| Ergonomics | Optional injection | `#[inject] Option<Arc<T>>` | ✅ |
| Ergonomics | Collection injection (all beans of a type) | `#[inject] Vec<Arc<dyn T>>`, `get_all`/`get_all_as` | ✅ |
| Lifecycle | Post-construct hook (receives `Arc<Self>`) | `#[post_construct(method)]` | ✅ |

**Files:** `crates/di-core/src/{context,buildable,error,lib}.rs`, `crates/di-macro/src/lib.rs`.
**Runnable example:** `examples/hello-di-v3/`. **Tests:** `crates/di-core/src/context.rs` (runtime) + `crates/di-macro/tests/derive.rs` (derive).

---

## 2. Usage (by feature)

### 2.1 Auto-ordering + cycle detection
Register components in **any order**; `refresh()` topologically sorts (Kahn) and
builds them in the right order, or returns a `DiError`.

```rust
ctx.register_component::<UserController>()   // needs UserService
   .register_component::<UserService>()      // needs UserRepo
   .register_component::<UserRepo>();         // no deps
ctx.refresh()?;   // wires everything, in order
```
- Missing provider → `DiError::MissingDependency`.
- Hard cycle A→B→A → `DiError::CircularDependency { cycle }`.
- Beans registered with `register_instance` before `refresh` count as "already present".

### 2.2 Result instead of panic
`Buildable::build` returns `Result<Arc<Self>, DiError>`; generated code uses `?`.
Misconfiguration is a controlled startup error, not a runtime crash.

### 2.3 Inject by interface
```rust
trait UserRepo: Send + Sync {           // ⚠️ Send + Sync supertraits are REQUIRED
    fn find(&self, id: u64) -> Option<String>;
}

#[derive(Component)]
#[provides(dyn UserRepo)]                // register the concrete under the interface
struct PgUserRepo { /* … */ }

#[derive(Component)]
struct UserService {
    #[inject] repo: Arc<dyn UserRepo>,   // inject by interface
}
```
Because Kernway has no reflection, the **producer must declare** `#[provides(dyn T)]` —
trading a little explicitness for zero-cost wiring.

### 2.4 Qualifier / named injection
```rust
#[derive(Component)]
struct Config {
    #[inject(qualifier = "db_url")] url: Arc<String>,
}
ctx.register_qualified::<String>("db_url", Arc::new("postgres://…".into()))?;
```

### 2.5 Optional injection
```rust
#[derive(Component)]
struct Svc {
    #[inject] cache: Option<Arc<Cache>>,   // missing → None, not an error
}
```

### 2.6 Collection injection (plugin / strategy)
```rust
#[derive(Component)]
struct Registry {
    #[inject] plugins: Vec<Arc<dyn Plugin>>,   // ALL providers of dyn Plugin
}
```
`refresh()` guarantees **every** provider is built before the consumer (it counts
providers within the batch), so the `Vec` always sees the complete set.

### 2.7 Lifecycle hook `#[post_construct]`
Runs **after** the bean is built, registered, and its bindings published, in
dependency order. Receives `Arc<Self>` — which `build()` does not yet have.
```rust
#[derive(Component)]
#[post_construct(start)]
struct Worker { /* … */ }

impl Worker {
    fn start(self: &Arc<Self>, ctx: &AppContext) -> Result<(), DiError> {
        // register self as a listener, spawn a thread holding Arc<Self>, warm a cache…
        Ok(())
    }
}
```

---

## 3. Comparison with Spring — where they are **EQUIVALENT**

Capabilities Kernway matches (different syntax, same concept):

| Spring | Kernway | Notes |
|---|---|---|
| `@Component` / `@Service` | `#[derive(Component)]` | Marks the bean + generates wiring metadata |
| `@Autowired` (field) | `#[inject]` on a field | Codegen can access private fields too |
| Constructor injection | the derived `build()` body | Fully wired the moment it is constructed |
| Inject by interface | `#[inject] Arc<dyn T>` + `#[provides(dyn T)]` | Program-to-interface |
| `@Qualifier("name")` | `#[inject(qualifier = "name")]` | Select a bean by name |
| `@Primary` | `BeanEntry.is_primary` | Disambiguation |
| `@Autowired(required=false)` / `Optional<T>` | `#[inject] Option<Arc<T>>` | Optional injection |
| Inject `List<T>` / `Map<String,T>` | `#[inject] Vec<Arc<dyn T>>` | Collection injection |
| `ApplicationContext.refresh()` | `ctx.refresh()` | Topo-sort of the whole graph |
| Circular dependency detection | `DiError::CircularDependency` | Caught at `refresh()` |
| `@PostConstruct` | `#[post_construct(method)]` | Hook after wiring, with `Arc<Self>` |
| `ApplicationContextAware` | `build(ctx)` / hook receives `ctx` | Access to the container |

---

## 4. Comparison with Spring — where Kernway is **BETTER** (or more effective)

This is the core insight: many Spring "features" are really *workarounds for the
JVM*. Rust already has a better mechanism, so Kernway **does not need** to emulate them.

| Problem | Spring solves it with | Kernway | Why Kernway is better |
|---|---|---|---|
| Releasing resources (close conn, flush) | `@PreDestroy` / `DisposableBean` (manual, because GC is non-deterministic) | **`Drop`** (RAII) | When `AppContext` drops, every `Arc<T>` is released *deterministically*, in ownership order — Spring builds machinery to emulate what Rust gives for free |
| Fallible init after wiring | split `constructor` + `@PostConstruct` | `build()` **returns `Result`** | Construct + fallible-init fused into one, cleaner; `#[post_construct]` is only for init needing `Arc<Self>` |
| Object exists but not fully wired ("half-initialized window") | field/setter injection → needs `@PostConstruct` to confirm wiring | constructor injection | The object **never** exists in a half-wired state — fully wired the moment it appears |
| AOP (`@Transactional`, `@Cacheable`) | runtime proxies + `BeanPostProcessor` (dynamic dispatch, reflection) | **macro codegen** (planned) | Compile-time wrapping, **zero-cost**, no proxy, no extra vtable |
| Bean thread-safety | developer's responsibility, container proxies | beans are **immutable** `Arc<T>` | Read-only sharing across request threads is *type-safe*, lock-free |
| Startup cost | classpath scan + reflection | compile-time derive | No runtime reflection; the graph is **fail-fast validated** at `refresh()` |
| Missing-bean bug | may throw at runtime | `DiError` at `refresh()` (or a compile error on a type mismatch) | Detected earlier, type-safe |

---

## 5. What Kernway **DELIBERATELY DOES NOT** copy from Spring

Not missing features — deliberate design decisions, because Rust has a better idiom.

| Spring feature | Why it stays out of the container | The right Rust approach |
|---|---|---|
| **Setter injection** | Beans are immutable `Arc<T>`; setters require post-construction mutation → wrapping each field in `Mutex`/`RwLock`, adding locks, breaking read-only sharing | Constructor injection (already have it) |
| **Request/Session scope in the container** | The container is shared as read-only `Arc<AppContext>` across threads; a request-scoped bean would break that model | Request-local state passed via `Request`/extensions, not via DI |
| **`@PreDestroy` as a separate hook** | Redundant with `Drop` | `impl Drop for Bean` |
| **Runtime proxies / `BeanPostProcessor`** | Rust prefers compile-time wrapping | Macro codegen (`#[cacheable]`, `#[transactional]`) |

*(Can be added later if genuinely needed: `build_prototype::<T>()` for prototype scope; `shutdown()` for reverse-order teardown if plain `Drop` is not enough; `#[value("key")]` for config — all optional, not required.)*

---

## 6. Invariants & important notes

- **⚠️ Injectable interfaces must declare `Send + Sync` supertraits** (`trait Repo: Send + Sync`). Otherwise `dyn Repo` is not `Send + Sync` → the `Arc<dyn Any + Send + Sync>` type-erasure and `downcast` **will not compile**. This is load-bearing for interface injection.
- Trait objects must be `'static` (the default inside `Arc<…>`). Traits with a lifetime are not supported.
- `qualifier` applies only to a single required `Arc<T>` field — not to `Option`/`Vec` (compile error otherwise).
- `refresh()` is re-entrancy safe (components registered *during* a build are picked up on a later pass) and handles mutual soft-dep stalls (builds the rest, ignoring soft ordering).
- The old manual path (`ctx.build::<T>()` in dependency order) still works — fully backward compatible.

---

## 7. Internals (summary)

- **Storage**: `AppContext.instances: TypeIdMap<Vec<(BeanEntry, Arc<dyn Any + Send + Sync>)>>`. Trait bindings live in the **same map**, keyed by `TypeId::of::<Arc<dyn Trait>>()`, value is `Arc<Arc<dyn Trait>>` (double-Arc — `Arc<dyn Trait>` is Sized + Any).
- **Hot-path hasher**: uses a passthrough `TypeIdHasher` (`TypeId` is already a hash) instead of the default SipHash → `get::<T>()` is ~2× faster, **zero dependencies** (pure std).
- **`get` safety**: `register_with_entry` **validates** the concrete type matches `entry.type_id` (returns `DiError::TypeMismatch` on mismatch) → the downcast in `get` is *infallible*, no panic from safe code.
- **Wiring**: `register_component` pushes a `ComponentDef { deps, opt_deps, provides, build }`. `refresh()` runs Kahn: a hard dep needs ≥1 provider (`available`), a soft dep waits for **all** batch providers (via `pending_providers`).
- **Codegen**: the derive emits `Buildable::build` (using `?`), `RegistersComponent::{dependencies, optional_dependencies, provides, register_bindings, post_construct}`, and `KernwayComponent`.

## 8. `trait Container` — the architectural seam

`Buildable::build` is generic over [`Container`] rather than hard-wired to `AppContext`:
```rust
fn build<C: Container + ?Sized>(ctx: &C) -> Result<Arc<Self>, DiError>;
```
`Container` is a read-only view (`get`/`get_qualified`/`get_as`/`get_all`/`get_all_as`).
`AppContext` implements it. This lets a component be built against **any**
container — a child/scoped context, or a **mock in a unit test** — without a real
`AppContext`:
```rust
struct Mock { repo: Arc<Repo> }
impl Container for Mock { /* get::<Repo>() returns repo, the rest NotFound/empty */ }
let svc = Service::build(&Mock { repo })?;   // no AppContext / refresh needed
```
> `Container` is not object-safe (it has generic methods) → use `C: Container` /
> `&impl Container`, not `&dyn Container`.

## 9. Known limitations / roadmap

- **Trait bindings have no primary/qualifier yet**: two `#[provides(dyn T)]` → `get_as` returns `Ambiguous`; use `get_all_as` (collection). The macro does not parse `#[primary]` for trait bindings.
- **`MissingDependency`** currently names only the dependent bean, not *which* type is missing (a `TypeId` carries no name).
- Prototype scope / lazy init / `#[value("key")]`: not yet (optional, future work).

> Rust version / MSRV policy: [VERSIONING.md](VERSIONING.md)

---

## 10. Verification

```bash
cargo build --workspace                                   # clean
cargo test  --workspace                                   # 139 passed / 0 failed
cargo run   -p hello-di-v3                                # demo of all 7 features
cargo clippy --workspace --all-targets -- -D warnings     # clean across the workspace
```
