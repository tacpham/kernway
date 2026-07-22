# Your First App

Create a simple REST API with one endpoint in **5 minutes**.

## 1. Create a project

```bash
kernway new hello-kernway
cd hello-kernway
```

Generated structure:

```
hello-kernway/
├── Cargo.toml
├── config/
│   └── application.toml
└── src/
    ├── main.rs
    ├── lib.rs
    ├── controller/
    ├── service/
    ├── repository/
    ├── model/
    └── exception/
```

## 2. Review the sample code

`src/main.rs`:

```rust
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

## 3. Add your first endpoint

Create `src/controller/hello_controller.rs`:

```rust
use kernway::prelude::*;

#[controller("/hello")]
pub struct HelloController;

#[route(GET, "/")]
async fn say_hello() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "message": "Hello from Kernway!" }))
}

#[route(GET, "/{name}")]
async fn say_hello_to(name: Path<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "message": format!("Hello, {}!", *name) }))
}
```

Register it in `src/lib.rs`:

```rust
pub mod controller;
```

## 4. Run

```bash
kernway dev
```

```
[kernway] Starting development server...
[kernway] App ready at http://localhost:8080
```

## 5. Test

```bash
curl http://localhost:8080/hello
# {"message":"Hello from Kernway!"}

curl http://localhost:8080/hello/World
# {"message":"Hello, World!"}

curl http://localhost:8080/health
# {"status":"UP"}
```

## 6. Change the code — Hot reload

Edit the message in `hello_controller.rs`, then save the file:

```
[kernway] Change detected: src/controller/hello_controller.rs
[kernway] Building... (2.1s)
[kernway] Hot-reloaded ✓
```

Refresh — you will see the change immediately, with no restart required.

## Next

- [Project Structure](project-structure.md) — understand the directory layout
- [Building a REST API](../guides/rest-api.md) — full CRUD with a DB
- [For Spring Developers](for-spring-developers.md) — if you are coming from Spring
