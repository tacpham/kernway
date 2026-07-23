# rt-net — TCP Network Layer

## Purpose

Async TCP stream + shard bootstrap (per-core listener setup).

## Standards

- RFC 793 (TCP) — connection establishment, data transfer, teardown
- RFC 9110 §7.3 — connection reuse

## AsyncTcpStream

```rust
/// Async TCP stream backed by mio::net::TcpStream + Reactor registration.
pub struct AsyncTcpStream {
    inner: mio::net::TcpStream,
    token: Token,
    reactor: Rc<RefCell<Reactor>>,
}

impl AsyncTcpStream {
    pub async fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> { ... }
    pub async fn write(&mut self, buf: &[u8]) -> io::Result<usize> { ... }
    pub async fn flush(&mut self) -> io::Result<()> { ... }
    pub async fn shutdown(&mut self) -> io::Result<()> { ... }
}
```

## Shard Bootstrap

```
Platform        Strategy                 What it buys
──────────      ─────────────────────    ──────────────────────────────
Linux           SO_REUSEPORT             Kernel distributes connections
macOS           SO_REUSEPORT             Kernel distributes connections
Windows         Shared socket + IOCP     IOCP dispatch completions

Result: every core accepts and handles connections independently — no shared queue
```

```rust
pub struct ShardConfig {
    pub addr: SocketAddr,
    pub num_shards: usize,    // default: available_parallelism()
    pub backlog: i32,         // default: 1024
}

pub fn bootstrap_shards(config: ShardConfig) -> Vec<TcpListener> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    { bootstrap_reuseport(config) }
    #[cfg(target_os = "windows")]
    { bootstrap_shared(config) }
}
```

## Connection Accept Loop

```rust
// Each shard runs on one thread
pub async fn accept_loop(listener: TcpListener, reactor: Rc<RefCell<Reactor>>) {
    loop {
        let stream = AsyncTcpListener::from(listener).accept().await?;
        // Spawn the task on the same executor — never migrates to another thread
        spawn_local(handle_connection(stream));
    }
}
```
