# Kernway — Error Handling Guide

> A complete guide to handling errors in a Kernway app: defining errors, separating layers, handlers, and response formats.

---

## Step 1 — Define Errors for Each Layer

Each layer should have its own error type. Use the `thiserror::Error` derive macro.

```rust
// src/exception/mod.rs

use thiserror::Error;

// --- Repository layer ---
#[derive(Debug, Error)]
pub enum DbError {
    #[error("record not found")]
    NotFound,
    #[error("duplicate key: {0}")]
    DuplicateKey(String),
    #[error("connection failed: {0}")]
    ConnectionFailed(String),
}

// --- Domain/Service layer ---
#[derive(Debug, Error)]
pub enum UserError {
    #[error("user not found: id={0}")]
    NotFound(u64),
    #[error("email already registered: {0}")]
    DuplicateEmail(String),
    #[error("invalid credentials")]
    InvalidCredentials,
}

#[derive(Debug, Error)]
pub enum PaymentError {
    #[error("insufficient funds")]
    InsufficientFunds,
    #[error("payment gateway timeout")]
    GatewayTimeout,
    #[error("card declined")]
    CardDeclined,
}

// --- App-level error: gom tất cả lại (giống base class trong Java) ---
#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    User(#[from] UserError),        // UserError → AppError tự động qua ?

    #[error(transparent)]
    Payment(#[from] PaymentError),  // PaymentError → AppError tự động qua ?

    #[error(transparent)]
    Db(#[from] DbError),            // DbError → AppError tự động qua ?

    #[error("forbidden")]
    Forbidden,

    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}
```

---

## Step 2 — Use `?` for Automatic Conversion Between Layers

`#[from]` automatically generates the `From` trait → use `?` without needing `.map_err()`: 

```rust
// src/service/user_service.rs
impl UserService {
    pub async fn create(&self, req: CreateUserReq) -> Result<User, AppError> {
        // DbError tự động → AppError::Db qua ?
        if self.repo.email_exists(&req.email).await? {
            return Err(UserError::DuplicateEmail(req.email).into());
        }

        // DbError tự động → AppError::Db qua ?
        let user = self.repo.insert(req).await?;
        Ok(user)
    }
}

// src/repository/user_repository.rs
impl UserRepository {
    pub async fn email_exists(&self, email: &str) -> Result<bool, DbError> {
        spawn_blocking(move || {
            // diesel query...
        }).await
    }
}
```

---

## Step 3 — Define Exception Handlers

### Global handler — catches everything

```rust
// src/exception/handlers.rs

// Bắt AppError — handler mặc định cho toàn app
#[exception_handler]
async fn handle_app_error(err: AppError) -> impl IntoResponse {
    match err {
        AppError::User(e)    => user_error_response(e),
        AppError::Payment(e) => payment_error_response(e),
        AppError::Db(e)      => db_error_response(e),
        AppError::Forbidden  => (StatusCode::FORBIDDEN, Json(error_body("FORBIDDEN", "access denied"))),
        AppError::Internal(e) => {
            log::error!("internal error: {e:?}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(error_body("INTERNAL", "internal server error")))
        }
    }
}
```

### Specific handlers — higher priority than global

```rust
// Handler cho UserError — thắng AppError handler khi lỗi là UserError
#[exception_handler]
async fn handle_user_error(err: UserError) -> impl IntoResponse {
    match err {
        UserError::NotFound(id) => (
            StatusCode::NOT_FOUND,
            Json(error_body("USER_001", format!("user {id} not found")))
        ),
        UserError::DuplicateEmail(mail) => (
            StatusCode::CONFLICT,
            Json(error_body("USER_002", format!("{mail} is already registered")))
        ),
        UserError::InvalidCredentials => (
            StatusCode::UNAUTHORIZED,
            Json(error_body("USER_003", "invalid email or password"))
        ),
    }
}

// Handler scoped — chỉ áp dụng trong PaymentController
#[exception_handler(scope = PaymentController)]
async fn handle_payment_error(err: PaymentError) -> impl IntoResponse {
    match err {
        PaymentError::InsufficientFunds => (
            StatusCode::PAYMENT_REQUIRED,
            Json(error_body("PAY_001", "insufficient funds"))
        ),
        PaymentError::GatewayTimeout => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_body("PAY_002", "payment service temporarily unavailable"))
        ),
        PaymentError::CardDeclined => (
            StatusCode::PAYMENT_REQUIRED,
            Json(error_body("PAY_003", "card was declined"))
        ),
    }
}
```

### Final catch-all — never expose the stack trace

```rust
// Bắt bất kỳ error nào không được handle ở trên
#[exception_handler]
async fn handle_unknown(err: Box<dyn std::error::Error>) -> impl IntoResponse {
    log::error!("unhandled error type: {err:?}");
    (StatusCode::INTERNAL_SERVER_ERROR, Json(error_body("UNKNOWN", "something went wrong")))
}
```

---

## Handler Priority

```
Cao nhất ──────────────────────────────────────────────────────── Thấp nhất

scope=Controller  >  scope="module"  >  specific type  >  parent type  >  catch-all

Ví dụ khi PaymentError xảy ra trong PaymentController:

  1. #[exception_handler(scope = PaymentController)] cho PaymentError  ← CHẠY CÁI NÀY
  2. #[exception_handler] cho PaymentError
  3. #[exception_handler] cho AppError (vì PaymentError → AppError qua From)
  4. #[exception_handler] cho Box<dyn Error>
```

---

## Error Response Format — RFC 7807

Kernway returns errors using the RFC 7807 Problem Details standard:

```rust
// src/exception/mod.rs — helper function dùng chung
pub fn error_body(code: &str, message: impl Into<String>) -> serde_json::Value {
    json!({
        "type":    format!("https://my-app.com/errors/{}", code.to_lowercase()),
        "code":    code,
        "message": message.into(),
    })
}

pub fn validation_body(errors: ValidationErrors) -> serde_json::Value {
    json!({
        "type":    "https://my-app.com/errors/validation",
        "code":    "VALIDATION_FAILED",
        "message": "request validation failed",
        "errors":  errors.field_errors(),
    })
}
```

Sample response:

```json
// 404 Not Found
{
  "type":    "https://my-app.com/errors/user_001",
  "code":    "USER_001",
  "message": "user 42 not found"
}

// 400 Validation Failed
{
  "type":    "https://my-app.com/errors/validation",
  "code":    "VALIDATION_FAILED",
  "message": "request validation failed",
  "errors": {
    "email":    "must be a valid email address",
    "password": "must be at least 8 characters"
  }
}

// 500 Internal (không lộ detail)
{
  "type":    "https://my-app.com/errors/internal",
  "code":    "INTERNAL",
  "message": "internal server error"
}
```

---

## File Structure in the Project

```
src/
└── exception/
    ├── mod.rs          // AppError enum, error_body(), validation_body()
    ├── user_error.rs   // UserError enum
    ├── payment_error.rs // PaymentError enum
    ├── db_error.rs     // DbError enum
    └── handlers.rs     // tất cả #[exception_handler]
```

---

## Validation Errors — Automatic

`#[validated]` + `Validated<Json<T>>` automatically returns RFC 7807 when validation fails — no separate handler is needed:

```rust
#[derive(Deserialize, Validate)]
pub struct CreateUserReq {
    #[validate(email)]
    pub email: String,

    #[validate(min_length = 8)]
    pub password: String,
}

#[route(POST, "/users")]
#[validated]  // ← tự động bắt validation error, trả 422 + RFC 7807
async fn create_user(
    body: Validated<Json<CreateUserReq>>,
    ctrl: &UserController,
) -> Result<Json<User>, AppError> {
    ctrl.service.create(body.into_inner()).await
}
```

---

## Automatic Error Logging

Kernway automatically logs every error that is not a 4xx:

```
ERROR [kernway] unhandled AppError::Db(ConnectionFailed("timeout"))
      request_id=550e8400  method=POST  path=/users  duration_ms=5023
```

Custom log level per error type:

```rust
#[exception_handler]
#[log_level(error)]    // log ở ERROR level (default cho 5xx)
async fn handle_db(err: DbError) -> impl IntoResponse { ... }

#[exception_handler]
#[log_level(warn)]     // log ở WARN (hợp lý hơn cho 4xx)
async fn handle_user(err: UserError) -> impl IntoResponse { ... }

#[exception_handler]
#[log_level(none)]     // không log (ví dụ: validation error — quá nhiều, không cần)
async fn handle_validation(err: ValidationError) -> impl IntoResponse { ... }
```

---

## Quick Summary

```rust
// 1. Định nghĩa errors trong src/exception/
#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)] User(#[from] UserError),
    #[error(transparent)] Db(#[from] DbError),
}

// 2. Dùng ? trong service/repository — tự convert
let user = repo.find(id).await?;   // DbError → AppError tự động

// 3. Handler bắt và trả HTTP response
#[exception_handler]
async fn handle(err: AppError) -> impl IntoResponse { ... }

// 4. Không cần try/catch — Rust Result + ? là đủ
// 5. Compiler báo lỗi nếu quên handle 1 variant — không bao giờ miss error
```
