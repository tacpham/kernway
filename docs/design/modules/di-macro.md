# di-macro — DI Attribute Macros

## Purpose

Procedural macros: `#[component]`, `#[inject]`, `#[controller]`, `#[route]`, `#[default_impl]`, `#[primary]`.  
Generate bootstrap code, route registration — zero runtime reflection.

> **Required when implementing**: See `docs/ARCHITECTURE.md#override-system` — every framework default bean MUST use `#[default_impl]`, and conflicts MUST produce a compile error.

## Macros

### `#[component]`

```rust
// Input:
#[component]
struct UserService {
    #[inject]
    repo: Arc<UserRepository>,
    #[inject]
    config: Arc<AppConfig>,
}

// Generated code (simplified):
impl KernwayComponent for UserService {
    fn register(ctx: &mut AppContext) {
        let repo = ctx.get::<UserRepository>();
        let config = ctx.get::<AppConfig>();
        ctx.register(UserService { repo, config });
    }
    fn dependencies() -> &'static [TypeId] {
        &[TypeId::of::<UserRepository>(), TypeId::of::<AppConfig>()]
    }
}
```

### `#[controller]`

```rust
// Input:
#[controller("/users")]
struct UserController {
    #[inject]
    service: Arc<UserService>,
}

// Generated: registers all #[route] methods + mounts to router at "/users"
```

### `#[route]`

```rust
// Input:
#[route(GET, "/{id}")]
async fn get_user(&self, Path(id): Path<u64>) -> impl IntoResponse {
    Json(self.service.find_by_id(id).await?)
}

// Full path: GET /users/{id}
// Generated: registers handler in router with path parameter extraction
```

### `#[default_impl]` — Framework default (can be overridden)

```rust
// Used for the framework's DEFAULT implementations.
// A user overrides one by defining a #[component] impl of the same trait.

#[component]
#[default_impl]   // ← required on every framework default
struct DefaultErrorHandler;
impl ErrorHandler for DefaultErrorHandler { ... }

// Generated:
impl KernwayComponent for DefaultErrorHandler {
    fn register(ctx: &mut AppContext) {
        ctx.register_default(DefaultErrorHandler);  // is_default = true
    }
}
// → If the user already has a #[component] impl of ErrorHandler, this bean is skipped automatically
```

### `#[primary]` — Resolve conflict

```rust
// Two #[component]s implementing the same trait → compile error.
// Resolve it with #[primary].

#[component]
#[primary]   // ← this one wins
struct JwtAuth;
impl AuthExtractor for JwtAuth { ... }

#[component]
struct ApiKeyAuth;
impl AuthExtractor for ApiKeyAuth { ... }
// Neither carries #[primary] → compile error:
// error[kernway]: multiple beans implement `AuthExtractor`
// help: add #[primary] to the one that should be used by default
```

### `#[exception_handler]`

```rust
// Input:
#[exception_handler]
async fn handle_not_found(_err: NotFoundError) -> impl IntoResponse {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" })))
}

// Generated: registers as fallback error handler in AppContext
```

## Compile-time Checks

1. **Dependency graph**: all `#[inject]` fields must have registered `#[component]`
2. **Circular deps**: topological sort detects cycles
3. **Route conflicts**: duplicate paths with same method = compile error
4. **Handler signature**: wrong extractor type = compile error with helpful message
5. **Bean conflict**: 2 non-default beans implement same trait without `#[primary]` = compile error
6. **Override validation**: if a `#[default_impl]` bean is overridden → log a warning in debug builds

## syn features

```toml
# di-macro/Cargo.toml
syn = { version = "2", features = ["derive", "parsing", "proc-macro"] }
# NOT features = ["full"] — keeps compile time down
```
