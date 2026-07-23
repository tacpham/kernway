# kernway-db — Database Bridge

## Purpose

Bridge blocking DB operations (diesel + r2d2) into the async executor via `spawn_blocking`.

## Architecture

```
Async handler
│
├── spawn_blocking(|| db.find_user(id))   ← blocking call
│   │
│   └── Thread pool (r2d2 connections)
│       └── diesel query → PostgreSQL/MySQL/SQLite
│
└── await result via channel
```

**This pattern is equivalent to** Java Project Loom virtual-thread offloading, or `CompletableFuture.supplyAsync(blockingExecutor)`.

## Supported Databases

| Database | Driver | Pool |
|---|---|---|
| PostgreSQL | `diesel` + `diesel-postgres` | `r2d2` |
| MySQL | `diesel` + `diesel-mysql` | `r2d2` |
| SQLite | `diesel` + `diesel-sqlite` | `r2d2` |

Feature flags:

```toml
kernway = { version = "0.4", features = ["db-postgres"] }
kernway = { version = "0.4", features = ["db-mysql"] }
kernway = { version = "0.4", features = ["db-sqlite"] }
```

## Usage

```rust
// repository/user_repository.rs
#[component]
struct UserRepository {
    #[inject]
    pool: Arc<dyn DbPool>,
}

impl UserRepository {
    pub async fn find_by_id(&self, id: u64) -> Result<Option<User>, DbError> {
        let pool = self.pool.clone();
        spawn_blocking(move || {
            let conn = pool.get()?;   // r2d2 blocking get
            users::table
                .find(id as i64)
                .first::<User>(&conn)
                .optional()
        }).await
    }

    #[transactional]
    pub async fn create(&self, req: CreateUserRequest) -> Result<User, DbError> {
        let pool = self.pool.clone();
        spawn_blocking(move || {
            let conn = pool.get()?;
            diesel::insert_into(users::table)
                .values(&req)
                .get_result::<User>(&conn)
        }).await
    }
}
```

## Connection Pool Config

```rust
KernwayApp::builder()
    .db(PostgresPool::builder()
        .url(env!("DATABASE_URL"))
        .max_size(10)                          // default: num_cores * 2
        .connection_timeout(Duration::from_secs(5))
        .idle_timeout(Duration::from_secs(600))
        .build()?)
```

## Migration Support

```bash
# kernway-cli integrates with diesel_migrations
kernway db migrate        # run pending migrations
kernway db rollback       # rollback last migration
kernway db status         # show migration status
```

```rust
// Embedded migrations — run at startup
embed_migrations!("migrations/");

#[kernway::main]
async fn main() {
    KernwayApp::builder()
        .db(pool)
        .migrate_on_start(true)    // run migrations before accepting requests
        .build()
        .run()
        .await
}
```

## Why Not Native Async DB?

- Native async `sqlx` requires tokio — it cannot be used with the Kernway runtime
- `diesel` is battle-tested, provides compile-time query validation, and avoids runtime SQL strings
- The `spawn_blocking` pattern adds one channel send/receive per query (~microseconds) — acceptable overhead
- If the community wants native async support, implement the `DbPool` trait with a custom driver — no Kernway fork required
