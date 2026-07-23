# Kernway — Fault Tolerance

> Kernway is designed to **never stop because of a single failure**. Each failure level has its own handling mechanism — only truly fatal failures stop the app.

---

## 5-Level Overview

```
Level 1 — Request Error   That request fails  App never notices     ✅ Self-handled
Level 2 — Handler Panic   HTTP 500            Core thread continues ✅ catch_unwind
Level 3 — Core Crash      ~10ms downtime      Supervisor restart    ✅ Auto-recover
Level 4 — Startup Error   App will not start  Caught early          ⚠️  Fix & restart
Level 5 — Fatal           App stops           OOM / all cores down  ❌ Docker restart
```

---

## Level 1 — Request Error

**Impact**: Only that request gets an error. The app is unaffected. Zero impact.

```rust
// Ordinary errors travel through Result — #[exception_handler] catches them and returns the HTTP response
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

**Impact**: That request gets HTTP 500. The core thread **keeps running**.

**Rust issue**: By default, panic unwinds → the thread dies.

**Kernway solution**: `catch_unwind` wraps each task in the executor:

```rust
// rt-core/src/executor.rs — MUST be implemented exactly like this
impl Executor {
    fn poll_task(&self, task: Rc<Task>) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            task.poll()
        }));

        match result {
            Ok(_) => {}
            Err(panic_payload) => {
                // 1. Log the panic with a stack trace
                let msg = panic_message(&panic_payload);
                log::error!(
                    "handler panic: {msg}",
                    request_id = task.request_id(),
                    path       = task.path(),
                );

                // 2. Return 500 for that request
                task.send_error_response(StatusCode::INTERNAL_SERVER_ERROR);

                // 3. The core thread carries on — no crash
            }
        }
    }
}
```

**Behavior**:
```
Request A: GET /users/1   → being handled normally
Request B: GET /users/2   → handler panic!
Request C: GET /users/3   → being handled normally

Result:
  Request A → 200 OK
  Request B → 500 Internal Server Error  (the panic was caught)
  Request C → 200 OK
  Core thread → keeps running, unaffected
```

**Spring comparison**: The JVM catches exceptions in the servlet container, and the thread returns to the pool. Similar behavior — but Kernway does this at the runtime layer and does not depend on the servlet container.

---

## Level 3 — Core Thread Crash

**Impact**: In-flight requests on that core get a connection reset. The core restarts in ~10ms.

This happens when a panic occurs in reactor/executor code (a framework bug), not in handler code.

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

                    // Wait a moment before restarting
                    // (avoids a restart loop when the bug is persistent)
                    std::thread::sleep(Duration::from_millis(100));

                    // Spawn a fresh core
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
Crash 1 → restart after 100ms
Crash 2 → restart after 500ms
Crash 3 → restart after 2s
Crash 4 → restart after 10s + alert log
Crash 5 → NO restart, log CRITICAL, reduce capacity
```

**Config**:
```toml
[supervisor]
restart_delay_ms = 100          # delay before the first restart
max_restart_attempts = 5        # no restarts beyond this
backoff = "exponential"         # linear | exponential
alert_threshold = 3             # log CRITICAL after N crashes
```

**Spring comparison**: Spring has no supervisor mechanism. If the JVM thread pool is saturated, the whole app can hang. In Kernway, each core is isolated, so one core crashing does not affect the others.

---

## Level 4 — Startup Errors

The app **does not start** — early detection is better than crashing while serving traffic.

| Startup error | Behavior | Config |
|---|---|---|
| Port already in use | Fail fast — clearly log which port | Cannot be overridden |
| DB connection error | Configurable | `db.startup_check` |
| Missing required env var | Fail fast — clearly log which variable is missing | Cannot be overridden |
| Config file parse error | Fail fast — clearly log which line is invalid | Cannot be overridden |
| Bean circular dependency | **Compile error** — does not wait until runtime | — |
| Pending migrations | Configurable | `db.migrate_on_start` |

```toml
# config/application.toml
[startup]
fail_fast = true              # false = warn and continue (not recommended)

[db]
startup_check     = "retry"   # fail_fast | retry | skip
retry_attempts    = 5
retry_delay_secs  = 2
migrate_on_start  = true      # run pending migrations before accepting traffic
```

**Startup sequence**:
```
1. Parse config → fail on error
2. Validate env vars → fail if a required one is missing
3. Bootstrap DI graph → fail on a circular dep (already caught at compile time)
4. Connect DB pool → retry per config
5. Run migrations → fail on a migration error
6. Bind port → fail if it is already taken
7. Start supervisor + cores
8. Ready — start accepting traffic
```

**Spring comparison**: Spring is similar — `ApplicationContext` fails fast at startup. Kernway is better here because circular dependencies are compile errors, not runtime errors.

---

## Level 5 — Fatal (App Stops)

Situations that cannot be recovered from:

| Scenario | Behavior |
|---|---|
| Out of Memory | OS kills the process — unrecoverable |
| SIGKILL | Stops immediately |
| SIGTERM | **Graceful shutdown** — drains in-flight requests |
| All cores crash | App stops — log CRITICAL |
| Supervisor crash | App stops — nothing is left to monitor the cores |

**Graceful shutdown** (SIGTERM):

```
SIGTERM received
│
├── Stop accepting new connections
├── Log: "Graceful shutdown initiated, draining requests..."
├── Wait for in-flight requests to complete
│   ├── Timeout: 30s (configurable)
│   └── After the timeout: force-close the rest
├── Flush log buffers
├── Close DB connections
└── Exit 0
```

```toml
[shutdown]
timeout_secs     = 30     # wait at most 30s for in-flight requests
force_close_secs = 35     # force kill sau 35s
drain_log        = true   # flush the log buffer before exiting
```

**Docker/Kubernetes**:
```yaml
# docker-compose.yml
services:
  my-app:
    stop_grace_period: 35s   # must be > shutdown.timeout_secs
    restart: unless-stopped  # auto restart on crash
```

**Spring comparison**: Spring Boot has `server.shutdown=graceful`. Kernway is equivalent, but draining happens at the runtime level and does not depend on the servlet container.

---

## Circuit Breaker — Automatically Opens on Downstream Failure

```rust
// Avoids cascading failure when the DB / an external service keeps failing
#[component]
struct PaymentService {
    #[inject] gateway: Arc<PaymentGateway>,
}

impl PaymentService {
    #[circuit_breaker(
        failure_threshold  = 5,     // open the circuit after 5 consecutive failures
        timeout_secs       = 60,    // retry after 60s
        fallback           = "payment_fallback"
    )]
    async fn charge(&self, amount: f64) -> Result<Receipt> {
        self.gateway.charge(amount).await
    }

    async fn payment_fallback(&self, amount: f64) -> Result<Receipt> {
        // Queue it for later processing
        Err(AppError::ServiceUnavailable("payment gateway down"))
    }
}
```

**Spring comparison**: Requires Resilience4j (external library). Kernway has it built in.

---

## Health Check Endpoints

```
GET /health  → 200 OK while the app is running (liveness)
GET /ready   → 200 OK once the app can accept traffic (readiness)
```

```json
// GET /ready — the details
{
  "status": "UP",
  "components": {
    "database":   { "status": "UP",   "response_ms": 2 },
    "cores":      { "status": "UP",   "alive": 4, "total": 4 },
    "disk_space": { "status": "UP",   "free_mb": 4096 },
    "memory":     { "status": "WARN", "used_percent": 85 }
  }
}

// While a core has crashed and not yet restarted:
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

## Overall Comparison

| Failure level | Spring | Kernway |
|---|---|---|
| Request error | `@ExceptionHandler` | `#[exception_handler]` |
| Handler panic | JVM catches, servlet OK | `catch_unwind` per task |
| Thread crash | Thread pool refill | Supervisor restarts core |
| All threads crash | App hangs (does not accept requests) | Supervisor logs CRITICAL + alert |
| Startup error | `ApplicationContext` fails | Compile-time (DI) + runtime |
| Graceful shutdown | `server.shutdown=graceful` | `shutdown_timeout` + drain |
| Circuit breaker | Resilience4j (external) | `#[circuit_breaker]` built-in |
| Health check | Actuator (external dependency) | Built-in `/health` `/ready` |
| Core isolation | ❌ shared thread pool | ✅ core crashes do not spread |
| Restart storm protection | ❌ | ✅ exponential backoff |
