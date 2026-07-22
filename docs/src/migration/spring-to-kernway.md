# Spring Boot → Kernway Migration

**A practical guide for moving a Spring Boot service to Kernway v1.0.**

Kernway is intentionally Spring-shaped, but its implementation model is different:

- synchronous, thread-per-connection HTTP
- no Tokio or async runtime
- native Rust binaries
- explicit wiring where Spring would often use runtime reflection

This guide focuses on the APIs that exist today and clearly labels what is still a marker or roadmap item.

---

## Migration mindset

Migrate in layers rather than rewriting everything at once.

1. **Model the domain** in Rust (`struct`, `enum`, serde)
2. **Port repositories** to `Repository<T>` and `InMemoryRepository<T>` for local validation
3. **Port services** into `AppContext`-managed components
4. **Port controllers** into `KernwayApp::builder()` route registrations
5. **Add cross-cutting features**: middleware, cache, OpenAPI, SSE
6. **Replace infrastructure** piece by piece as real DB/cache drivers arrive

A good first target is a CRUD module with one entity, one service, and one controller.

---

## Annotation and concept mapping

| Spring Boot | Kernway | Status / notes |
|---|---|---|
| `@Component` / `@Service` | `#[derive(Component)]` | Supported |
| `@Autowired` | `#[inject]` on `Arc<T>` field | Supported |
| `@RestController` | `#[derive(Component)]` + route registration | Supported pattern |
| `@GetMapping("/path")` | `.get("/path", handler)` | Supported |
| `@PostMapping("/path")` | `.post("/path", handler)` | Supported |
| `@PutMapping("/path")` | `.put("/path", handler)` | Supported |
| `@PatchMapping("/path")` | `.patch("/path", handler)` | Supported |
| `@DeleteMapping("/path")` | `.delete("/path", handler)` | Supported |
| `@RequestBody` | `serde_json::from_slice(&req.body)` | Supported |
| `@PathVariable` | `Path::<T>::from_request(req, "name")` | Supported |
| `@RequestParam` | `req.query.get("name")` or `Query<T>::from_request(req)` | Supported |
| `@Entity` | `#[entity(table = "name")]` | Supported |
| `@Id` | `#[id(strategy = "auto")]` | Supported |
| `@Column` | `#[column(name = "col")]` | Supported metadata |
| `JpaRepository` | `impl Repository<T>` via `#[repository]` | Marker-style API / evolving |
| `@Cacheable` | `#[cacheable(key, ttl)]` | Marker today; manual cache-aside is the working pattern |
| `@CacheEvict` | `#[cache_evict(key)]` | Marker today |
| `ResponseEntity<T>` | `(StatusCode, Json<T>).into_response()` | Supported |
| `@ResponseStatus(404)` | `ProblemDetail::not_found(...)` | Supported |
| `@ControllerAdvice` | custom `Middleware` impl | Supported pattern |
| `HandlerInterceptor` | `impl Middleware for MyMiddleware` | Supported |
| `@Transactional` | `#[transactional]` marker | Planned implementation in v1.x |

---

## Dependency injection

### Spring

```java
@Service
@RequiredArgsConstructor
public class UserService {
    private final UserRepository repo;
}
```

### Kernway

```rust
use di_macro::Component;
use std::sync::Arc;

#[derive(Component)]
pub struct UserService {
    #[inject]
    repo: Arc<UserRepository>,
}
```

### What changes?

- Spring resolves dependencies at runtime through reflection and bean metadata.
- Kernway generates wiring code at compile time.
- Injected fields should use `Arc<T>`.
- Fields without `#[inject]` must be `Default` or be initialized manually.

If a service needs runtime state that is not DI-managed, register it manually:

```rust
let repo = Arc::new(UserRepository::new());
ctx.register_instance::<UserRepository>(Arc::clone(&repo)).unwrap();
```

---

## Controllers and routing

Spring uses annotations on methods. Kernway v1.0 uses fluent route registration.

### Spring

```java
@RestController
@RequestMapping("/users")
public class UserController {
    @GetMapping("/{id}")
    public User get(@PathVariable Long id) { ... }
}
```

### Kernway

```rust
KernwayApp::builder()
    .get("/users/{id}", |req, ctx| {
        let id = match Path::<u64>::from_request(req, "id") {
            Ok(id) => *id,
            Err(err) => return ProblemDetail::bad_request(err),
        };

        match ctx.get::<UserService>().unwrap().get(id) {
            Some(user) => Json(user).into_response(),
            None => ProblemDetail::not_found(format!("user {} not found", id)),
        }
    })
```

### Request mapping equivalents

| Spring | Kernway |
|---|---|
| `@GetMapping` | `.get(...)` |
| `@PostMapping` | `.post(...)` |
| `@PutMapping` | `.put(...)` |
| `@PatchMapping` | `.patch(...)` |
| `@DeleteMapping` | `.delete(...)` |

---

## Request extraction

### Path parameters

```java
@GetMapping("/{id}")
public User get(@PathVariable Long id) { ... }
```

```rust
let id = Path::<u64>::from_request(req, "id")?;
```

### Query parameters

```java
@GetMapping
public List<User> list(@RequestParam(required = false) Boolean active) { ... }
```

```rust
let active = req.query.get("active").and_then(|v| match v.as_str() {
    "true" => Some(true),
    "false" => Some(false),
    _ => None,
});
```

### JSON body

```java
@PostMapping
public User create(@RequestBody CreateUserRequest body) { ... }
```

```rust
let body: CreateUser = match serde_json::from_slice(&req.body) {
    Ok(body) => body,
    Err(err) => return ProblemDetail::bad_request(format!("invalid body: {}", err)),
};
```

---

## Responses and errors

Spring often uses `ResponseEntity` plus exception handlers. Kernway keeps responses explicit.

### Spring

```java
return ResponseEntity.status(HttpStatus.CREATED).body(user);
```

### Kernway

```rust
let mut resp = Json(user).into_response();
resp.status = StatusCode::CREATED;
resp
```

For standard failures, use `ProblemDetail` helpers:

```rust
ProblemDetail::not_found("user 42 not found")
ProblemDetail::bad_request("invalid id")
ProblemDetail::internal_error("db unavailable")
```

This maps cleanly from:

| Spring | Kernway |
|---|---|
| `@ResponseStatus(HttpStatus.NOT_FOUND)` | `ProblemDetail::not_found(...)` |
| `ResponseEntity<T>` | `(StatusCode, Json<T>).into_response()` or manual response mutation |
| `@ControllerAdvice` | middleware or explicit response helpers |

---

## ORM model mapping

### Spring JPA entity

```java
@Entity
@Table(name = "todos")
public class Todo {
    @Id
    @GeneratedValue(strategy = GenerationType.IDENTITY)
    private Long id;

    @Column(nullable = false)
    private String title;
}
```

### Kernway entity

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[entity(table = "todos")]
pub struct Todo {
    #[id(strategy = "auto")]
    pub id: u64,
    pub title: String,
}
```

### Repository pattern

Kernway's stable implementation style today is a small wrapper around `InMemoryRepository<T>` or a future real driver.

```rust
pub struct TodoRepository {
    inner: Arc<InMemoryRepository<Todo>>,
}

impl TodoRepository {
    pub fn new() -> Self {
        Self { inner: Arc::new(InMemoryRepository::new()) }
    }

    pub fn find_by_id(&self, id: u64) -> Option<Todo> {
        self.inner.find_by_id(&id).unwrap_or(None)
    }
}
```

Think of this as the Kernway equivalent of a hand-written Spring Data adapter while the ecosystem grows.

---

## Service layer

### Spring

```java
@Service
public class TodoService {
    private final TodoRepository repo;

    public Todo complete(Long id) { ... }
}
```

### Kernway

```rust
pub struct TodoService {
    repo: Arc<TodoRepository>,
}

impl TodoService {
    pub fn complete(&self, id: u64) -> Option<Todo> {
        let mut todo = self.repo.find_by_id(id)?;
        todo.done = true;
        Some(self.repo.save(todo))
    }
}
```

Use `#[derive(Component)]` when all fields can be injected or defaulted. Register manually when the service must construct internal state such as caches or mutexes.

---

## Caching

Spring has `@Cacheable` as a full runtime feature. Kernway v1.0 provides the abstraction plus in-memory implementation.

### Stable pattern today: manual cache-aside

```rust
use kernway_cache_core::{Cache, Ttl};
use kernway_cache_memory::InMemoryCache;

pub struct UserService {
    repo: Arc<UserRepository>,
    cache: Arc<InMemoryCache<u64, User>>,
}

pub fn get(&self, id: u64) -> Option<User> {
    if let Ok(Some(cached)) = self.cache.get(&id) {
        return Some(cached);
    }

    let user = self.repo.find_by_id(id)?;
    let _ = self.cache.put(id, user.clone(), Ttl::minutes(1));
    Some(user)
}
```

### Mapping

| Spring | Kernway |
|---|---|
| `@Cacheable` | `#[cacheable(...)]` marker + manual cache-aside |
| `@CacheEvict` | `#[cache_evict(...)]` marker + explicit `cache.evict(...)` |

---

## Middleware vs interceptors/advice

### Spring

- `HandlerInterceptor`
- servlet filters
- `@ControllerAdvice`

### Kernway

```rust
KernwayApp::builder()
    .layer(RequestIdMiddleware)
    .layer(LoggingMiddleware)
    .layer(MyCustomMiddleware)
```

Custom middleware implements the `Middleware` trait. This is the right place for:

- request IDs
- logging
- authentication
- shared error translation
- metrics hooks

| Spring | Kernway |
|---|---|
| `HandlerInterceptor` | `impl Middleware for MyMiddleware` |
| servlet filter | `.layer(...)` |
| `@ControllerAdvice` | middleware or explicit helpers |

---

## OpenAPI and SSE

These features are built into the Kernway ecosystem rather than external plugins.

### OpenAPI

```rust
let mut api = OpenApiRegistry::new("Todo API", "1.0.0");
api.add_route(
    RouteDoc::new("Get todo")
        .path_param("id", "Todo ID", "integer")
        .response_json(200, "Todo", "#/components/schemas/Todo"),
    "GET", "/todos/{id}",
);
```

### SSE

```rust
.get("/events", |_req, _ctx| {
    SseStream::new(vec![
        SseEvent::data("connected"),
        SseEvent::named("heartbeat", "{}"),
    ]).into_response()
})
```

Spring equivalents:

| Spring | Kernway |
|---|---|
| SpringDoc / Swagger | `kernway-openapi` |
| `SseEmitter` | `kernway-sse::SseStream` |

---

## What is intentionally different?

### 1. No async runtime

Spring uses the JVM runtime. Axum/Actix use Tokio. Kernway v1.0 uses `std` only.

That means:

- handlers are synchronous
- blocking I/O is expected
- concurrency comes from OS threads
- examples do **not** use `async fn`

### 2. Less magic, more explicit code

You will write:

- route registrations
- JSON parsing with `serde_json`
- simple repository wrappers

The tradeoff is predictable behavior and small binaries.

### 3. Some annotations are markers today

`#[cacheable]`, `#[cache_evict]`, and `#[transactional]` communicate intent now, while full AOP-style codegen remains on the roadmap.

---

## Recommended migration checklist

- [ ] Replace Java DTOs with Rust structs using `serde`
- [ ] Port entities with `#[entity]`
- [ ] Wrap persistence behind `Repository<T>`-style APIs
- [ ] Move business logic into DI-managed services
- [ ] Re-register HTTP endpoints with `KernwayApp::builder()`
- [ ] Convert common errors to `ProblemDetail`
- [ ] Add `RequestIdMiddleware` and `LoggingMiddleware`
- [ ] Add cache-aside for hot reads
- [ ] Generate `/openapi.json`
- [ ] Add an SSE endpoint if the service exposes change events

---

## Feature gaps and current workarounds

| Spring feature | Kernway v1.0 status | Practical workaround |
|---|---|---|
| `@Transactional` | marker only | keep transaction boundaries in repository/service code |
| `@Cacheable` AOP | marker only | manual cache-aside with `InMemoryCache` |
| full Spring Data equivalent | partial | small wrapper around `InMemoryRepository<T>` |
| real DB drivers | roadmap | validate behavior with in-memory repo first |
| servlet filter chain ecosystem | smaller | implement `Middleware` directly |

---

## See also

- [Building a REST API](../guides/rest-api.md)
- [Database Access](../guides/database.md)
- [Annotations Reference](../reference/annotations.md)
- [README flagship example](../../../README.md)
