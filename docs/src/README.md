# Kernway

> **Rust Web Framework — Spring-inspired.**  
> Build Rust web apps in the Spring Boot style. Compile. Run.

---

## What is Kernway?

Kernway is a web framework for Rust, designed so that **Spring/Java developers** can move to Rust without relearning everything from scratch.

```rust
use kernway::prelude::*;

#[component]
struct UserService { /* ... */ }

#[controller("/users")]
struct UserController {
    #[inject] service: Arc<UserService>,
}

#[route(GET, "/{id}")]
async fn get_user(ctrl: &UserController, id: Path<u64>) -> Json<User> {
    ctrl.service.find(*id).await.into()
}

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

## Why Kernway?

| | Kernway | Axum | Actix-web | Spring Boot |
|---|---|---|---|---|
| DI compile-time | ✅ | ❌ | ❌ | ⚠️ Runtime |
| AOP (`#[transactional]`) | ✅ | ❌ | ❌ | ✅ |
| Circuit breaker built-in | ✅ | ❌ | ❌ | ❌ external |
| Binary size | ~3MB | ~8MB | ~10MB | 200MB+ JVM |
| Cold start | ~20ms | ~30ms | ~25ms | 3-10s |
| Spring DX | ✅ | ❌ | ❌ | ✅ |

---

## Getting Started

- **New to Kernway?** → [Installation](getting-started/installation.md) then [Your First App](getting-started/first-app.md)
- **Coming from Spring?** → [For Spring Developers](getting-started/for-spring-developers.md)
- **Looking for a specific feature?** → [Guides](guides/rest-api.md)
- **Need an annotation reference?** → [Annotation Reference](reference/annotations.md)
