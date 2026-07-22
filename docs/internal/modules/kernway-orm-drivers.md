# kernway-orm — Multi-Database Driver Strategy

> All drivers implement the `kernway-orm-core` traits.  
> User code does NOT change when swapping databases.

---

## Driver Matrix

| Database | Module | Rust Crate | Async | Cross-platform | Tier |
|---|---|---|---|---|---|
| PostgreSQL | `kernway-orm-sqlx` | `sqlx` | ✅ Native | ✅ | Official |
| MySQL | `kernway-orm-sqlx` | `sqlx` | ✅ Native | ✅ | Official |
| SQLite | `kernway-orm-sqlx` | `sqlx` | ✅ | ✅ | Official |
| MongoDB | `kernway-orm-mongo` | `mongodb` (official) | ✅ Native | ✅ | Official |
| SQL Server | `kernway-orm-mssql` | `tiberius` (pure Rust) | ✅ Native | ✅ | Official v0.6+ |
| Oracle | `kernway-orm-oracle` | `odbc-api` | ⚠️ spawn_blocking | ✅ (requires an ODBC driver) | Community v1.0+ |
| Redis | `kernway-cache` | `fred` | ✅ Native | ✅ | Official v0.5+ (not an ORM) |

> **Oracle note**: There is no native pure-Rust Oracle driver (Oracle Corp has not provided one).  
> The ODBC approach is production-grade, but requires the Oracle ODBC driver to be installed on the OS.  
> Enterprise Oracle environments usually already have ODBC set up.

---

## Why sqlx instead of diesel?

| | diesel | sqlx |
|---|---|---|
| Query checking | Compile-time ✅ | Compile-time (with macros) ✅ |
| Async | ❌ (requires spawn_blocking) | ✅ Native async |
| Databases | PG, MySQL, SQLite | PG, MySQL, SQLite, MSSQL |
| Pure Rust | ✅ | ✅ |
| Performance | Good | Better (native async) |

**Decision**: `kernway-orm-sqlx` is the **official reference implementation**.  
`kernway-orm-diesel` is still supported for users who need stricter compile-time query checking.

---

## 1. kernway-orm-sqlx — SQL databases

Supports: PostgreSQL, MySQL, and SQLite (one crate, selected via feature flags).

### Cargo.toml

```toml
# PostgreSQL
kernway = { version = "0.4", features = ["orm-sqlx", "db-postgres"] }

# MySQL
kernway = { version = "0.4", features = ["orm-sqlx", "db-mysql"] }

# SQLite
kernway = { version = "0.4", features = ["orm-sqlx", "db-sqlite"] }
```

### How `Repository<T>` is implemented

```rust
// kernway-orm-sqlx/src/repository.rs

pub struct SqlxRepository<T: Entity> {
    pool: Arc<sqlx::Pool<DB>>,   // DB = Postgres | MySql | Sqlite
    _phantom: PhantomData<T>,
}

impl<T: Entity + for<'r> sqlx::FromRow<'r, DB::Row>> Repository<T> for SqlxRepository<T> {
    async fn find_by_id(&self, id: &T::Id) -> Result<Option<T>, OrmError> {
        let sql = format!(
            "SELECT * FROM {} WHERE {} = $1",
            T::table_name(),
            T::id_column()
        );
        sqlx::query_as::<_, T>(&sql)
            .bind(id)
            .fetch_optional(&*self.pool)
            .await
            .map_err(map_sqlx_error)
    }

    async fn save(&self, entity: T) -> Result<T, OrmError> {
        // Upsert: INSERT ... ON CONFLICT DO UPDATE
        // SQL tự sinh từ T::columns() metadata
        let sql = build_upsert_sql::<T>();
        sqlx::query_as::<_, T>(&sql)
            .bind_entity(&entity)           // macro-generated
            .fetch_one(&*self.pool)
            .await
            .map_err(map_sqlx_error)
    }
}

fn map_sqlx_error(e: sqlx::Error) -> OrmError {
    match e {
        sqlx::Error::RowNotFound             => OrmError::NotFound,
        sqlx::Error::Database(dbe) if dbe.is_unique_violation()
                                             => OrmError::UniqueViolation { field: ... },
        sqlx::Error::Database(dbe) if dbe.is_foreign_key_violation()
                                             => OrmError::ForeignKeyViolation,
        _                                    => OrmError::Query(e.to_string()),
    }
}
```

### QueryBuilder → SQL WHERE

```rust
impl<T: Entity> QueryBuilder<T> for SqlxQueryBuilder<T> {
    fn filter(self: Box<Self>, predicate: Box<dyn EntityPredicate<T>>) -> Box<dyn QueryBuilder<T>> {
        // predicate.to_sql() → "email = $1 AND active = $2"
        Box::new(SqlxQueryBuilder {
            where_clauses: { let mut c = self.where_clauses; c.push(predicate.to_sql()); c },
            bindings: { let mut b = self.bindings; b.extend(predicate.bindings()); b },
            ..(*self)
        })
    }
}
```

---

## 2. kernway-orm-mongo — MongoDB

MongoDB is a **document store**, not a relational database. Key differences:

| SQL | MongoDB |
|---|---|
| Table | Collection |
| Row | Document (BSON) |
| WHERE clause | BSON filter `{ field: value }` |
| JOIN | `$lookup` aggregation |
| Transaction | ✅ (requires a replica set) |
| Schema | Flexible (no ALTER TABLE) |

### Cargo.toml

```toml
kernway = { version = "0.5", features = ["orm-mongo"] }
```

### Entity with MongoDB

```rust
// #[id] map sang _id field của MongoDB
#[entity(collection = "users")]   // collection thay vì table
pub struct User {
    #[id]                          // map sang _id: ObjectId
    pub id: ObjectId,

    #[column]
    pub name: String,

    #[column(index = true)]        // tạo MongoDB index
    pub email: String,

    // Embedded document (MongoDB-only, không có trong SQL)
    #[embedded]
    pub address: Address,

    // Reference (tương đương FK)
    #[ref_one(collection = "posts")]
    pub posts: Vec<ObjectId>,
}
```

### Repository<T> → MongoDB operations

```rust
impl<T: Entity + Serialize + DeserializeOwned> Repository<T> for MongoRepository<T> {
    async fn find_by_id(&self, id: &T::Id) -> Result<Option<T>, OrmError> {
        self.collection
            .find_one(doc! { "_id": id }, None)
            .await
            .map_err(map_mongo_error)
    }

    async fn save(&self, entity: T) -> Result<T, OrmError> {
        let doc = bson::to_document(&entity)?;
        self.collection
            .replace_one(
                doc! { "_id": entity.id() },
                doc,
                ReplaceOptions::builder().upsert(true).build(),
            )
            .await?;
        Ok(entity)
    }
}
```

### QueryBuilder<T> → BSON filter

```rust
// User code (giống nhau với SQL):
repo.query()
    .filter(|u| u.email == email)
    .order_by_desc(|u| u.created_at)
    .fetch_page(0, 20)
    .await

// Bên trong MongoQueryBuilder, filter() thêm vào BSON doc:
// { "email": "test@example.com" }
// Kết quả cuối: collection.find(filter_doc, find_options).await
```

### .with() → $lookup aggregation

```rust
// SQL: JOIN users LEFT JOIN posts ON posts.user_id = users._id
// MongoDB equivalent: $lookup aggregation pipeline
repo.query()
    .filter(|u| u.id == user_id)
    .with("posts")   // → $lookup { from: "posts", localField: "_id", ... }
    .fetch_one()
    .await
```

---

## 3. kernway-orm-mssql — SQL Server

Uses `tiberius` — **pure Rust**, no C library required, native async.

```toml
kernway = { version = "0.6", features = ["orm-mssql"] }
```

```toml
# config/application.toml
[db]
url      = "mssql://user:pass@server/database"
max_size = 10
```

### Implementation

Similar to `kernway-orm-sqlx`, but replaces sqlx with `tiberius`:
- Different SQL dialect: MSSQL uses `TOP` instead of `LIMIT`, and `OFFSET FETCH` instead of `OFFSET`
- Upsert: `MERGE INTO ... USING ... ON ... WHEN MATCHED THEN UPDATE ... WHEN NOT MATCHED THEN INSERT`
- Types: `NVARCHAR`, `DATETIME2` instead of `VARCHAR`, `TIMESTAMP`

The `kernway-orm-core` spec does not expose SQL dialect details, so the driver handles them and user code remains transparent.

---

## 4. kernway-orm-oracle — Oracle Database

> ⚠️ **Requirement**: Oracle ODBC Driver must be installed on the server.  
> There is no pure-Rust Oracle driver — Oracle Corp has not provided one.

```toml
kernway-orm-oracle = "1.0"   # community crate, không built-in
```

```toml
# config/application.toml
[db]
url      = "oracle://user:pass@host:1521/SID"
max_size = 10
odbc_dsn = "OracleODBC"      # ODBC DSN name
```

### How it is implemented

Uses the `odbc-api` crate + `spawn_blocking` (ODBC is not native async):

```rust
impl<T: Entity> Repository<T> for OracleRepository<T> {
    async fn find_by_id(&self, id: &T::Id) -> Result<Option<T>, OrmError> {
        let pool = self.pool.clone();
        let id   = id.clone();
        spawn_blocking(move || {
            let conn = pool.get()?;
            // odbc-api synchronous call
            let stmt = conn.prepare(&format!(
                "SELECT * FROM {} WHERE {} = ?", T::table_name(), T::id_column()
            ))?;
            stmt.execute((&id,))?.fetch::<T>()
        }).await
    }
}
```

Like MSSQL, Oracle has its own dialect:
- Pagination: `ROWNUM` (Oracle 11g-) or `OFFSET ... ROWS FETCH NEXT ... ROWS ONLY` (Oracle 12c+)
- Upsert: `MERGE INTO`
- Sequences instead of AUTOINCREMENT

---

## 5. kernway-cache — Redis

> Redis is **not an ORM**. It has no schema, no Entity model, and no SQL.  
> It is a **Cache / KV store** → a completely separate module.

```toml
kernway = { version = "0.5", features = ["cache-redis"] }
```

```toml
# config/application.toml
[cache]
url      = "redis://localhost:6379"
pool_size = 10
```

### Cache<K, V> trait

```rust
// kernway-cache-core/src/lib.rs

pub trait Cache: Send + Sync {
    async fn get<V: DeserializeOwned>(&self, key: &str)
        -> Result<Option<V>, CacheError>;

    async fn set<V: Serialize>(&self, key: &str, value: &V, ttl_secs: Option<u64>)
        -> Result<(), CacheError>;

    async fn delete(&self, key: &str)
        -> Result<bool, CacheError>;

    async fn exists(&self, key: &str)
        -> Result<bool, CacheError>;

    async fn increment(&self, key: &str, delta: i64)
        -> Result<i64, CacheError>;

    // Multi-key
    async fn mget<V: DeserializeOwned>(&self, keys: &[&str])
        -> Result<Vec<Option<V>>, CacheError>;

    async fn mset<V: Serialize>(&self, entries: &[(&str, &V)], ttl_secs: Option<u64>)
        -> Result<(), CacheError>;
}
```

### Usage in a service

```rust
#[component]
pub struct UserService {
    #[inject] repo:  Arc<UserRepository>,
    #[inject] cache: Arc<dyn Cache>,
}

impl UserService {
    pub async fn find_user(&self, id: u64) -> Result<Option<User>, AppError> {
        let key = format!("user:{id}");

        // Cache-aside pattern
        if let Some(user) = self.cache.get::<User>(&key).await? {
            return Ok(Some(user));
        }

        let user = self.repo.find_by_id(&id).await?;
        if let Some(ref u) = user {
            self.cache.set(&key, u, Some(300)).await?;   // TTL 5 phút
        }
        Ok(user)
    }
}
```

### `#[cacheable]` annotation

```rust
// Tự động cache-aside, không cần viết thủ công:
impl UserService {
    #[cacheable(key = "user:{id}", ttl = 300)]
    pub async fn find_user(&self, id: u64) -> Result<Option<User>, AppError> {
        self.repo.find_by_id(&id).await.map_err(Into::into)
    }

    #[cache_evict(key = "user:{user.id}")]
    pub async fn update_user(&self, user: User) -> Result<User, AppError> {
        self.repo.save(user).await.map_err(Into::into)
    }
}
```

### Redis-specific features (beyond the Cache trait)

```rust
// Pub/Sub
cache.publish("events.user.created", &event).await?;
cache.subscribe("events.*", |msg| async { ... }).await?;

// Distributed lock
let lock = cache.acquire_lock("process:job:42", Duration::from_secs(30)).await?;
// ... critical section
lock.release().await?;

// Rate limiting (built-in Redis Lua script)
let allowed = cache.rate_limit("ip:1.2.3.4", limit: 100, window_secs: 60).await?;
```

---

## Summary: user code stays the same when swapping databases

```rust
// src/service/user_service.rs — CODE NÀY KHÔNG ĐỔI dù dùng bất kỳ DB nào

#[component]
pub struct UserService {
    #[inject] repo: Arc<UserRepository>,
}

pub async fn find_active_users(page: u64) -> Result<Page<User>, AppError> {
    self.repo.query()
        .filter(|u| u.active == true)
        .order_by_desc(|u| u.created_at)
        .fetch_page(page, 20)
        .await
        .map_err(Into::into)
}
```

Only `Cargo.toml` changes:

```toml
# PostgreSQL → MySQL → MongoDB — business logic KHÔNG thay đổi
kernway = { version = "0.4", features = ["orm-sqlx", "db-mysql"] }
# kernway = { version = "0.5", features = ["orm-mongo"] }
# kernway = { version = "0.6", features = ["orm-mssql"] }
```

---

## Roadmap

| Version | Driver | Status |
|---|---|---|
| v0.4 | `kernway-orm-sqlx` (PostgreSQL, MySQL, SQLite) | Planned |
| v0.5 | `kernway-cache` (Redis) | Planned |
| v0.6 | `kernway-orm-mssql` (SQL Server via tiberius) | Planned |
| v0.6 | `kernway-orm-mongo` (MongoDB) | Planned |
| v1.0+ | `kernway-orm-oracle` (Oracle via ODBC) | Community |
