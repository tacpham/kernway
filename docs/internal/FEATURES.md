# Kernway — Features

## Feature overview

| Feature | v0.3 | v0.4 | v0.5 | v0.6 |
|---|---|---|---|---|
| REST API (GET/POST/PUT/DELETE) | ✅ | ✅ | ✅ | ✅ |
| JSON request/response | ✅ | ✅ | ✅ | ✅ |
| Path params, Query params | ✅ | ✅ | ✅ | ✅ |
| DI (component, inject) | ✅ | ✅ | ✅ | ✅ |
| Config profiles (dev/prod) | ✅ | ✅ | ✅ | ✅ |
| Health check endpoints | ✅ | ✅ | ✅ | ✅ |
| Static file serving | ✅ | ✅ | ✅ | ✅ |
| Graceful shutdown | ✅ | ✅ | ✅ | ✅ |
| Testing (TestApp) | ✅ | ✅ | ✅ | ✅ |
| Validation (#[validated]) | | ✅ | ✅ | ✅ |
| AOP (transactional, require_role) | | ✅ | ✅ | ✅ |
| Observability (tracing, metrics) | | ✅ | ✅ | ✅ |
| CORS, CSRF, HSTS | | ✅ | ✅ | ✅ |
| Rate limiting | | ✅ | ✅ | ✅ |
| Database (diesel + r2d2) | | ✅ | ✅ | ✅ |
| TLS (rustls, HTTP/2) | | | ✅ | ✅ |
| Hot reload (kernway dev) | | | ✅ | ✅ |
| WebSocket | | | | ✅ |
| Template engine (kernleaf) | | | | ✅ |
| OpenAPI / Swagger UI | | | | ✅ |
| Multipart / File upload | | | | ✅ |
| SSE (Server-Sent Events) | | | | ✅ |
| i18n / Localization | | | | ✅ |

---

## Validation

```rust
use kernway::prelude::*;
use kernway::validate::Validate;

#[derive(Deserialize, Validate)]
struct RegisterRequest {
    #[validate(email)]
    email: String,

    #[validate(min_length = 8, max_length = 128)]
    password: String,

    #[validate(min = 13, max = 150)]
    age: u32,
}

#[route(POST, "/users/register")]
#[validated]
async fn register(body: Validated<Json<RegisterRequest>>) -> impl IntoResponse {
    // Chỉ đến đây nếu tất cả fields valid
    // body.0 là RegisterRequest đã validated
    Json(json!({ "status": "ok" }))
}

// Khi validation thất bại — RFC 7807 Problem Details:
// HTTP 400 Bad Request
// {
//   "type": "https://kernway.dev/errors/validation",
//   "title": "Validation Failed",
//   "status": 400,
//   "errors": { "email": "must be a valid email", "age": "must be >= 13" }
// }
```

---

## Observability

```rust
// Tự động với plugin:
KernwayApp::builder()
    .plugin(TracingPlugin::json_logs())   // structured JSON logs
    .plugin(MetricsPlugin::prometheus())  // /metrics endpoint
    .build()
    .run()
    .await

// Mỗi request tự động có:
// - request_id header (UUID v4)
// - duration_ms log
// - status_code log
// - span tracing (tích hợp với Jaeger/Zipkin)
```

**Structured log output:**
```json
{
  "timestamp": "2024-01-15T10:30:00Z",
  "level": "INFO",
  "request_id": "550e8400-e29b-41d4-a716-446655440000",
  "method": "POST",
  "path": "/users",
  "status": 201,
  "duration_ms": 45,
  "message": "request completed"
}
```

**Prometheus metrics:**
```
# HELP http_requests_total Total HTTP requests
http_requests_total{method="POST",path="/users",status="201"} 1523
# HELP http_request_duration_seconds Request duration
http_request_duration_seconds{quantile="0.99"} 0.045
```

---

## Configuration

```rust
// config/app_config.rs
#[configuration]
pub struct AppConfig {
    #[env("PORT", default = "8080")]
    pub port: u16,

    #[env("DATABASE_URL")]
    pub database_url: String,

    #[env("JWT_SECRET")]
    pub jwt_secret: String,
}

// Profile-based: config/application.toml
// config/application-dev.toml
// config/application-prod.toml

// Inject vào bất kỳ component nào:
#[component]
struct UserService {
    #[inject]
    config: Arc<AppConfig>,
}
```

---

## Testing

```rust
// Integration test
#[kernway::test]
async fn test_create_user() {
    let app = TestApp::new(my_app_config()).await;

    let res = app
        .post("/users")
        .json(&json!({ "name": "Alice", "email": "alice@example.com" }))
        .send()
        .await;

    assert_eq!(res.status(), 201);
    let body: Value = res.json().await;
    assert_eq!(body["name"], "Alice");
}

// Mock beans
#[kernway::test]
async fn test_with_mock_db() {
    let app = TestApp::builder()
        .mock::<UserRepository>(MockUserRepository {
            users: vec![User { id: 1, name: "Alice".into() }]
        })
        .build()
        .await;

    let res = app.get("/users/1").send().await;
    assert_eq!(res.status(), 200);
}
```

---

## Security Middleware

```rust
KernwayApp::builder()
    .layer(CorsLayer::new()
        .allow_origins(["https://example.com"])
        .allow_methods([Method::GET, Method::POST])
        .max_age(Duration::from_secs(3600)))
    .layer(CsrfLayer::new())          // CSRF token validation
    .layer(HstsLayer::new(86400))     // Strict-Transport-Security
    .layer(RateLimitLayer::new()
        .per_ip(100, Duration::from_secs(60)))  // 100 req/min per IP
    .build()
    .run()
    .await
```

---

## WebSocket (v0.6)

```rust
#[route(GET, "/ws/chat")]
async fn chat_handler(ws: WebSocket) -> impl IntoResponse {
    ws.on_connect(|mut conn| async move {
        while let Some(msg) = conn.recv().await {
            match msg {
                WsMessage::Text(text) => {
                    conn.send(WsMessage::Text(format!("Echo: {text}"))).await?;
                }
                WsMessage::Close => break,
                _ => {}
            }
        }
        Ok(())
    })
}
```

---

## Template Engine — kernleaf (v0.6)

```html
<!-- templates/users/profile.html -->
<!DOCTYPE html>
<html>
<head>
    <title kw:text="'Profile - ' + user.name">Profile - Name</title>
</head>
<body>
    <!-- kw:authorize kiểm tra role -->
    <div kw:authorize="hasRole('ADMIN')">
        <a href="/admin">Admin Panel</a>
    </div>

    <h1 kw:text="${user.name}">John Doe</h1>
    <p kw:text="${user.email}">john@example.com</p>

    <ul>
        <li kw:each="post : ${user.posts}" kw:text="${post.title}">Post title</li>
    </ul>

    <!-- CSRF token tự động inject vào form POST -->
    <form method="POST" action="/profile/update">
        <input type="text" name="name" kw:value="${user.name}">
        <button type="submit">Update</button>
    </form>
</body>
</html>
```

```rust
// Controller
#[route(GET, "/users/{id}/profile")]
async fn user_profile(
    Path(id): Path<u64>,
    service: Arc<UserService>,
) -> impl IntoResponse {
    let user = service.find_by_id(id).await?;
    Template::new("users/profile", context! { user })
}

// Security: kw:text auto-escapes HTML — XSS không thể xảy ra qua template
// Raw HTML chỉ qua kw:utext (explicit unsafe)
```

---

## OpenAPI / Swagger (v0.6)

```rust
#[route(POST, "/users")]
#[openapi(
    summary = "Create a new user",
    tag = "users",
    response(201, "User created", schema = UserResponse),
    response(400, "Validation error"),
)]
async fn create_user(body: Validated<Json<CreateUserRequest>>) -> impl IntoResponse {
    // ...
}

// Tự động generate:
// GET /openapi.json  → OpenAPI 3.0 spec
// GET /swagger-ui    → Swagger UI
// GET /redoc         → ReDoc UI
```

---

## Hot Reload (v0.5)

```bash
# Terminal 1: start development server
kernway dev

# Terminal 2: edit code và save
# kernway-server detect .so thay đổi → drain in-flight requests → reload

# Output:
# [kernway dev] Watching src/ for changes...
# [kernway dev] Change detected: src/controller/user_controller.rs
# [kernway dev] Building...  (2.3s)
# [kernway dev] Hot-reloaded: 0 in-flight requests drained
# [kernway dev] App ready at http://localhost:8080
```

**State behavior during reload:**
- Dev mode: state resets (acceptable)
- Database state: persists (in the DB, not in process memory)
- Session state: configure external Redis to preserve sessions across reloads

---

## Static Files

```rust
KernwayApp::builder()
    .static_files("/assets", "public/assets/")  // serve từ thư mục
    .static_files("/favicon.ico", "public/favicon.ico")
    // ETag + Cache-Control headers tự động
    // Range requests (RFC 7233) cho large files
    .build()
    .run()
    .await
```

---

## Multipart / File Upload (v0.6)

```rust
#[route(POST, "/upload")]
async fn upload(mut multipart: Multipart) -> impl IntoResponse {
    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or("unknown");
        let filename = field.file_name().unwrap_or("file");
        let data = field.bytes().await?;
        // save data...
    }
    Json(json!({ "status": "uploaded" }))
}
```

---

## SSE — Server-Sent Events (v0.6)

```rust
#[route(GET, "/events")]
async fn sse_handler() -> impl IntoResponse {
    let stream = futures_stream_from_somewhere();
    SseStream::new(stream)
        .keep_alive(Duration::from_secs(30))
}
```
