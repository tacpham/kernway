# di-core — Dependency Injection Runtime

## Purpose

`AppContext`: bean registry, lifecycle management, dependency graph resolution.

## Standards

- JSR-330 patterns (Dependency Injection for Java) — design inspiration
- Compile-time dependency graph validation (no runtime surprises)

## Override Rules — MUST be implemented correctly

> **Required**: `di-core` must support `#[default_impl]` beans. See `docs/ARCHITECTURE.md#override-system` to understand the full mechanism.

### Bean priority (priority order, high → low)

1. Builder-level override (`.error_handler(X)`) — **always wins**
2. User `#[component]` (without `#[default_impl]`)
3. Framework `#[component]` + `#[default_impl]` — **used only when nothing above is present**

```rust
pub struct AppContext {
    beans: HashMap<TypeId, BeanEntry>,
}

struct BeanEntry {
    bean: Arc<dyn Any + Send + Sync>,
    is_default: bool,   // true nếu từ #[default_impl]
    is_primary: bool,   // true nếu có #[primary]
}

impl AppContext {
    pub fn register<T>(&mut self, bean: T, is_default: bool) {
        let type_id = TypeId::of::<T>();
        match self.beans.get(&type_id) {
            // Default đã có — user bean thắng, bỏ default
            Some(existing) if existing.is_default => {
                self.beans.insert(type_id, BeanEntry { bean: Arc::new(bean), is_default: false, .. });
            }
            // User bean đã có, thêm default — bỏ default
            Some(_) if is_default => { /* skip */ }
            // Conflict thật — 2 user beans cùng type
            Some(existing) if !existing.is_default && !is_default => {
                panic!("compile error: multiple beans for {:?} — add #[primary]", type_id);
            }
            None => {
                self.beans.insert(type_id, BeanEntry { bean: Arc::new(bean), is_default, .. });
            }
        }
    }
}
```



```rust
pub struct AppContext {
    beans: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl AppContext {
    /// Register a singleton bean.
    pub fn register<T: Any + Send + Sync>(&mut self, bean: T) {
        self.beans.insert(TypeId::of::<T>(), Arc::new(bean));
    }

    /// Get a bean — panics if not found (indicates misconfigured app, not runtime error).
    pub fn get<T: Any + Send + Sync>(&self) -> Arc<T> {
        self.beans
            .get(&TypeId::of::<T>())
            .and_then(|b| b.downcast_ref::<Arc<T>>().cloned())
            .expect("bean not found — check #[component] registration")
    }

    /// Try get a bean.
    pub fn try_get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> { ... }
}
```

## Bean Scopes

| Scope | Rust type | Lifecycle |
|---|---|---|
| Singleton (default) | `Arc<T>` | Created once at startup |
| Request-scoped | `Rc<T>` | Created per request (not Send) |
| Prototype | `fn() -> T` | Created on every inject |

## Circular Dependency Detection

Detected at compile time by `di-macro`:

```
error[kernway-di]: Circular dependency detected
  --> src/service/user_service.rs:5
   |
   | UserService → OrderService → UserService (cycle!)
   |
   = help: extract the shared dependency into a separate component
```

## Bootstrap Order

```rust
// di-macro generates this code from #[component] annotations:
fn bootstrap(ctx: &mut AppContext) {
    // Topological sort of dependency graph
    // Leaf dependencies first
    let db_pool = PostgresPool::new(config.database_url);
    ctx.register(db_pool);

    let user_repo = UserRepository::new(ctx.get::<PostgresPool>());
    ctx.register(user_repo);

    let user_service = UserService::new(ctx.get::<UserRepository>());
    ctx.register(user_service);
    // ...
}
```
