# Kernway — Cross-Platform Support

## Summary

| Platform | Status | Notes |
|---|---|---|
| Linux x86_64 | ✅ First-class | Production target |
| Linux ARM64 | ✅ First-class | Raspberry Pi, AWS Graviton |
| macOS (ARM M-chip) | ✅ Full support | Dev environment |
| macOS (Intel) | ✅ Full support | Dev environment |
| Windows 10/11 | ✅ Full support | IOCP native via mio 0.8+ |
| Linux musl | ✅ Static binary | Docker scratch image |

---

## Issues and solutions

### 1. Connection Distribution (instead of SO_REUSEPORT)

**Issue**: `SO_REUSEPORT` is not available on Windows.

**Solution**: Shared socket + N threads, with each thread calling `AcceptAsync`.

```
Linux/macOS:                    Windows:
                                
Core 0 → Socket 0 (REUSEPORT)  Core 0 ──┐
Core 1 → Socket 1 (REUSEPORT)  Core 1 ──┤──→ Shared Socket → IOCP dispatches
Core 2 → Socket 2 (REUSEPORT)  Core 2 ──┤     completions to any waiting thread
Core 3 → Socket 3 (REUSEPORT)  Core 3 ──┘
```

Equivalent outcome: each thread accepts connections and handles them independently. Overhead < 2% (this is exactly how Kestrel — the ASP.NET Core server — works).

**Implementation** (`rt-net/src/shard.rs`):

```rust
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn bind_shard(addr: SocketAddr, _shard_id: usize) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None)?;
    socket.set_reuse_port(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(TcpListener::from(socket))
}

#[cfg(target_os = "windows")]
fn bind_shared(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None)?;
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(TcpListener::from(socket))  // shared across IOCP threads
}
```

---

### 2. CPU Affinity

**Issue**: CPU pinning APIs differ by platform.

| Platform | API | Result |
|---|---|---|
| Linux | `sched_setaffinity()` | Hard pin — the kernel does not migrate the thread |
| macOS | `pthread_mach_thread_np` + thread_policy | Hint only — the macOS scheduler may override it |
| Windows | `SetThreadAffinityMask()` | Hard pin — equivalent to Linux |

**Acceptable because**:
- macOS = development environment, not production
- Thread-per-core has 2 benefits: (1) cache locality (requires pinning), (2) zero lock contention (does not require pinning)
- Benefit #2 works on all platforms

**Implementation** (`rt-core/src/sys/`):

```rust
// rt-core/src/sys/mod.rs — public API
pub fn pin_current_thread_to_core(core_id: usize) -> io::Result<()> {
    sys_impl::pin_current_thread_to_core(core_id)
}

// Three implementations — the compiler picks the right one
// rt-core/src/sys/linux.rs
// rt-core/src/sys/macos.rs
// rt-core/src/sys/windows.rs
```

---

### 3. Core Count in Docker/Containers

**Issue**: The `num_cpus` crate reads `/proc/cpuinfo` — it returns the host CPU count, not the container limit.

**Solution**: `std::thread::available_parallelism()` — container-aware since Rust 1.73.

```rust
let num_cores = std::thread::available_parallelism()
    .map(|n| n.get())
    .unwrap_or(1);

// In Docker with --cpus=2: returns 2 (correct)
// On bare metal with 16 cores: returns 16 (correct)
```

---

### 4. Signal Handling

**Issue**: `SIGTERM`/`SIGINT` are Unix signals and do not exist on Windows.

**Solution**: A unified graceful shutdown API.

```rust
// Kernway internal — platform-specific
#[cfg(unix)]
async fn wait_for_shutdown() {
    use libc::{SIGINT, SIGTERM};
    // wait for SIGINT or SIGTERM
}

#[cfg(windows)]
async fn wait_for_shutdown() {
    // SetConsoleCtrlHandler for Ctrl+C
    // Service Control Manager for SCM stop
}

// User-facing API — cross-platform
KernwayApp::builder()
    .shutdown_timeout(Duration::from_secs(30))
    .build()
    .run()
    .await
```

---

### 5. IOCP vs epoll

**Initial concern**: It was assumed that mio on Windows used a wepoll emulation layer.

**Reality**: mio 0.8+ uses **native IOCP** — there is no emulation.

- mio on Linux: `epoll`
- mio on macOS: `kqueue`
- mio on Windows: `IOCP` (completion-based, native Windows API)

Kernway code only calls `mio::Poll` — mio handles the rest. No platform-specific code is needed for I/O event polling.

---

## sys/ Directory Convention

Each crate with platform-specific I/O code should follow this pattern:

```
crate/src/
├── sys/
│   ├── mod.rs       // public API + #[cfg] dispatch
│   ├── linux.rs     // epoll + SO_REUSEPORT + sched_setaffinity
│   ├── macos.rs     // kqueue + SO_REUSEPORT + thread policy
│   └── windows.rs   // IOCP + shared socket + SetThreadAffinityMask
├── reactor.rs       // uses sys/ via mod.rs — no #[cfg] here
└── ...
```

**Rule**: `#[cfg(target_os = ...)]` may only appear in `sys/mod.rs` and `sys/*.rs`.  
There must be no `#[cfg]` in business logic files.

---

## Cross-platform testing goals

CI matrix (GitHub Actions):

```yaml
strategy:
  matrix:
    os: [ubuntu-latest, macos-latest, windows-latest]
    target:
      - x86_64-unknown-linux-gnu
      - aarch64-unknown-linux-gnu     # cross-compile
      - x86_64-unknown-linux-musl     # static binary
      - x86_64-apple-darwin
      - aarch64-apple-darwin
      - x86_64-pc-windows-msvc
```

Each platform: unit tests + echo server integration test + HTTP request/response roundtrip.
