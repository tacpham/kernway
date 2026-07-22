# Database Access

> Kernway ORM follows a **spec-first** approach — similar to JPA. Write code against the `kernway-orm-core` spec, then swap the database implementation by changing a single line in `Cargo.toml`.

## Choose an implementation

```toml
# Cargo.toml

# PostgreSQL (official)
kernway = { version = "0.4", features = ["orm-diesel", "db-postgres"] }

# MySQL (official)
kernway = { version = "0.4", features = ["orm-diesel", "db-mysql"] }

# SQLite (official)
kernway = { version = "0.4", features = ["orm-diesel", "db-sqlite"] }

# Đổi sang sqlx (community) — code KHÔNG thay đổi
kernway-orm-sqlx = "0.4"
```

```toml
# config/application.toml
[db]
url      = "${DATABASE_URL}"
max_size = 10
```

```toml
# .env
DATABASE_URL=postgresql://user:pass@localhost/mydb
```

---

## Define an entity

```rust
// src/model/user.rs
use kernway::prelude::*;

#[entity(table = "users")]
pub struct User {
    #[id(strategy = "auto")]
    pub id: u64,

    #[column]
    pub name: String,

    #[column(unique)]
    pub email: String,

    #[column(default = true)]
    pub active: bool,

    #[column(auto)]
    pub created_at: DateTime,

    // Relationship
    #[one_to_many(mapped_by = "user_id")]
    pub posts: Vec<Post>,
}
```

---

## Repository — auto-generated methods

```rust
// src/repository/user_repository.rs
use kernway::prelude::*;

// #[repository] tự sinh: find_by_id, find_by_email, find_by_active,
// find_by_email_and_active, exists_by_email, count, save, delete_by_id...
#[repository(User)]
pub struct UserRepository;
```

Use it in a service:

```rust
#[component]
pub struct UserService {
    #[inject] repo: Arc<UserRepository>,
}

impl UserService {
    // Auto-generated — không cần viết thêm gì
    pub async fn find_by_email(&self, email: &str) -> Result<Option<User>, OrmError> {
        self.repo.find_by_email(email).await
    }

    // Lambda query — như LINQ
    pub async fn find_active_users(&self, page: u64) -> Result<Page<User>, OrmError> {
        self.repo.query()
            .filter(|u| u.active == true)
            .order_by_desc(|u| u.created_at)
            .fetch_page(page, 20)
            .await
    }

    // Eager load — tránh N+1
    pub async fn find_with_posts(&self, id: u64) -> Result<Option<User>, OrmError> {
        self.repo.query()
            .filter(|u| u.id == id)
            .with("posts")       // JOIN users + posts — 1 query duy nhất
            .fetch_one()
            .await
    }
}
```

---

## Transaction

```rust
impl UserService {
    #[transactional]   // commit khi Ok, rollback khi Err
    pub async fn transfer_credits(&self, from: u64, to: u64, amount: u32) -> Result<(), AppError> {
        self.repo.deduct_credits(from, amount).await?;
        self.repo.add_credits(to, amount).await?;
        Ok(())
    }
}
```

---

## Custom query

```rust
// SQL trực tiếp khi cần — vẫn type-safe
let users = query!(
    "SELECT * FROM users WHERE email LIKE ? AND active = ?",
    "%@gmail.com", true
).fetch_all::<User>().await?;
```

---

## Migration

```bash
kernway db migrate           # chạy pending migrations
kernway db new add_users     # tạo migration mới
kernway db status
```

```rust
// Tự động migrate khi start
KernwayApp::builder()
    .migrate_on_start(true)
    .build()
    .run()
    .await
```

---

## Swap databases — change one line in Cargo.toml

```toml
# Đang dùng diesel PostgreSQL:
kernway = { version = "0.4", features = ["orm-diesel", "db-postgres"] }

# Đổi sang MySQL — code KHÔNG thay đổi:
kernway = { version = "0.4", features = ["orm-diesel", "db-mysql"] }

# Đổi sang community sqlx — code KHÔNG thay đổi:
kernway-orm-sqlx = "0.4"
```

---

## See also

- [Reference: Database](../reference/database.md)
- [`#[transactional]`](../reference/annotations.md)
- [Error Handling](error-handling.md)

## Multiple Databases

```rust
// Dùng qualifier để phân biệt nhiều pool
#[component]
#[qualifier("primary")]
struct PrimaryPool;
impl DbPool for PrimaryPool { /* postgres */ }

#[component]
#[qualifier("analytics")]
struct AnalyticsPool;
impl DbPool for AnalyticsPool { /* clickhouse */ }

// Inject đúng pool
#[repository(Report)]
#[qualifier("analytics")]
pub struct ReportRepository;
```

---

## See also

- [Reference: Database](../reference/database.md)
- [`#[transactional]`](../reference/annotations.md)
- [Error Handling](error-handling.md)
