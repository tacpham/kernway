# Kernway — Annotation Reference

> Complete list of all annotations supported by Kernway, their purpose, and comparisons with Spring / Axum / Actix.

---

## Design Decision: Return type instead of annotation

> **Kernway's strength**: Response behavior is determined by the **return type**, not by annotations.

Spring must distinguish between `@Controller` (template) vs `@RestController` (JSON) vs `@ResponseBody` (per-method):

```java
// Spring — phải chọn đúng annotation, mix thì phức tạp
@Controller                       // template mode
public class UserController {
    @GetMapping("/{id}")
    @ResponseBody                 // ← thêm annotation để trả JSON trong @Controller
    public User getUser() { }

    @GetMapping("/profile")
    public String profile() { }   // trả view name
}
```

Kernway — **the return type decides, with no extra annotation needed**:

```rust
#[controller("/users")]           // một annotation duy nhất, không phân biệt REST hay MVC
struct UserController { }

#[route(GET, "/{id}")]
async fn get_user() -> Json<User> { }      // → JSON  (REST)

#[route(GET, "/profile")]
async fn profile() -> Template { }         // → HTML  (MVC)

#[route(GET, "/export")]
async fn export() -> Csv<Vec<User>> { }    // → CSV   (custom)
// Mix tự do trong cùng controller — không cần thêm annotation nào
```

| | Spring | Kernway |
|---|---|---|
| REST controller | `@RestController` | `#[controller]` + return `Json<T>` |
| Template controller | `@Controller` | `#[controller]` + return `Template` |
| Mix REST + template | Requires `@ResponseBody` per-method | Natural — the return type speaks for itself |
| Custom format (CSV, XML) | `HttpMessageConverter` config | Implement `IntoResponse` + return |
| Enforce JSON-only | `@RestController` | Optional `#[response_format(json)]` |

---

## Comparison overview

| Kernway | Spring | Axum | Actix | Purpose |
|---|---|---|---|---|
| `#[kernway::main]` | `@SpringBootApplication` | manual | manual | Entry point |
| `#[component]` | `@Component` / `@Service` / `@Repository` | ❌ | ❌ | DI bean |
| `#[controller]` | `@RestController` | ❌ | ❌ | HTTP controller |
| `#[configuration]` | `@Configuration` | ❌ | ❌ | Config bean |
| `#[inject]` | `@Autowired` | ❌ | ❌ | Field injection |
| `#[primary]` | `@Primary` | ❌ | ❌ | Resolve DI conflict |
| `#[default_impl]` | `@ConditionalOnMissingBean` | ❌ | ❌ | Overridable default |
| `#[qualifier("name")]` | `@Qualifier` | ❌ | ❌ | Select a specific bean |
| `#[route(METHOD, path)]` | `@GetMapping` / `@PostMapping` / ... | `Router::route` | `web::get()` | Route handler |
| `#[transactional]` | `@Transactional` | ❌ | ❌ | DB transaction |
| `#[require_role("R")]` | `@PreAuthorize("hasRole('R')")` | ❌ | ❌ | Authorization |
| `#[validated]` | `@Validated` / `@Valid` | ❌ | ❌ | Input validation |
| `#[exception_handler]` | `@ExceptionHandler` + `@ControllerAdvice` | ❌ | ❌ | Error handling |
| `#[cached]` | `@Cacheable` | ❌ | ❌ | Method cache |
| `#[retry]` | `@Retryable` | ❌ | ❌ | Auto retry |
| `#[timeout]` | (custom AOP) | ❌ | ❌ | Method timeout |
| `#[rate_limit]` | (no equivalent) | ❌ | ❌ | Rate limiting |
| `#[circuit_breaker(...)]` | Hystrix/Resilience4j | ❌ | ❌ | Auto circuit breaker |
| `#[traced]` | `@NewSpan` (Micrometer Tracing) | ❌ | ❌ | Auto span + timing log |
| `#[timed]` | `@Timed` (Micrometer) | ❌ | ❌ | Metrics timing |
| `#[logged]` | `@Slf4j` (Lombok) | ❌ | ❌ | Inject a logger into a component |
| `#[profile("dev")]` | `@Profile("dev")` | ❌ | ❌ | Conditional on profile |
| `#[env("KEY")]` | `@Value("${key}")` | ❌ | ❌ | Env var injection |
| `#[kernway::test]` | `@SpringBootTest` | ❌ | ❌ | Integration test |
| `#[mock]` | `@MockBean` | ❌ | ❌ | Mock bean in tests |
| `#[openapi(...)]` | Springdoc / Swagger | ❌ | ❌ | API documentation |

---

## Annotation details

### Entry Point

#### `#[kernway::main]`

```rust
#[kernway::main]
async fn main() {
    KernwayApp::builder()
        .bind("0.0.0.0:8080")
        .build()
        .run()
        .await
}
```

- Generates bootstrap code for the thread-per-core runtime
- Equivalent to: `@SpringBootApplication` + `SpringApplication.run()`
- Not available in Axum/Actix — you must set up the runtime yourself

---

### DI Layer

#### `#[component]`

```rust
#[component]
struct UserService {
    #[inject] repo: Arc<UserRepository>,
}
```

- Registers the struct in `AppContext` as a singleton bean
- Spring equivalent: `@Component`, `@Service`, `@Repository` (Kernway does not distinguish them — semantics are enough)
- Default scope: Singleton (`Arc<T>`)

#### `#[controller("base_path")]`

```rust
#[controller("/users")]
struct UserController {
    #[inject] service: Arc<UserService>,
}
```

- Equivalent to `#[component]` + mounting routes under a base path
- Equivalent to: `@RestController` + `@RequestMapping`

#### `#[configuration]`

```rust
#[configuration]
struct DatabaseConfig {
    #[env("DATABASE_URL")]
    url: String,

    #[env("DB_MAX_CONNECTIONS", default = "10")]
    max_connections: u32,
}
```

- Bean containing config values from env vars / config files
- Equivalent to: `@Configuration` + `@ConfigurationProperties`

#### `#[inject]`

```rust
#[component]
struct OrderService {
    #[inject] user_service: Arc<UserService>,
    #[inject] db: Arc<dyn DbPool>,
}
```

- Field-level injection
- Equivalent to: `@Autowired`
- Only injects `Arc<T>` (singleton) or `Box<dyn Trait>`

#### `#[primary]`

```rust
#[component]
#[primary]  // ← thắng khi có conflict
struct JwtAuthExtractor;
impl AuthExtractor for JwtAuthExtractor { ... }
```

- Resolves conflicts when multiple beans implement the same trait
- Equivalent to: `@Primary`
- A conflict without `#[primary]` → compile error (unlike Spring: runtime exception)

#### `#[default_impl]`

```rust
// Chỉ dùng trong kernway framework — không phải user code
#[component]
#[default_impl]
struct DefaultErrorHandler;
impl ErrorHandler for DefaultErrorHandler { ... }
```

- Framework default — ignored if the user defines an impl for the same trait
- Equivalent to: `@ConditionalOnMissingBean`
- See: `docs/ARCHITECTURE.md#override-system`

#### `#[qualifier("name")]`

```rust
#[component]
#[qualifier("postgres")]
struct PostgresPool;
impl DbPool for PostgresPool { ... }

#[component]
#[qualifier("analytics")]
struct ClickhousePool;
impl DbPool for ClickhousePool { ... }

// Inject theo tên
#[component]
struct ReportService {
    #[inject]
    #[qualifier("analytics")]
    pool: Arc<dyn DbPool>,
}
```

- Selects a specific bean when multiple impls exist
- Equivalent to: `@Qualifier("name")`

---

### Routing Layer

#### `#[route(METHOD, "path")]`

```rust
#[route(GET,    "/{id}")]
#[route(POST,   "/")]
#[route(PUT,    "/{id}")]
#[route(DELETE, "/{id}")]
#[route(PATCH,  "/{id}")]
```

- Defines an HTTP route handler
- Equivalent to: `@GetMapping`, `@PostMapping`, `@PutMapping`, `@DeleteMapping`, `@PatchMapping`
- Path param: `{id}` → `Path<u64>`
- Wildcard: `{*rest}` → catch-all

---

### AOP Layer

#### `#[transactional]`

```rust
impl UserService {
    #[transactional]
    async fn create_user(&self, req: CreateUserReq) -> Result<User> {
        // auto commit on Ok, rollback on Err
    }

    // Tùy chỉnh:
    #[transactional(isolation = "READ_COMMITTED", propagation = "REQUIRES_NEW")]
    async fn transfer(&self, from: u64, to: u64, amount: f64) -> Result<()> { ... }
}
```

- Wraps the method in a DB transaction
- `propagation`: `REQUIRED` (default), `REQUIRES_NEW`, `SUPPORTS`, `NOT_SUPPORTED`
- `isolation`: `READ_COMMITTED` (default), `READ_UNCOMMITTED`, `REPEATABLE_READ`, `SERIALIZABLE`
- Equivalent to: `@Transactional`

#### `#[require_role("ROLE")]`

```rust
#[route(DELETE, "/{id}")]
#[require_role("ADMIN")]                   // single role
async fn delete_user(...) { ... }

#[route(GET, "/reports")]
#[require_role("ADMIN", "MANAGER")]        // any of these roles
async fn view_reports(...) { ... }

// Có thể đặt trên controller (apply cho tất cả routes)
#[controller("/admin")]
#[require_role("ADMIN")]
struct AdminController { ... }
```

- Checks the role before entering the handler
- 403 Forbidden if permissions are insufficient, 401 Unauthorized if the user is not logged in
- Equivalent to: `@PreAuthorize("hasRole('ADMIN')")`

#### `#[validated]`

```rust
#[route(POST, "/users")]
#[validated]
async fn create_user(body: Validated<Json<CreateUserReq>>) -> impl IntoResponse {
    // body.0 đã pass validation
}
```

- Validates the request body/params before the handler runs
- Errors → RFC 7807 Problem Details response (400)
- Equivalent to: `@Validated` / `@Valid`

#### `#[exception_handler]` + scope

```rust
// Global — áp dụng toàn app
#[exception_handler]
async fn handle_app_error(err: AppError) -> impl IntoResponse { ... }

// Scoped theo controller — thắng global
#[exception_handler(scope = UserController)]
async fn handle_user_error(err: UserError) -> impl IntoResponse { ... }

// Scoped theo module
#[exception_handler(scope = "controller::api")]
async fn handle_api_error(err: ApiError) -> impl IntoResponse { ... }
```

Priority: controller-scope > module-scope > global

- Equivalent to: `@ExceptionHandler` + `@ControllerAdvice(assignableTypes = ...)`

#### `#[cached]`

```rust
impl UserService {
    #[cached(key = "user:{id}", ttl = 300)]  // 5 phút
    async fn find_by_id(&self, id: u64) -> Result<User> { ... }

    #[cache_evict(key = "user:{id}")]        // xóa cache khi update
    async fn update(&self, id: u64, ...) -> Result<User> { ... }
}
```

- Caches method results
- Equivalent to: `@Cacheable`, `@CacheEvict`
- Backend: in-memory (default) or Redis (override the `CacheBackend` trait)

#### `#[retry]`

```rust
#[retry(max = 3, backoff = "exponential", on = [NetworkError, TimeoutError])]
async fn call_external_api(&self) -> Result<Response> { ... }
```

- Automatically retries on specific errors
- Equivalent to: `@Retryable`

#### `#[timeout]`

```rust
#[timeout(secs = 30)]
async fn slow_operation(&self) -> Result<Data> { ... }
```

- Method-level timeout
- Returns `Error::Timeout` if the deadline is exceeded

#### `#[rate_limit]`

```rust
#[route(POST, "/auth/login")]
#[rate_limit(per_ip = 5, window_secs = 60)]  // 5 lần/phút per IP
async fn login(...) { ... }
```

- Per-route rate limiting (added to the global layer)
- No equivalent in Spring annotations (usually handled with a filter/gateway)

#### `#[traced]`

```rust
#[traced]  // tạo span cho distributed tracing
async fn process_order(&self, order_id: u64) -> Result<Order> { ... }
```

- Automatically creates a tracing span and propagates trace context
- Equivalent to: Spring AOP + `@NewSpan` (Sleuth/Micrometer Tracing)

#### `#[timed]`

```rust
#[timed(metric = "user_service.find_by_id")]
async fn find_by_id(&self, id: u64) -> Result<User> { ... }
// → Prometheus metric: method_duration_seconds{method="user_service.find_by_id"}
```

- Measures method duration and exports a metric
- Equivalent to: `@Timed` (Micrometer)

---

### Config Layer

#### `#[env("KEY")]`

```rust
#[configuration]
struct AppConfig {
    #[env("PORT", default = "8080")]
    port: u16,

    #[env("DATABASE_URL")]          // required — panic nếu không có
    database_url: String,

    #[env("JWT_SECRET")]
    jwt_secret: String,

    #[env("RUST_ENV", default = "development")]
    environment: String,
}
```

- Injects an env var into a field
- Equivalent to: `@Value("${key:default}")`

#### `#[profile("name")]`

```rust
#[component]
#[profile("dev")]               // chỉ active trong profile dev
struct DevMailSender;
impl MailSender for DevMailSender {
    async fn send(&self, mail: Mail) {
        println!("[DEV] Would send: {:?}", mail);  // log, không gửi thật
    }
}

#[component]
#[profile("prod")]
struct SmtpMailSender;
impl MailSender for SmtpMailSender { /* gửi thật */ }
```

- The bean is only registered for a specific profile
- Equivalent to: `@Profile("dev")`

---

### Validation Constraints

Used on struct fields together with `#[validated]`:

```rust
#[derive(Deserialize, Validate)]
struct RegisterRequest {
    #[validate(email)]
    email: String,

    #[validate(min_length = 8, max_length = 128)]
    password: String,

    #[validate(min = 13, max = 150)]
    age: u32,

    #[validate(not_blank)]
    name: String,

    #[validate(pattern = r"^\+?[1-9]\d{1,14}$")]  // E.164 phone
    phone: Option<String>,

    #[validate(url)]
    website: Option<String>,
}
```

| Constraint | Spring equivalent | Description |
|---|---|---|
| `email` | `@Email` | Valid email format |
| `min_length` / `max_length` | `@Size` | String length |
| `min` / `max` | `@Min` / `@Max` | Numeric range |
| `not_blank` | `@NotBlank` | Non-empty, non-whitespace |
| `not_null` | `@NotNull` | Non-null (Rust: non-None) |
| `pattern` | `@Pattern` | Regex match |
| `url` | `@URL` | Valid URL |
| `positive` | `@Positive` | > 0 |
| `range(min, max)` | `@Range` | Numeric range |

---

### Testing Layer

#### `#[kernway::test]`

```rust
#[kernway::test]
async fn test_create_user() {
    let app = TestApp::new(app_config()).await;
    let res = app.post("/users").json(&body).send().await;
    assert_eq!(res.status(), 201);
}
```

- Starts the full app in a test
- Equivalent to: `@SpringBootTest`

#### `#[mock]` (field in the TestApp builder)

```rust
#[kernway::test]
async fn test_with_mock() {
    let app = TestApp::builder()
        .mock::<UserRepository>(MockUserRepository::new())
        .build()
        .await;
}
```

- Replaces the real bean with a mock
- Equivalent to: `@MockBean`

---

### OpenAPI

#### `#[openapi(...)]`

```rust
#[route(POST, "/users")]
#[openapi(
    summary = "Create user",
    description = "Register a new user account",
    tag = "users",
    response(201, "Created", schema = UserResponse),
    response(400, "Validation error"),
    response(409, "Email already exists"),
)]
#[validated]
async fn create_user(body: Validated<Json<CreateUserReq>>) -> impl IntoResponse { ... }
```

- Generates an OpenAPI 3.1 spec
- Equivalent to: Springdoc `@Operation`, `@ApiResponse`
- Auto-generates `/openapi.json` and `/swagger-ui`

---

### Custom Annotations (v0.4+)

#### `kernway::define_annotation!`

```rust
// Định nghĩa annotation gộp nhiều annotation
kernway::define_annotation! {
    /// Controller cho API v1: authenticated, validated, returns JSON
    pub annotation ApiV1Controller {
        #[controller]           // nhận path argument
        #[require_role("USER")]
        #[validated]
        #[traced]
    }
}

// Dùng như annotation thường
#[ApiV1Controller("/users")]
struct UserController {
    #[inject] service: Arc<UserService>,
}

// Expand thành:
// #[controller("/users")]
// #[require_role("USER")]
// #[validated]
// #[traced]
// struct UserController { ... }
```

- Equivalent to: custom annotations in Spring (`@interface`)
- Release: v0.4+

---

## Annotations by layer (quick reference)

```
App bootstrap:     #[kernway::main]
                   #[configuration]  #[env]  #[profile]

DI:                #[component]  #[controller]  #[inject]
                   #[primary]    #[qualifier]   #[default_impl]

Routing:           #[route(METHOD, path)]

Security/AOP:      #[require_role]  #[validated]  #[transactional]
                   #[cached]        #[retry]       #[timeout]
                   #[rate_limit]    #[traced]      #[timed]

Error handling:    #[exception_handler]  #[exception_handler(scope = X)]

Validation fields: #[validate(email|min_length|max_length|min|max|...)]

Testing:           #[kernway::test]  #[mock]

API docs:          #[openapi(...)]

Custom:            kernway::define_annotation! { ... }
```
