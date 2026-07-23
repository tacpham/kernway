# Caching with Redis

> `kernway-cache` — Cache-aside pattern, distributed lock, pub/sub.  
> Redis is a KV store, **not an ORM** — it is a completely separate module.

## Setup

```toml
# Cargo.toml
kernway = { version = "0.5", features = ["cache-redis"] }
```

```toml
# config/application.toml
[cache]
url       = "${REDIS_URL}"    # redis://localhost:6379
pool_size = 10
```

---

## #[cacheable] — automatic cache-aside

```rust
#[component]
pub struct UserService {
    #[inject] repo: Arc<UserRepository>,
}

impl UserService {
    // Automatic: check the cache → on a miss, call the function → store the result
    #[cacheable(key = "user:{id}", ttl = 300)]
    pub async fn find_user(&self, id: u64) -> Result<Option<User>, AppError> {
        self.repo.find_by_id(&id).await.map_err(Into::into)
    }

    // Drop the cache entry on update
    #[cache_evict(key = "user:{user.id}")]
    pub async fn update_user(&self, user: User) -> Result<User, AppError> {
        self.repo.save(user).await.map_err(Into::into)
    }
}
```

---

## Manual cache

```rust
#[component]
pub struct ProductService {
    #[inject] repo:  Arc<ProductRepository>,
    #[inject] cache: Arc<dyn Cache>,
}

impl ProductService {
    pub async fn find_product(&self, id: u64) -> Result<Option<Product>, AppError> {
        let key = format!("product:{id}");

        if let Some(p) = self.cache.get::<Product>(&key).await? {
            return Ok(Some(p));
        }

        let product = self.repo.find_by_id(&id).await?;
        if let Some(ref p) = product {
            self.cache.set(&key, p, Some(600)).await?;   // TTL 10 minutes
        }
        Ok(product)
    }
}
```

---

## Multi-key (batch)

```rust
// Fetch many keys at once — cheaper than N separate gets
let users = cache.mget::<User>(&["user:1", "user:2", "user:3"]).await?;

// Set many keys
cache.mset(&[("user:1", &u1), ("user:2", &u2)], Some(300)).await?;
```

---

## Counter / Rate limiting

```rust
// Atomic increment
let views = cache.increment("post:42:views", 1).await?;

// Rate limiting (built-in Redis Lua — atomic, no race condition)
let allowed = cache.rate_limit("ip:1.2.3.4", 100, 60).await?;
if !allowed {
    return Err(AppError::TooManyRequests);
}
```

---

## Distributed lock

```rust
// Run the job on exactly one instance in the cluster
let lock = cache.acquire_lock("cron:daily-report", Duration::from_secs(60)).await?;
match lock {
    Some(guard) => {
        run_daily_report().await?;
        guard.release().await?;
    }
    None => {
        // Another instance is already running it — skip
    }
}
```

---

## Pub/Sub

```rust
// Publisher
cache.publish("events.order.created", &order_event).await?;

// Subscriber (runs a background listener)
cache.subscribe("events.order.*", |event: OrderEvent| async move {
    send_notification(event).await?;
    Ok(())
}).await?;
```

---

## Comparison with Spring

| Spring | kernway-cache |
|---|---|
| `@Cacheable` | `#[cacheable(key="...", ttl=N)]` |
| `@CacheEvict` | `#[cache_evict(key="...")]` |
| `RedisTemplate` | `Arc<dyn Cache>` injection |
| `CacheManager` | automatic via DI |
| `@EnableCaching` | not required (auto-enabled) |

---

## See also

- [Reference: Database](../reference/database.md)
- [Error Handling](error-handling.md)
