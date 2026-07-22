# Logging

> Set up and use logging in a Kernway app.

## Default — No configuration required

Kernway logs automatically with sensible defaults — no setup required:

```rust
#[kernway::main]
async fn main() {
    KernwayApp::builder().build().run().await
    // Logging đã hoạt động ngay
}
```

| | Default |
|---|---|
| Level | `INFO` |
| Format | `pretty` (terminal) / `json` (Docker/CI) |
| Output | stdout |
| File | Disabled |
| Access log | Enabled — skip `/health` `/ready` `/metrics` |

---

## Configure via file (primary approach)

```toml
# config/application.toml
[log]
level  = "INFO"
format = "pretty"    # pretty | json | compact

[log.access]
enabled    = true
skip_paths = ["/health", "/ready", "/metrics"]
```

```toml
# config/application-dev.toml
[log]
level  = "DEBUG"
format = "pretty"

[log.output.file]
enabled = false
```

```toml
# config/application-prod.toml
[log]
level  = "INFO"
format = "json"

[log.modules]
"my_app::repository" = "DEBUG"

[log.output.file]
enabled   = true
path      = "/var/log/my-app/app.log"

[log.output.file.rotation]
strategy  = "daily"
max_files = 30
compress  = true
```

---

## Use a Logger in a Component

### `#[logged]` — automatically inject a logger (recommended)

```rust
#[component]
#[logged]
struct UserService {
    #[inject] repo: Arc<UserRepository>,
}

impl UserService {
    async fn find_by_id(&self, id: u64) -> Result<User> {
        self.log.info(format!("finding user id={id}"));

        match self.repo.find(id).await {
            Ok(user) => {
                self.log.debug(format!("found: {}", user.name));
                Ok(user)
            }
            Err(e) => {
                self.log.error(format!("not found id={id}: {e}"));
                Err(e)
            }
        }
    }
}
```

### Direct macros — no injection needed

```rust
use kernway::log::{info, debug, warn, error};

async fn handler() -> impl IntoResponse {
    info!("request received", user_id = 42);
    warn!("slow query", duration_ms = 450);
    error!("db error", error = %e);
}
```

---

## Log Levels

```rust
self.log.trace("rất chi tiết");    // TRACE — tắt trong production
self.log.debug("debug info");      // DEBUG — tắt trong production
self.log.info("bình thường");      // INFO — default
self.log.warn("bất thường");       // WARN
self.log.error("lỗi");            // ERROR
```

Per-module config:
```toml
[log.modules]
"my_app::repository" = "DEBUG"   # verbose cho DB layer
"my_app::service"    = "INFO"
"kernway"            = "WARN"    # ít verbose cho framework
```

---

## `#[traced]` — Automatically log timing

```rust
impl OrderService {
    #[traced]
    async fn process(&self, order_id: u64) -> Result<Order> {
        // Tự động log:
        // INFO  process{order_id=99} started
        // INFO  process{order_id=99} completed duration_ms=45
        // ERROR process{order_id=99} failed error="..." duration_ms=12
    }

    #[traced(skip(password))]    // không log field nhạy cảm
    async fn login(&self, username: &str, password: &str) -> Result<Token> { }
}
```

---

## Automatic Request Context (MDC)

Every log line within a request automatically includes context:

```json
{
  "timestamp": "2024-01-15T10:30:00.123Z",
  "level": "INFO",
  "message": "finding user",
  "id": 42,
  "request_id": "550e8400",
  "method": "GET",
  "path": "/users/42",
  "user_id": "123"
}
```

Add a custom field to the context:
```rust
RequestContext::current().set("tenant_id", tenant.id);
```

---

## File Rotation

```toml
[log.output.file]
enabled  = true
path     = "logs/app.log"

[log.output.file.rotation]
strategy  = "daily"      # daily | size | hourly
max_size  = "100MB"      # dùng với strategy = "size"
max_files = 30
compress  = true
pattern   = "logs/app.%Y-%m-%d.log"
```

Result:
```
logs/
├── app.log               ← file hiện tại
├── app.2024-01-14.log.gz
└── app.2024-01-13.log.gz
```

---

## Custom Format

**Pattern string** (simple):
```toml
[log]
format  = "custom"
pattern = "[{timestamp}] {level:5} {target} — {message} {fields}"
```

**Implement `LogFormatter`** (full control):
```rust
struct MyFormat;
impl LogFormatter for MyFormat {
    fn format(&self, record: &LogRecord) -> String {
        format!("[{}] {} {}", record.level, record.target, record.message)
    }
}

KernwayApp::builder()
    .plugin(LogPlugin::with_formatter(MyFormat))
```

---

## Sensitive Field Masking

```toml
[log.mask]
fields = ["password", "token", "secret", "authorization", "credit_card"]
# Output: "password": "***"
```

---

## See also

- [Reference: Logging](../reference/logging.md) — all configuration options
- [`#[traced]`](../reference/annotations.md) — distributed tracing
