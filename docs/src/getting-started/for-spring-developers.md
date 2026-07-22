# For Spring Developers

> You already know Spring Boot. This page helps you get a Kernway app running in the shortest time possible.

## Core mindset — How is it different from Spring?

| | Spring Boot | Kernway |
|---|---|---|
| Language | Java/Kotlin | Rust |
| DI | Runtime reflection | Compile-time macro |
| Deployment | JAR + JVM | Single binary ~3MB |
| Cold start | 3–10s | ~20ms |
| Annotation | `@Component` | `#[component]` |
| Async | Project Loom / CompletableFuture | Native `async/await` |
| Exceptions | `throw` / `try-catch` | `Result<T, E>` / `?` |

---

## Annotation Mapping — Read this first

| Spring | Kernway | Notes |
|---|---|---|
| `@SpringBootApplication` | `#[kernway::main]` | Entry point |
| `@RestController` + `@RequestMapping("/path")` | `#[controller("/path")]` | Combined into one |
| `@GetMapping("/{id}")` | `#[route(GET, "/{id}")]` | All methods use the same `#[route]` |
| `@PostMapping` | `#[route(POST, "/")]` | |
| `@PutMapping` | `#[route(PUT, "/{id}")]` | |
| `@DeleteMapping` | `#[route(DELETE, "/{id}")]` | |
| `@Service` | `#[component]` | Kernway does not distinguish between Service and Repository |
| `@Repository` | `#[component]` | |
| `@Component` | `#[component]` | |
| `@Autowired` | `#[inject]` | Field injection |
| `@Primary` | `#[primary]` | |
| `@Qualifier("name")` | `#[qualifier("name")]` | |
| `@Transactional` | `#[transactional]` | |
| `@PreAuthorize("hasRole('X')")` | `#[require_role("X")]` | |
| `@Valid` / `@Validated` | `#[validated]` | |
| `@ExceptionHandler` + `@ControllerAdvice` | `#[exception_handler]` | |
| `@Cacheable` | `#[cached]` | |
| `@Value("${key}")` | `#[env("KEY")]` | |
| `@Configuration` | `#[configuration]` | |
| `@Profile("dev")` | `#[profile("dev")]` | |
| `@SpringBootTest` | `#[kernway::test]` | |
| `@MockBean` | `#[mock]` | |
| `@Slf4j` (Lombok) | `#[logged]` | |

---

## Why are `@RestController` + `@Controller` merged into `#[controller]`?

Spring needs to distinguish between:
- `@Controller` → returns a view name (template)
- `@RestController` → returns JSON/data

Kernway **does not need this** — the return type decides automatically:

```rust
#[controller("/users")]
struct UserController { }

// Trả JSON → REST behavior (như @RestController)
#[route(GET, "/{id}")]
async fn get_user(...) -> Json<User> { ... }

// Trả template → MVC behavior (như @Controller)
#[route(GET, "/{id}/profile")]
async fn profile(...) -> Template { ... }

// Mix tự do trong cùng controller — không cần thêm annotation
```

---

## Code comparison — Full CRUD

**Spring Boot:**

```java
@RestController
@RequestMapping("/users")
@RequiredArgsConstructor
public class UserController {
    private final UserService userService;

    @GetMapping("/{id}")
    public ResponseEntity<UserResponse> getUser(@PathVariable Long id) {
        return ResponseEntity.ok(userService.findById(id));
    }

    @PostMapping
    @ResponseStatus(HttpStatus.CREATED)
    public UserResponse createUser(@RequestBody @Valid CreateUserRequest req) {
        return userService.create(req);
    }
}
```

**Kernway:**

```rust
#[controller("/users")]
struct UserController {
    #[inject] service: Arc<UserService>,
}

#[route(GET, "/{id}")]
async fn get_user(ctrl: &UserController, id: Path<u64>) -> Json<User> {
    ctrl.service.find(*id).await.into()
}

#[route(POST, "/")]
#[validated]
async fn create_user(ctrl: &UserController, body: Validated<Json<CreateUserReq>>) -> (StatusCode, Json<User>) {
    let user = ctrl.service.create(body.into_inner()).await.unwrap();
    (StatusCode::CREATED, Json(user))
}
```

---

## Exception Handling — Key difference

**Spring:** `throw exception` → caught by `@ExceptionHandler`

**Kernway:** `Result<T, E>` + `?` operator → caught by `#[exception_handler]`

```rust
// Không có throw — dùng Result
async fn find_user(id: u64) -> Result<User, AppError> {
    let user = repo.find(id).await?;  // ? = throw trong Spring
    Ok(user)
}

// Không có try-catch — dùng #[exception_handler]
#[exception_handler]
async fn handle_error(err: AppError) -> impl IntoResponse {
    match err {
        AppError::NotFound(id) => (StatusCode::NOT_FOUND, Json(json!({ "error": format!("user {id} not found") }))),
        AppError::Internal(e)  => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "internal error" }))),
    }
}
```

---

## Project structure — Similar to Spring

```
my-app/                          Spring equivalent
└── src/
    ├── main.rs                  Application.java
    ├── controller/              @RestController classes
    │   └── user_controller.rs
    ├── service/                 @Service classes
    │   └── user_service.rs
    ├── repository/              @Repository classes
    │   └── user_repository.rs
    ├── model/                   Entity + DTO classes
    │   ├── user.rs
    │   └── dto/
    │       ├── create_user_req.rs
    │       └── user_response.rs
    └── exception/               @ExceptionHandler + custom exceptions
        ├── mod.rs               AppError enum
        └── handlers.rs          #[exception_handler] functions
```

---

## application.toml — Similar to application.yml

**Spring `application.yml`:**

```yaml
server:
  port: 8080

spring:
  datasource:
    url: jdbc:postgresql://localhost/mydb
  jpa:
    hibernate:
      ddl-auto: validate

logging:
  level:
    root: INFO
    com.example: DEBUG
```

**Kernway `config/application.toml`:**

```toml
[server]
port = 8080

[db]
url = "postgresql://localhost/mydb"

[log]
level = "INFO"

[log.modules]
"my_app" = "DEBUG"
```

---

## Dependency Injection — Similar, but compile-time

```rust
// Spring: DI tại runtime, lỗi khi start app
// Kernway: DI tại compile-time, lỗi khi build

#[component]
struct OrderService {
    #[inject] user_service: Arc<UserService>,     // phải có #[component]
    #[inject] payment_service: Arc<PaymentService>, // phải có #[component]
}

// Nếu UserService chưa có #[component] → compile error:
// error[kernway]: bean `UserService` not found
// help: add #[component] to struct UserService
```

---

## Concepts not present in Kernway

| Spring | Kernway replacement |
|---|---|
| `@Bean` in `@Configuration` | `#[component]` directly on the struct |
| `@Scope("prototype")` | `fn() -> T` factory function |
| `@Lazy` | Not supported yet (v1.0+) |
| `ApplicationEvent` / `@EventListener` | Not supported yet (v1.0+) |
| `@Scheduled` | Not supported yet (v1.0+) |
| Spring Security filter chain | `Layer` trait + middleware builder |
| `MockMvc` | `TestApp` + `.get()` `.post()` |

---

## Next steps

- [Your First App](first-app.md) — run your first app
- [Building a REST API](../guides/rest-api.md) — CRUD with a database
- [Spring Boot → Kernway Migration](../migration/spring-to-kernway.md) — migrate an existing project
- [Annotation Reference](../reference/annotations.md) — complete reference
