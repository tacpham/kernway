# aop-layer — AOP & Security Middleware

## Purpose

Aspect-Oriented Programming via `Layer` trait: transactional, security, validation, rate limiting.

## Standards

- **OWASP Top 10** — security baseline (A01 Access Control, A07 Auth Failures)
- **RFC 7807** — Problem Details (error response format)
- JSR-330 `@Transactional`, `@PreAuthorize` patterns — design inspiration

## Macros

### `#[transactional]`

```rust
#[component]
struct UserService {
    #[inject] db: Arc<DbPool>,
}

impl UserService {
    #[transactional]
    async fn create_user(&self, req: CreateUserRequest) -> Result<User, Error> {
        // Begin transaction — auto-commit on Ok, rollback on Err
        let user = self.db.insert_user(req).await?;
        self.db.send_welcome_email(user.id).await?;
        Ok(user)  // ← commit here
        // Err(e) ← rollback here
    }
}
```

### `#[require_role]`

```rust
// OWASP A01: Broken Access Control — checked before handler executes
#[route(DELETE, "/users/{id}")]
#[require_role("ADMIN")]
async fn delete_user(Path(id): Path<u64>) -> impl IntoResponse {
    // Only ADMIN can reach here
}

// With multiple roles (any match):
#[require_role("ADMIN", "MODERATOR")]
```

### `#[validated]`

```rust
// Validation happens before handler body — fail fast
#[route(POST, "/users")]
#[validated]
async fn create_user(body: Validated<Json<CreateUserRequest>>) -> impl IntoResponse {
    // body.0 guaranteed valid
}
```

### `#[exception_handler]`

```rust
// Global error handler
#[exception_handler]
async fn handle_validation_error(err: ValidationError) -> impl IntoResponse {
    // RFC 7807 Problem Details response
    ProblemDetail {
        r#type: "https://kernway.dev/errors/validation",
        title: "Validation Failed",
        status: 400,
        errors: err.field_errors(),
    }
}

#[exception_handler]
async fn handle_auth_error(_err: AuthError) -> impl IntoResponse {
    StatusCode::UNAUTHORIZED
}
```

## Built-in Layers

```rust
// CORS — Fetch Living Standard
CorsLayer::new()
    .allow_origins(["https://example.com"])
    .allow_methods([Method::GET, Method::POST, Method::PUT, Method::DELETE])
    .allow_headers(["Content-Type", "Authorization"])
    .max_age(Duration::from_secs(3600))

// CSRF — OWASP CSRF Cheat Sheet
CsrfLayer::new()
    .cookie_name("_csrf")
    .header_name("X-CSRF-Token")

// HSTS — RFC 6797
HstsLayer::new(86400)  // max-age=86400

// Rate Limiting — OWASP A07
RateLimitLayer::new()
    .per_ip(100, Duration::from_secs(60))         // 100 req/min per IP
    .per_user(1000, Duration::from_secs(60))       // 1000 req/min per user
    .on_exceeded(StatusCode::TOO_MANY_REQUESTS)    // 429

// Request timeout
TimeoutLayer::new(Duration::from_secs(30))

// Request size limit
RequestSizeLayer::new(10 * 1024 * 1024)  // 10MB
```

## Layer Execution Order

```
Request →
  [RateLimitLayer]
  [TimeoutLayer]
  [CorsLayer]
  [CsrfLayer]
  [AuthLayer]          ← sets user principal in request extensions
  [#[require_role]]    ← reads principal from extensions
  [#[validated]]       ← validates request body
  [#[transactional]]   ← wraps handler in DB transaction
  [Handler]
← Response
```
