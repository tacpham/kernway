# rt-core — Reactor & Executor

## Purpose

Custom async runtime: Reactor (I/O events) + Executor (task scheduling).  
Thread-per-core model: each OS thread has its own Reactor + Executor.

## Standards

- RFC 793 (TCP) — connection lifecycle
- Rust `core::future::Future` — task abstraction
- Rust `core::task::{Waker, RawWaker, RawWakerVTable}` — waker mechanism

## Architecture

```
Thread (core N)
│
├── Reactor
│   ├── mio::Poll        ← wraps epoll/kqueue/IOCP
│   ├── Interest registry (token → waker)
│   └── poll() → wake interested tasks
│
├── Executor
│   ├── VecDeque<Rc<Task>>   ← single-thread: Rc not Arc
│   ├── run_until_stalled()
│   └── run()                ← main loop: poll reactor, drain queue
│
└── Task
    ├── future: Pin<Box<dyn Future<Output=()>>>
    ├── waker: Waker          ← backed by Rc<Task>
    └── state: AtomicU8       ← IDLE | SCHEDULED | RUNNING
```

## Waker Implementation

```rust
// Custom Waker không dùng futures crate
static WAKER_VTABLE: RawWakerVTable = RawWakerVTable::new(
    clone_waker,
    wake_waker,
    wake_by_ref_waker,
    drop_waker,
);

fn make_waker(task: Rc<Task>) -> Waker {
    let ptr = Rc::into_raw(task) as *const ();
    unsafe { Waker::from_raw(RawWaker::new(ptr, &WAKER_VTABLE)) }
}
```

## Executor

```rust
pub struct Executor {
    queue: VecDeque<Rc<Task>>,
    reactor: Rc<RefCell<Reactor>>,
}

impl Executor {
    /// Main event loop — chạy cho đến khi không còn task nào
    pub fn run(&mut self) {
        loop {
            // 1. Drain ready tasks
            while let Some(task) = self.queue.pop_front() {
                task.poll_once();
            }
            // 2. Poll reactor for I/O events — wake relevant tasks
            self.reactor.borrow_mut().poll(Duration::from_millis(1));
            // 3. If nothing left, exit
            if self.queue.is_empty() {
                break;
            }
        }
    }
}
```

## sys/ Layer

```
rt-core/src/sys/
├── mod.rs       pub fn pin_current_thread_to_core(core_id: usize) -> io::Result<()>
├── linux.rs     sched_setaffinity()
├── macos.rs     pthread_mach_thread_np + thread_policy_set()
└── windows.rs   SetThreadAffinityMask()
```

**Rule**: `#[cfg(target_os = ...)]` must only appear in `sys/`.

## spawn_blocking

```rust
/// Run blocking work on a dedicated thread pool.
/// Result returned via channel to the async executor.
/// Pattern: same as Java's CompletableFuture.supplyAsync(executor)
pub fn spawn_blocking<F, T>(f: F) -> impl Future<Output = T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
```

Use for: diesel DB queries, blocking file I/O, and CPU-heavy computation.
