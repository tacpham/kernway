# kernway-log — Logging Module

## Purpose

Structured logging for Kernway apps. Automatic request context (MDC-equivalent).  
Zero-overhead context propagation thanks to the thread-per-core architecture.

## Standards

- **OpenTelemetry** — trace context propagation (W3C Trace Context spec)
- **RFC 5424** — Syslog severity levels
- **NDJSON** — Newline-delimited JSON log format (production)

---

## Why thread-per-core improves logging

```
Tokio (work-stealing):
  Request → Thread A → log "start" [MDC: requestId=abc]
           → Task migrate → Thread B → log "done" [MDC: LOST]
  Fix: phải copy MDC context mỗi lần migrate → overhead

Kernway (thread-per-core):
  Request → Thread 2 → log "start" [MDC: requestId=abc]
           → KHÔNG migrate → Thread 2 → log "done" [MDC: requestId=abc] ✓
  Zero copy, zero overhead — hoạt động như Spring MDC
```

---

## Default — No setup required

```rust
// Không cần thêm gì — logging hoạt động ngay
#[kernway::main]
async fn main() {
    KernwayApp::builder()
        .build()
        .run()
        .await
}
```

**Default behavior:**

| | Default |
|---|---|
| Level | `INFO` |
| Format | `pretty` (when running in a color terminal) / `json` (when no TTY is available — Docker, CI) |
| Output | stdout |
| File | Disabled |
| Access log | Enabled — skip `/health`, `/ready`, `/metrics` |
| Sensitive masking | `password`, `token`, `secret`, `authorization` |

---

## Profile-based config (primary approach)

> **No code required** — only a config file. Kernway automatically reads the `[log]` section.

```toml
# config/application.toml — base config

[log]
level  = "INFO"
format = "pretty"     # pretty | json | compact

[log.access]
enabled    = true
skip_paths = ["/health", "/ready", "/metrics"]
```

```toml
# config/application-dev.toml — override cho dev
[log]
level  = "DEBUG"
format = "pretty"     # màu sắc, dễ đọc

[log.output.file]
enabled = false
```

```toml
# config/application-prod.toml — production
[log]
level  = "INFO"
format = "json"       # structured, dễ parse bởi ELK/Datadog

[log.modules]
"my_app::repository" = "DEBUG"   # verbose hơn cho DB layer

[log.output.file]
enabled  = true
path     = "/var/log/my-app/app.log"

[log.output.file.rotation]
strategy  = "daily"
max_files = 30
compress  = true
```

**Priority** (high → low):
```
Code (.plugin(LogPlugin::...))    ← override mọi thứ
  ↓
config/application-{profile}.toml
  ↓
config/application.toml
  ↓
Built-in defaults
```

---

## Code-based setup (optional — when overrides are needed)

```rust
// Chỉ dùng khi cần điều khiển phức tạp hơn config file cho phép
KernwayApp::builder()
    .plugin(LogPlugin::builder()
        .level(Level::WARN)            // override config file
        .format(LogFormat::Json)
        .access_log(true)
        .build())
    .build()
    .run()
    .await
```

---

## Custom Log Format

### Method 1 — Pattern string (simple)

```toml
# config/application.toml
[log]
format  = "custom"
pattern = "[{timestamp}] {level:5} {target} — {message} {fields}"

# Tokens: {timestamp} {level} {target} {message} {fields} {request_id} {thread}
```

Output:
```
[2024-01-15T10:30:00Z] INFO  my_app::service — user created id=42 request_id=abc
```

### Method 2 — Implement the `LogFormatter` trait (full control)

```rust
use kernway_log::{LogFormatter, LogRecord};

struct DatadogFormat;

impl LogFormatter for DatadogFormat {
    fn format(&self, record: &LogRecord) -> String {
        // Toàn quyền format — trả về String
        serde_json::json!({
            "dd.trace_id": record.trace_id,
            "dd.span_id":  record.span_id,
            "message":     record.message,
            "level":       record.level.as_str(),
            "service":     "my-app",
            "env":         std::env::var("ENV").unwrap_or_default(),
        }).to_string()
    }

    fn format_access(&self, record: &AccessRecord) -> Option<String> {
        // None = dùng default access log format
        None
    }
}

// Đăng ký — thắng config file format
KernwayApp::builder()
    .plugin(LogPlugin::with_formatter(DatadogFormat))
```

### Method 3 — Override `LogBackend` (send elsewhere)

```rust
struct CloudWatchBackend { client: CloudWatchClient }

impl LogBackend for CloudWatchBackend {
    fn write(&self, record: &LogRecord) {
        self.client.put_log_event(record.to_cloudwatch());
    }
    fn flush(&self) { self.client.flush(); }
}

KernwayApp::builder()
    .plugin(LogPlugin::with_backend(CloudWatchBackend::new()))
```

---

## Injecting a Logger into a Component

### Method 1 — `#[logged]` macro (recommended)

```rust
#[component]
#[logged]                   // inject `self.log: Logger` tự động
struct UserService {
    #[inject] repo: Arc<UserRepository>,
}

impl UserService {
    async fn find_by_id(&self, id: u64) -> Result<User> {
        self.log.info(kv!("finding user", id));

        match self.repo.find(id).await {
            Ok(user) => {
                self.log.debug(kv!("user found", name = user.name));
                Ok(user)
            }
            Err(e) => {
                self.log.error(kv!("user not found", id, error = e));
                Err(e)
            }
        }
    }
}
```

### Method 2 — Direct `log!` macro usage (no injection required)

```rust
use kernway::log::{info, debug, warn, error};

async fn handler(...) -> impl IntoResponse {
    info!("processing request", user_id = 42);
    warn!("slow query detected", duration_ms = 450);
    error!("database error", error = %e);
}
```

### Method 3 — Get the logger from context

```rust
let logger = Logger::current();   // lấy logger của request hiện tại
logger.info("custom message");
```

---

## Log Levels

```rust
self.log.trace("very verbose");        // TRACE — disabled in production
self.log.debug("debug info");          // DEBUG — disabled in production
self.log.info("normal operation");     // INFO  — default
self.log.warn("something unexpected"); // WARN
self.log.error("operation failed");    // ERROR
```

Config level:
```toml
# config/application.toml
[log]
level = "INFO"

# Per-module level override
[log.modules]
"my_app::repository" = "DEBUG"    # verbose cho repository layer
"my_app::controller" = "INFO"
"kernway"            = "WARN"     # ít verbose hơn cho framework internals
```

---

## Automatic Request Context (MDC)

The framework automatically attaches this data to **every log line** in the request:

```json
{
  "timestamp": "2024-01-15T10:30:00.123Z",
  "level": "INFO",
  "message": "finding user",
  "id": 42,
  "request_id": "550e8400-e29b-41d4-a716-446655440000",
  "method": "GET",
  "path": "/users/42",
  "user_id": "123",
  "session_id": "abc",
  "target": "my_app::service::user_service"
}
```

Add custom fields to the context:

```rust
// Trong auth layer — thêm user info vào mọi log sau đó
RequestContext::current().set("tenant_id", tenant.id);
RequestContext::current().set("user_role", user.role);
```

---

## `#[traced]` — Auto span + timing

```rust
impl OrderService {
    #[traced]   // tự động log start/end/duration/error
    async fn process(&self, order_id: u64) -> Result<Order> {
        // AUTO LOG:
        // INFO  process{order_id=99} started
        // INFO  process{order_id=99} completed duration_ms=45
        // ERROR process{order_id=99} failed error="PaymentError" duration_ms=12
    }

    #[traced(level = "DEBUG")]   // chỉ log ở DEBUG level
    async fn internal_calc(&self) -> f64 { ... }

    #[traced(skip(password))]    // không log field nhạy cảm
    async fn login(&self, username: &str, password: &str) -> Result<Token> { ... }
}
```

---

## Access Log

Automatically log every HTTP request/response:

```json
{ "type": "access", "event": "request",  "request_id": "550e8400", "method": "GET",  "path": "/users/42", "remote_addr": "10.0.0.1", "timestamp": "..." }
{ "type": "access", "event": "response", "request_id": "550e8400", "status": 200,    "duration_ms": 12,   "bytes": 248,              "timestamp": "..." }
```

Config:
```toml
[log.access]
enabled    = true
level      = "INFO"
skip_paths = ["/health", "/ready", "/metrics"]  # không log health checks
```

---

## Log Formats

### JSON (production — default)

```json
{"timestamp":"2024-01-15T10:30:00.123Z","level":"INFO","message":"user created","user_id":42,"request_id":"550e8400","target":"my_app::service"}
```

### Pretty (development)

```
2024-01-15T10:30:00.123Z  INFO my_app::service user created user_id=42 request_id=550e8400
2024-01-15T10:30:00.145Z  WARN my_app::repo    slow query duration_ms=450 query="SELECT * FROM users"
2024-01-15T10:30:00.146Z ERROR my_app::service db error error=ConnectionRefused
```

### Compact (low-resource environments)

```
[10:30:00] INFO  user created id=42
[10:30:00] ERROR db error err=ConnectionRefused
```

Config:
```toml
[log]
format = "json"      # json | pretty | compact
```

---

## File Output + Rotation (Archive)

```toml
# config/application.toml

[log.output]
console = true                    # stdout (mặc định true)

[log.output.file]
enabled  = true
path     = "logs/app.log"         # đường dẫn file

[log.output.file.rotation]
strategy  = "daily"               # daily | size | hourly
max_size  = "100MB"               # dùng với strategy = "size"
max_files = 30                    # giữ tối đa 30 file
compress  = true                  # gzip các file cũ
pattern   = "logs/app.%Y-%m-%d.log"  # tên file theo ngày
```

Result:
```
logs/
├── app.log              ← file hiện tại
├── app.2024-01-14.log.gz
├── app.2024-01-13.log.gz
└── app.2024-01-12.log.gz
```

---

## Full Example Config File

```toml
# config/application.toml — log config

[log]
level  = "INFO"
format = "json"        # json (prod) | pretty (dev) | compact

# Per-module level
[log.modules]
"my_app::repository" = "DEBUG"
"my_app::service"    = "INFO"
"my_app::controller" = "INFO"
"kernway"            = "WARN"

# Access log
[log.access]
enabled    = true
level      = "INFO"
skip_paths = ["/health", "/ready", "/metrics"]

# Console output
[log.output]
console = true
color   = false          # true chỉ khi pretty format

# File output
[log.output.file]
enabled  = true
path     = "logs/app.log"

[log.output.file.rotation]
strategy  = "daily"
max_files = 30
compress  = true
pattern   = "logs/app.%Y-%m-%d.log"

# Sensitive field masking
[log.mask]
fields = ["password", "token", "secret", "credit_card"]
# → "password": "***" trong log output
```

---

## Profile-based Config

```toml
# config/application-dev.toml — override cho dev
[log]
level  = "DEBUG"
format = "pretty"        # dễ đọc hơn khi dev

[log.output.file]
enabled = false          # không ghi file khi dev
```

```toml
# config/application-prod.toml — production
[log]
level  = "WARN"          # ít verbose hơn
format = "json"

[log.output.file]
enabled = true
path    = "/var/log/my-app/app.log"

[log.output.file.rotation]
strategy  = "daily"
max_files = 90           # giữ 90 ngày
compress  = true
```

---

## Distributed Tracing (OpenTelemetry)

```toml
[log.tracing]
enabled  = true
exporter = "otlp"                           # otlp | jaeger | zipkin
endpoint = "http://localhost:4317"          # OpenTelemetry Collector
service  = "my-app"
```

```rust
// #[traced] tự động propagate trace context qua HTTP headers:
// traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01
// (W3C Trace Context spec)
```

---

## Override Default Logger (plugin system)

```rust
// Custom log backend — implement trait
struct DatadogLogger { /* ... */ }
impl LogBackend for DatadogLogger {
    fn write(&self, record: &LogRecord) {
        // gửi lên Datadog
    }
}

// Đăng ký
KernwayApp::builder()
    .plugin(LogPlugin::with_backend(DatadogLogger::new(api_key)))
```

---

## Workspace

```
crates/
└── kernway-log/
    ├── src/
    │   ├── lib.rs          // re-export
    │   ├── logger.rs       // Logger struct, levels
    │   ├── context.rs      // RequestContext (MDC equivalent)
    │   ├── format/
    │   │   ├── json.rs
    │   │   ├── pretty.rs
    │   │   └── compact.rs
    │   ├── output/
    │   │   ├── console.rs
    │   │   └── file.rs     // rotation, compression
    │   ├── plugin.rs       // LogPlugin
    │   └── macros.rs       // info!, debug!, warn!, error!, kv!
    └── Cargo.toml
```

Dependencies: `tracing` (spans), `tracing-subscriber` (output), `rolling-file` (rotation).  
`tracing` is a whitelisted dependency — it does not conflict with the Kernway runtime.
