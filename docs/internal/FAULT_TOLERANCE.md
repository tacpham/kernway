# Kernway — Fault Tolerance

> Kernway is designed to **never stop because of a single failure**. Each failure level has its own handling mechanism — only truly fatal failures stop the app.

---

## Overview of the 5 levels

```
Cấp 1 — Request Error     Request đó fail   App không hay biết      ✅ Tự xử lý
Cấp 2 — Handler Panic     HTTP 500           Core thread tiếp tục    ✅ catch_unwind
Cấp 3 — Core Crash        ~10ms downtime     Supervisor restart       ✅ Auto-recover
Cấp 4 — Startup Error     App không start    Phát hiện sớm           ⚠️  Fix & restart
Cấp 5 — Fatal             App dừng           OOM / all cores down     ❌ Docker restart
```

---

## Level 1 — Request Error

**Impact**: Only that request receives an error. The app does not notice. Zero impact.

```rust
// Lỗi bình thường qua Result — #[exception_handler] bắt và trả HTTP response
#[route(GET, "/users/{id}")]
async fn get_user(Path(id): Path<u64>, ctrl: &UserController) -> Result<Json<User>, AppError> {
    let user = ctrl.service.find(id).await?;  // Err → exception_handler
    Ok(Json(user))
}

// Global exception handler
#[exception_handler]
async fn handle_app_error(err: AppError) -> impl IntoResponse {
    match err {
        AppError::NotFound(msg)    => (StatusCode::NOT_FOUND,             Json(error_body(404, msg))),
        AppError::Unauthorized     => (StatusCode::UNAUTHORIZED,          Json(error_body(401, "unauthorized"))),
        AppError::Forbidden        => (StatusCode::FORBIDDEN,             Json(error_body(403, "forbidden"))),
        AppError::Validation(errs) => (StatusCode::UNPROCESSABLE_ENTITY,  Json(validation_body(errs))),
        AppError::Internal(e)      => {
            log::error!("internal error: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(error_body(500, "internal server error")))
        }
    }
}
```

**Spring comparison**: Equivalent to `@ExceptionHandler` + `@ControllerAdvice`. Both handle this level well.

---

## Level 2 — Handler Panic

**Impact**: That request receives HTTP 500. The core thread **keeps running**.

**Rust issue**: A panic unwinds by default → the thread dies.

**Kernway solution**: `catch_unwind` wraps each task in the executor:

```rust
// rt-core/src/executor.rs — PHẢI implement đúng như này
impl Executor {
    fn poll_task(&self, task: Rc<Task>) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            task.poll()
        }));

        match result {
            Ok(_) => {}
            Err(panic_payload) => {
                // 1. Log panic với stack trace
                let msg = panic_message(&panic_payload);
                log::error!(
                    "handler panic: {msg}",
                    request_id = task.request_id(),
                    path       = task.path(),
                );

                // 2. Trả 500 cho request đó
                task.send_error_response(StatusCode::INTERNAL_SERVER_ERROR);

                // 3. Core thread tiếp tục — không crash
            }
        }
    }
}
```

**Behavior**:
```
Request A: GET /users/1   → đang xử lý bình thường
Request B: GET /users/2   → handler panic!
Request C: GET /users/3   → đang xử lý bình thường

Kết quả:
  Request A → 200 OK
  Request B → 500 Internal Server Error  (panic được bắt)
  Request C → 200 OK
  Core thread → tiếp tục chạy, không bị ảnh hưởng
```

**Spring comparison**: The JVM catches exceptions in the servlet container, and the thread returns to the pool. Equivalent behavior — but Kernway does it at the runtime layer without depending on a servlet container.

---

## Level 3 — Core Thread Crash

**Impact**: In-flight requests on that core get a connection reset. The core restarts in ~10ms.

Occurs when: a panic happens in reactor/executor code (a framework bug), not in handler code.

**Kernway solution**: An independent supervisor thread monitors all cores:

```rust
// kernway-server/src/supervisor.rs
pub struct Supervisor {
    cores: Vec<CoreHandle>,
}

struct CoreHandle {
    id:       usize,
    thread:   JoinHandle<()>,
    alive_tx: Sender<()>,
}

impl Supervisor {
    pub fn run(mut self) {
        loop {
            for i in 0..self.cores.len() {
                if self.cores[i].thread.is_finished() {
                    log::error!(
                        "Core {i} crashed unexpectedly — restarting",
                        core_id = i,
                    );

                    // Đợi một chút trước khi restart
                    // (tránh restart loop nếu là bug liên tục)
                    std::thread::sleep(Duration::from_millis(100));

                    // Spawn core mới
                    self.cores[i] = spawn_core(i);

                    log::info!("Core {i} restarted successfully");
                }
            }

            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
```

**Restart storm protection** — if a core keeps crashing:

```
Lần 1 crash → restart sau 100ms
Lần 2 crash → restart sau 500ms
Lần 3 crash → restart sau 2s
Lần 4 crash → restart sau 10s + alert log
Lần 5 crash → KHÔNG restart, log CRITICAL, giảm capacity
```

**Config**:
```toml
[supervisor]
restart_delay_ms = 100          # delay trước lần restart đầu
max_restart_attempts = 5        # sau đây không restart nữa
backoff = "exponential"         # linear | exponential
alert_threshold = 3             # log CRITICAL sau N lần crash
```

**Spring comparison**: Spring has no supervisor mechanism. If the JVM thread pool is saturated → the whole app stalls. In Kernway, each core is independent, so a crash on one core does not affect the others.

---

## Level 4 — Startup Errors

The app **does not start** — early detection is better than crashing while serving traffic.

| Startup error | Behavior | Config |
|---|---|---|
| Port already in use | Fail fast — clearly log which port | Cannot be overridden |
| DB connection failure | Configurable | `db.startup_check` |
| Missing required env var | Fail fast — log which variable is missing | Cannot be overridden |
| Config file parse error | Fail fast — log which line is wrong | Cannot be overridden |
| Circular bean dependency | **Compile error** — do not wait until runtime | — |
| Pending migration | Configurable | `db.migrate_on_start` |

```toml
# config/application.toml
[startup]
fail_fast = true              # false = warn và tiếp tục (không khuyến nghị)

[db]
startup_check     = "retry"   # fail_fast | retry | skip
retry_attempts    = 5
retry_delay_secs  = 2
migrate_on_start  = true      # chạy pending migrations trước khi accept traffic
```

**Startup sequence**:
```
1. Parse config → fail nếu lỗi
2. Validate env vars → fail nếu thiếu required
3. Bootstrap DI graph → fail nếu circular dep (đã bắt compile-time)
4. Connect DB pool → retry theo config
5. Run migrations → fail nếu migration error
6. Bind port → fail nếu bị chiếm
7. Start supervisor + cores
8. Ready — bắt đầu accept traffic
```

**Spring comparison**: Spring is similar — `ApplicationContext` fails fast at startup. Kernway is better here because circular dependencies are compile errors, not runtime errors.

---

## Level 5 — Fatal (App stops)

Situations that cannot be recovered from:

| Situation | Behavior |
|---|---|
| Out of Memory | OS kills the process — cannot be recovered |
| SIGKILL | Stops immediately |
| SIGTERM | **Graceful shutdown** — drain in-flight requests |
| All cores crash | App stops — log CRITICAL |
| Supervisor crash | App stops — nothing is left to monitor the cores |

**Graceful shutdown** (SIGTERM):

```
SIGTERM nhận được
│
├── Stop accepting new connections
├── Log: "Graceful shutdown initiated, draining requests..."
├── Wait for in-flight requests to complete
│   ├── Timeout: 30s (configurable)
│   └── Sau timeout: force close còn lại
├── Flush log buffers
├── Close DB connections
└── Exit 0
```

```toml
[shutdown]
timeout_secs     = 30     # đợi tối đa 30s cho in-flight requests
force_close_secs = 35     # force kill sau 35s
drain_log        = true   # flush log buffer trước khi exit
```

**Docker/Kubernetes**:
```yaml
# docker-compose.yml
services:
  my-app:
    stop_grace_period: 35s   # phải > shutdown.timeout_secs
    restart: unless-stopped  # auto restart nếu crash
```

**Spring comparison**: Spring Boot has `server.shutdown=graceful`. Kernway is equivalent, but it drains at the runtime level without depending on a servlet container.

---

## Circuit Breaker — Automatically opens when downstream fails

```rust
// Tránh cascade failure khi DB / external service lỗi liên tục
#[component]
struct PaymentService {
    #[inject] gateway: Arc<PaymentGateway>,
}

impl PaymentService {
    #[circuit_breaker(
        failure_threshold  = 5,     // mở circuit sau 5 lỗi liên tiếp
        timeout_secs       = 60,    // thử lại sau 60s
        fallback           = "payment_fallback"
    )]
    async fn charge(&self, amount: f64) -> Result<Receipt> {
        self.gateway.charge(amount).await
    }

    async fn payment_fallback(&self, amount: f64) -> Result<Receipt> {
        // Queue lại để xử lý sau
        Err(AppError::ServiceUnavailable("payment gateway down"))
    }
}
```

**Spring comparison**: Requires Resilience4j (external library). Built into Kernway.

---

## Health Check Endpoints

```
GET /health  → 200 OK nếu app đang chạy (liveness)
GET /ready   → 200 OK nếu app sẵn sàng nhận traffic (readiness)
```

```json
// GET /ready — chi tiết
{
  "status": "UP",
  "components": {
    "database":   { "status": "UP",   "response_ms": 2 },
    "cores":      { "status": "UP",   "alive": 4, "total": 4 },
    "disk_space": { "status": "UP",   "free_mb": 4096 },
    "memory":     { "status": "WARN", "used_percent": 85 }
  }
}

// Khi 1 core crashed chưa kịp restart:
{
  "status": "DEGRADED",
  "components": {
    "cores": { "status": "DEGRADED", "alive": 3, "total": 4 }
  }
}
```

**Kubernetes config**:
```yaml
livenessProbe:
  httpGet:
    path: /health
  failureThreshold: 3
  periodSeconds: 10

readinessProbe:
  httpGet:
    path: /ready
  failureThreshold: 1
  periodSeconds: 5
```

---

## Overall comparison

| Failure level | Spring | Kernway |
|---|---|---|
| Request error | `@ExceptionHandler` | `#[exception_handler]` |
| Handler panic | JVM catches, servlet OK | `catch_unwind` per task |
| Thread crash | Thread pool refill | Supervisor restarts core |
| All threads crash | App stalls (cannot accept requests) | Supervisor logs CRITICAL + alert |
| Startup error | ApplicationContext fail | Compile-time (DI) + runtime |
| Graceful shutdown | `server.shutdown=graceful` | `shutdown_timeout` + drain |
| Circuit breaker | Resilience4j (external) | `#[circuit_breaker]` built-in |
| Health check | Actuator (external dep) | Built-in `/health` `/ready` |
| Core isolation | ❌ shared thread pool | ✅ core crashes do not spread |
| Restart storm protection | ❌ | ✅ exponential backoff |
