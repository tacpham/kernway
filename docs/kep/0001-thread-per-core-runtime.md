---
kep: 0001
title: Thread-per-core runtime instead of work-stealing
status: Accepted
created: 2026-07-23
decided: 2026-07-23
---

# KEP-0001: Thread-per-core runtime instead of work-stealing

> Backfilled. The decision predates this process; see the note in
> [README](README.md#a-note-on-the-first-four).

## Summary

Kernway runs its own async runtime (`rt-core` + `rt-net`) in which each OS thread
owns one reactor, one executor, and one task queue. A task is created on a core
and finishes on that core. There is no shared queue, no work stealing, and no
lock on the request path.

This rules out using tokio as the runtime.

## Motivation

Work-stealing optimises for **throughput under uneven task duration**: an idle
worker takes work from a busy one, so no core sits still while another has a
backlog. That is the right trade for a general-purpose runtime, and tokio is
excellent at it.

A web framework's workload is not that shape. Requests are numerous, short, and
roughly uniform, and they arrive already parallel — one per connection. There is
little imbalance left for stealing to correct, so the machinery mostly costs
rather than pays:

- **Task migration invalidates cache.** A task that starts on core 0 and resumes
  on core 3 leaves its connection buffer, its parsed headers, and its handler
  state in the wrong L1/L2. The work is redone in cache misses.
- **A shared queue needs synchronisation.** Every spawn and every steal touches
  memory other cores also touch, on the hot path, at request rate.
- **Request-scoped state has to be `Send`.** Anything that might migrate must be
  safe to move across threads, which pushes `Arc` and `Mutex` into places that
  would otherwise need neither.

The last point is the one that compounds. It is not a constant factor; it changes
what you are allowed to write.

Expected outcome: p99 and p999 latency that is flat rather than spiky, and a
request path with no atomics on it.

## Guide-level explanation

You do not configure this; it is what the server is.

```text
Core 0: [Reactor] ←→ [Executor] ←→ [Task A, Task B, Task C ...]
Core 1: [Reactor] ←→ [Executor] ←→ [Task D, Task E, Task F ...]
Core 2: [Reactor] ←→ [Executor] ←→ [Task G, Task H, Task I ...]
Core 3: [Reactor] ←→ [Executor] ←→ [Task J, Task K, Task L ...]
```

Each core accepts its own connections and handles them to completion. Two
consequences reach user code:

**Request-local state does not need to be thread-safe.** A value that lives for
one request stays on one thread, so a plain `RefCell` is sound where tokio would
have required a `Mutex`. Logging context (MDC) is the everyday case: it is set at
the start of a request and still there at the end, with no copying, because
nothing moved.

**A blocking call must be moved off the core deliberately.** There is no other
worker to pick up the slack — a blocking call stalls every task on that core, not
just the caller. Use `spawn_blocking`, which is why the ORM and cache specs are
synchronous ([KEP-0004](0004-no-lazy-loading.md) touches the same reasoning).

## Reference-level explanation

**Per-core loop.** Each shard owns a `Reactor` (a `mio::Poll`) and an `Executor`
with a `VecDeque` of ready tasks. The loop drains ready tasks, then polls for I/O
readiness, then repeats.

**Task ownership.** A task's future lives in an `Rc<Task>` in the owning shard's
slab and never leaves its thread. The future therefore need not be `Send`.

The waker is the subtle part, and it is where the implementation departs from the
original design note. A `Waker` is `Send + Sync` by definition — a
`spawn_blocking` worker or a timer thread can hold one — so the waker payload
cannot be the `Rc<Task>` itself, or its refcount would race. Instead the payload
is an `Arc<WakeHandle>` carrying a `TaskId` plus a handle to the shard's shared
queue. Waking enqueues the id and unparks the reactor; the task itself is only
ever touched by its own thread.

**Connection distribution** is the platform-specific part:

| Platform | Mechanism | Notes |
|---|---|---|
| Linux | `SO_REUSEPORT`, one listener per core | The kernel balances across sockets |
| macOS | `SO_REUSEPORT`, one listener per core | Permits the shared bind; does **not** balance |
| Windows | One shared socket, N threads in `AcceptEx` | IOCP distributes completions |

Only Linux actually balances. macOS and Windows still get the property that
matters — a connection, once accepted, is handled entirely by one thread — but
the distribution across cores is less even.

**Where the sharing is.** `spawn_blocking` results and timers cross threads, by
construction. Those are the only cross-thread paths, and none of them is on the
per-request hot path.

## Drawbacks

**A genuinely imbalanced workload will lose to tokio.** If one request takes 100×
another and they land on the same core, nothing rescues the short ones. Work
stealing exists for exactly this, and where it applies it wins. An application
with a few very expensive endpoints mixed into cheap traffic is not this
runtime's best case.

**We maintain a runtime.** tokio is battle-tested across an enormous number of
deployments; `rt-core` is not. Every bug in the executor, the waker vtable, or
the reactor is now ours, including the ones that only appear under load we have
not generated. The waker correction described above is an example of the class:
a design that read fine on paper and was unsound in practice.

**The tokio ecosystem does not come along.** `sqlx`, `reqwest`, `tonic` and most
async libraries assume a tokio runtime. Reaching them means `spawn_blocking` and
a sync bridge, or reimplementation. This is a large, ongoing cost, and it is the
strongest argument against this KEP.

**Two of three platforms distribute imperfectly.** The benefit is real
everywhere; the even distribution is not.

## Rationale and alternatives

**tokio with `LocalSet`.** tokio can pin `!Send` tasks per thread, which gets
some of the locality without maintaining a runtime. Rejected because the
multi-threaded scheduler is still underneath — the shared queue, the stealing
machinery, and the `Send` bounds on everything that touches it remain. It is
locality bolted onto a design that assumes migration, not a design without it.

**glommio.** A real thread-per-core Rust runtime, and closest to what this KEP
describes. Rejected on portability: it is `io_uring`-only, therefore Linux-only
and recent-kernel-only. Kernway targets Linux, macOS, and Windows.

**monoio.** Same model, same `io_uring` bias, same conclusion, with a smaller
ecosystem than glommio.

**Do nothing — use tokio as-is.** The honest default, and it would have been
faster to ship. Rejected because the `Send` requirement leaks into the framework's
public API: request-scoped state, logging context, and handler signatures are all
shaped by whether a task can migrate. That is not a decision that can be revisited
later without changing everything above it — which is precisely why it is a KEP.

## Prior art

The model is well established outside Rust, and Kernway is not inventing it:

- **Seastar** (ScyllaDB) — shared-nothing per core, the canonical statement of
  the approach.
- **Nginx** — one process per core, `SO_REUSEPORT`, no shared connection queue.
- **Kestrel** (ASP.NET Core) — per-core connection dispatch on all three of our
  target platforms, including Windows via IOCP. Direct evidence that the Windows
  story is workable rather than theoretical.
- **Node.js cluster** — one process per core, kernel-balanced.

Within Rust, glommio and monoio demonstrate the model and its portability
constraints; `rt-core` takes the model without the `io_uring` dependency.

## Unresolved questions

- **We have not measured our own claim.** The `ARCHITECTURE.md` table asserts a
  20–50% p999 improvement; that number is inherited from the literature, not from
  a Kernway benchmark. `examples/echo-server` exists but has not been run against
  tokio. Until it has, the table is a hypothesis.
- macOS distribution: is per-core accept without kernel balancing good enough
  under real load, or does it need an explicit hand-off?
- What is the least-bad bridge to the tokio ecosystem — `spawn_blocking` for
  everything, or a compatibility shim?

## Future possibilities

- Optional CPU pinning, for the deployments where it pays.
- An `io_uring` reactor on Linux, alongside the `mio` one — the shard model does
  not care which readiness mechanism is underneath.
- Per-core metrics, which are almost free in this model and awkward in a
  work-stealing one: queue depth per core is a real number here.
