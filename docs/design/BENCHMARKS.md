# Kernway — measured performance

The single source of truth for every performance number Kernway states. If a
figure appears in the README, a charter, or a KEP, it is quoted from here — so
there is one place to update and one place that can be wrong.

The rule is [KEP-0000 §2](../kep/0000-principles.md#2-fast--measured-or-it-is-not-a-claim):
a number comes from a benchmark or it is labelled a hypothesis. This file holds
the measured half. What is *not* here — end-to-end throughput, latency under
load, any comparison against another framework's runtime — is not yet measured,
and is listed as such at the bottom rather than guessed.

## How these were taken

```
Machine   Apple M2 Max, 12 cores
OS        macOS 26.5
rustc     1.96.0, release profile
Tool      criterion (statistical, warmed, outlier-filtered)
Date      2026-07-24 (routing/DI/http/runtime); static + pipeline added same day
Reproduce cargo bench -p di-core -p kernway-http -p kernway-server -p rt-core -p kernway-static
```

Absolute nanoseconds are specific to this machine. What carries across machines
is the **shape** — O(1) vs O(n), the ratio between two approaches — and that is
what the analysis below leans on. Re-run on your own hardware before quoting an
absolute figure elsewhere.

## Routing — flat with application size

`cargo bench -p kernway-server`

| Benchmark | 4 routes | 102 routes | Shape |
|---|---|---|---|
| `route/static_hit` | 41.2 ns | 41.6 ns | **flat — O(1)** |
| `route/param_hit` | 331 ns | 2.17 µs | grows with dynamic-route count |
| `route/miss` | 80 ns | 1.87 µs | grows with dynamic-route count |

The first row is the headline, and it is an architectural claim backed by a
measurement: **a static route costs the same whether the application has 4
routes or 102.** 41.2 ns at four, 41.6 ns at a hundred-and-two — inside the noise
of each other.

That is the payoff of splitting the router in two. A pattern with no placeholder
goes into a hash map and costs one lookup; only patterns containing `{param}` are
walked. So the routes that dominate a real application — `/`, `/health`,
`/assets/...`, every page — do not get slower as it grows.

The contrast makes the point: at 102 routes a static hit is **~52× faster** than
a parameterised one (41.6 ns vs 2.17 µs), because one is a hash lookup and the
other is a linear scan. Both are fine; the design ensures the common case is the
fast one.

**Allocation**: zero for a matched static route, one `HashMap` only for a route
that actually has parameters.

## The full request pipeline — every module together

`cargo bench -p kernway-server --bench pipeline`

The number that matters most, and the one every module below adds up to:
one request start to finish, in process — `kernway-http` parse → `kernway-server`
route → the handler builds a `kernway-core` `Response` → `kernway-http` encode.
No socket, no file I/O, so it is the CPU floor every request pays regardless of
the network.

| Benchmark | Time | What it exercises |
|---|---|---|
| `pipeline/static_get` | 392 ns | parse + static route + handler + encode |
| `pipeline/param_get` | 768 ns | parse (with headers) + param route + param map + JSON build + encode |

392 ns end to end for a static-route request is ~2.5 M requests/sec/core of pure
CPU headroom — the ceiling a real deployment works down from once the network,
the syscalls, and the scheduler are added. `param_get` costs roughly double: a
browser request with headers to parse, a parameter map to allocate, and a
`format!`ed JSON body.

This is the guard the module benchmarks below serve: a regression in parsing,
routing, or encoding shows up here, in the number that actually ships.

**Not** an end-to-end throughput figure — that needs a load test against a
running server, listed under "Not yet measured".

## Static resolution — the per-request path, before any I/O

`cargo bench -p kernway-static`

Pure CPU: turning a URL into a safe file path and naming its MIME type. The file
read is I/O and is not here.

| Benchmark | Time | Notes |
|---|---|---|
| `resolve/plain` | 136 ns | `/assets/app.css` → a `PathBuf` under the root |
| `resolve/index` | 103 ns | `/` → `index.html` |
| `resolve/…traversal_rejected` | 80 ns | `%2e%2e` decoded and refused — the reject path is cheap |
| `resolve/deep` | 229 ns | 8 segments; cost grows with depth |
| `etag/build` | 117 ns | format the validator from len + mtime |
| `etag/matches_hit` | 14 ns | the 304 decision on a matching request |
| `etag/matches_in_list` | 56 ns | ours last in a 4-entry `If-None-Match` |
| `mime_for` | 30 ns | extension → type |

`resolve` allocates one `PathBuf` (the result), which is most of the ~136 ns —
it is the one place the static path is not allocation-free, and a candidate to
optimise if a large-file load test later shows it mattering. `etag_matches` at
14 ns means a conditional request's 304 decision is effectively free next to the
file `stat` it accompanies.

## DI resolution — bean lookup is nearly free

`cargo bench -p di-core`

| Benchmark | Time |
|---|---|
| `resolve/get_concrete` | 4.2 ns |
| `resolve/get_as_trait` | 6.6 ns |
| `resolve/get_missing` | 2.4 ns |
| `refresh/two_component_graph` | 371 ns |

A bean lookup on every `#[inject]` field costs ~4 ns. `refresh` — the topological
wiring of the whole graph — runs once at startup, so its 371 ns for a two-node
graph is a boot cost, not a per-request one.

### The hasher, measured

| Benchmark | Time |
|---|---|
| `siphash_17_lookups` | 152 ns |
| `passthrough_17_lookups` | 26 ns |

**5.8× faster.** The container keys on `TypeId`, which is already a
well-distributed 128-bit value, so hashing it again with SipHash is wasted work;
a pass-through hasher folds it to 64 bits and stops.

This benchmark exists because a comment in `context.rs` once claimed the win was
"~2×". It was a guess, and it was wrong — the real figure is ~6×. Both numbers
were assertions until someone measured, which is the whole reason
[KEP-0000 §2](../kep/0000-principles.md#2-fast--measured-or-it-is-not-a-claim)
exists.

## HTTP codec

`cargo bench -p kernway-http`

| Benchmark | Time |
|---|---|
| `parse/minimal_get` | 207 ns |
| `parse/browser_get_8_headers` | 705 ns |
| `parse/json_post_with_body` | 331 ns |
| `parse/incomplete_head` | 133 ns |
| `encode/small_json_close` | 44 ns |
| `encode/small_json_keep_alive` | 43 ns |

Parsing a realistic browser request with eight headers takes ~705 ns; encoding a
small JSON response ~44 ns. `incomplete_head` is the partial-read path — the
parser returns "need more bytes" in 133 ns rather than failing, which is what
lets a connection accumulate a request across several reads.

## Runtime — executor and scheduling

`cargo bench -p rt-core`

| Benchmark | Time | Per unit |
|---|---|---|
| `block_on/ready_future` | 22 ns | — |
| `spawn/10` | 2.40 µs | 240 ns/task |
| `spawn/1000` | 65.5 µs | **65 ns/task** |
| `wake_poll_cycle/1` | 47 ns | — |
| `wake_poll_cycle/1000` | 27.5 µs | 27 ns/wake |
| `timers/expired_sleep` | 53 ns | — |

Spawning scales linearly and cheaply: ~65 ns per task at a thousand tasks. A
wake-and-poll cycle is ~27 ns amortised. These are the operations the runtime
does on every connection and every readiness event, so their being tens of
nanoseconds is what keeps the per-request floor low.

## What Kernway is, architecturally

These are design differences, not benchmark results. Each is verifiable by
reading the code rather than by running it, so no number is attached — and none
is a claim that another framework is *worse*, only that Kernway is built
differently, with consequences a developer can reason about.

| Property | Common server model | Kernway |
|---|---|---|
| Scheduling | thread pool or work-stealing event loop | thread-per-core |
| Task migration between cores | yes | **never** |
| Lock on the request hot path | shared queue needs one | **none** |
| Request-scoped state | thread-safe container (`ThreadLocal`, async-local, or a reactive context) | a plain `Rc` / `RefCell` — the task never leaves its thread |
| Async runtime | typically a shared dependency | its own (`rt-core`), no tokio |
| Deployment artifact | varies | one static binary |

The row that a developer feels day to day is request-scoped state. Because a
Kernway task is pinned to its core for its whole life, per-request data needs no
synchronisation — it is an ordinary `Rc`. Logging context (MDC), a request id, a
current user: all of it is a plain value, not a special container.

Whether thread-per-core also delivers lower tail latency under load is a separate
question, and one this file cannot yet answer — see below.

## Not yet measured — do not quote these

Listed so nobody mistakes silence for a good result. Each is a real question with
no Kernway number behind it today.

| Claim | Status |
|---|---|
| Requests/sec end to end | **not measured** — needs a load test against a running server |
| p50 / p99 / p999 latency | **not measured** |
| Latency vs an equivalent tokio/axum app | **not measured** — `examples/echo-server` exists but has not been run in a comparison |
| Tail-latency benefit of thread-per-core | **not measured** — the mechanism is real; the magnitude on Kernway is unverified |
| Cold start, idle RSS | **not measured** — belongs to milestone M6 (Docker) |
| Binary size | **not measured** — M6 |
| Compile time per feature configuration | **not measured** — meta-crate charter, M-cross-cutting |

Until a row here has a number, any document that needs it says "not yet
measured", never a figure from memory or from another project's blog post.

## When this file changes

- A new hot path gets a benchmark → a row here, same day.
- A number moves by more than measurement noise → update here, and check who
  quoted it (`grep -rn` for the figure across `docs/` and `README.md`).
- A "not yet measured" row gets measured → move it up into the body.
