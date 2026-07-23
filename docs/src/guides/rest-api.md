# Building a REST API

> Build a complete REST API with CRUD, validation, and error handling.

## Before you begin

You need a Kernway project. If you do not have one yet, see: [Your First App](../getting-started/first-app.md).

---

## 1. Define the model

```rust
// src/model/user.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct User {
    pub id:    u64,
    pub name:  String,
    pub email: String,
}

#[derive(Debug, Deserialize, Validate)]
pub struct CreateUserReq {
    #[validate(not_blank)]
    pub name: String,

    #[validate(email)]
    pub email: String,

    #[validate(min_length = 8)]
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateUserReq {
    pub name: Option<String>,
}
```

---

## 2. Service

```rust
// src/service/user_service.rs
use kernway::prelude::*;
use crate::model::user::{User, CreateUserReq, UpdateUserReq};
use crate::exception::{AppError, UserError};

#[component]
pub struct UserService {
    // Once a DB is wired up: #[inject] repo: Arc<UserRepository>
    store: std::sync::Mutex<Vec<User>>,
}

impl UserService {
    pub fn new() -> Self {
        Self { store: std::sync::Mutex::new(vec![]) }
    }

    pub async fn find_all(&self) -> Vec<User> {
        self.store.lock().unwrap().clone()
    }

    pub async fn find_by_id(&self, id: u64) -> Result<User, AppError> {
        self.store.lock().unwrap()
            .iter()
            .find(|u| u.id == id)
            .cloned()
            .ok_or(UserError::NotFound(id).into())
    }

    pub async fn create(&self, req: CreateUserReq) -> Result<User, AppError> {
        let mut store = self.store.lock().unwrap();
        if store.iter().any(|u| u.email == req.email) {
            return Err(UserError::DuplicateEmail(req.email).into());
        }
        let user = User { id: store.len() as u64 + 1, name: req.name, email: req.email };
        store.push(user.clone());
        Ok(user)
    }

    pub async fn update(&self, id: u64, req: UpdateUserReq) -> Result<User, AppError> {
        let mut store = self.store.lock().unwrap();
        let user = store.iter_mut().find(|u| u.id == id)
            .ok_or(UserError::NotFound(id))?;
        if let Some(name) = req.name { user.name = name; }
        Ok(user.clone())
    }

    pub async fn delete(&self, id: u64) -> Result<(), AppError> {
        let mut store = self.store.lock().unwrap();
        let pos = store.iter().position(|u| u.id == id)
            .ok_or(UserError::NotFound(id))?;
        store.remove(pos);
        Ok(())
    }
}
```

---

## 3. Controller

```rust
// src/controller/user_controller.rs
use kernway::prelude::*;
use crate::model::user::{User, CreateUserReq, UpdateUserReq};
use crate::service::user_service::UserService;
use crate::exception::AppError;

#[controller("/users")]
pub struct UserController {
    #[inject] service: Arc<UserService>,
}

// GET /users — list
#[route(GET, "/")]
async fn list_users(ctrl: &UserController) -> Json<Vec<User>> {
    Json(ctrl.service.find_all().await)
}

// GET /users/{id} — detail
#[route(GET, "/{id}")]
async fn get_user(ctrl: &UserController, id: Path<u64>) -> Result<Json<User>, AppError> {
    Ok(Json(ctrl.service.find_by_id(*id).await?))
}

// POST /users — create
#[route(POST, "/")]
#[validated]
async fn create_user(
    ctrl: &UserController,
    body: Validated<Json<CreateUserReq>>,
) -> Result<(StatusCode, Json<User>), AppError> {
    let user = ctrl.service.create(body.into_inner()).await?;
    Ok((StatusCode::CREATED, Json(user)))
}

// PUT /users/{id} — update
#[route(PUT, "/{id}")]
async fn update_user(
    ctrl: &UserController,
    id: Path<u64>,
    body: Json<UpdateUserReq>,
) -> Result<Json<User>, AppError> {
    Ok(Json(ctrl.service.update(*id, body.into_inner()).await?))
}

// DELETE /users/{id} — delete
#[route(DELETE, "/{id}")]
async fn delete_user(ctrl: &UserController, id: Path<u64>) -> Result<StatusCode, AppError> {
    ctrl.service.delete(*id).await?;
    Ok(StatusCode::NO_CONTENT)
}
```

---

## 4. Error Handling

```rust
// src/exception/mod.rs
use thiserror::Error;

#[derive(Debug, Error)]
pub enum UserError {
    #[error("user not found: id={0}")]
    NotFound(u64),
    #[error("email already registered: {0}")]
    DuplicateEmail(String),
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error(transparent)]
    User(#[from] UserError),
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

// src/exception/handlers.rs
#[exception_handler]
async fn handle_user_error(err: UserError) -> impl IntoResponse {
    match err {
        UserError::NotFound(id)         => (StatusCode::NOT_FOUND, Json(json!({ "error": format!("user {id} not found") }))),
        UserError::DuplicateEmail(mail) => (StatusCode::CONFLICT,  Json(json!({ "error": format!("{mail} already registered") }))),
    }
}

#[exception_handler]
async fn handle_internal(err: AppError) -> impl IntoResponse {
    log::error!("internal: {err:?}");
    StatusCode::INTERNAL_SERVER_ERROR
}
```

---

## 5. Register in main

```rust
// src/main.rs
use kernway::prelude::*;

#[kernway::main]
async fn main() {
    KernwayApp::builder()
        .bind("0.0.0.0:8080")
        .build()
        .run()
        .await
}
```

---

## 6. Try it out

```bash
kernway dev
```

```bash
# Create a user
curl -X POST http://localhost:8080/users \
  -H "Content-Type: application/json" \
  -d '{"name":"Alice","email":"alice@example.com","password":"secret123"}'
# {"id":1,"name":"Alice","email":"alice@example.com"}

# List
curl http://localhost:8080/users
# [{"id":1,"name":"Alice","email":"alice@example.com"}]

# Detail
curl http://localhost:8080/users/1
# {"id":1,"name":"Alice","email":"alice@example.com"}

# Validation error
curl -X POST http://localhost:8080/users \
  -H "Content-Type: application/json" \
  -d '{"name":"","email":"not-an-email","password":"short"}'
# {"code":"VALIDATION_FAILED","errors":{"email":"must be valid email","password":"min 8 chars"}}

# Not found
curl http://localhost:8080/users/999
# {"error":"user 999 not found"}

# Delete
curl -X DELETE http://localhost:8080/users/1
# 204 No Content
```

---

## Next steps

- [Database Access](database.md) — persist data to PostgreSQL instead of using in-memory storage
- [Validation](validation.md) — custom validators and nested object validation
- [Error Handling](error-handling.md) — a complete error hierarchy
- [Testing](testing.md) — write integration tests for this API
