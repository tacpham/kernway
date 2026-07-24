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
Reproduce cargo bench -p di-core -p kernway-http -p kernway-server -p rt-core -p kernway-static -p kernway-htmx
```

Absolute nanoseconds are specific to this machine. What carries across machines
is the **shape** — O(1) vs O(n), the ratio between two approaches — and that is
what the analysis below leans on. Re-run on your own hardware before quoting an
absolute figure elsewhere.

## Routing — a radix trie, optimised against the incumbent

`cargo bench -p kernway-server` and `--bench vs_matchit`

The router is a segment radix trie, tuned across three rounds against `matchit`
(axum's) under [KEP-0000 §2]'s loop: measure, compare, optimise, repeat. The
internal numbers, now flat in route count for *every* class:

| Benchmark | 4 routes | 102 routes | Shape |
|---|---|---|---|
| `route/static_hit` | 21 ns | 21 ns | flat — O(path length) |
| `route/param_hit` | 151 ns | 153 ns | **flat** — was O(n), now O(path) |
| `route/miss` | 12 ns | 12 ns | **flat** — was O(n) |

Against `matchit`, same table and machine:

| | kernway | matchit | kernway is |
|---|---|---|---|
| static hit, 22 routes | 21 ns | 14.3 ns | 1.5× slower |
| static hit, 102 routes | 21 ns | 14.4 ns | 1.5× slower |
| param hit, 22 routes | 156 ns | 27 ns | 5.8× slower |
| param hit, 102 routes | 156 ns | 27 ns | 5.8× slower |

### What the loop bought, and where it stopped

The starting point (a hash map plus a linear scan) was **2.9× slower on static
and up to 77× on param, widening without bound**. Three rounds closed most of it:

1. **Radix trie** replaced the linear scan. Param routing went from O(n) —
   2.22 µs at 102 routes — to O(path), a flat 226 ns. A miss went from 1.87 µs to
   the same order. The gap that grew without bound now does not grow at all.
2. **Walk the path string directly**, no `Vec<&str>` of segments. Static dropped
   52 → 30 ns and became allocation-free.
3. **FNV-1a for the trie's static children** (SipHash is DoS-resistant and slow,
   and a router faces no adversarial keys), and the parameter map filled in place
   rather than collected from a `Vec`. Static 30 → 21 ns, param 186 → 156 ns.

Net: **static went 41 → 21 ns and is now within 1.5× of matchit — competitive;
param went 2.22 µs → 156 ns at a hundred routes, a 14× improvement, and is flat.**

Static is where we set out to be. Param is still 5.8× behind, and the remaining
gap is not the trie — it is the API. `find` returns an **owned**
`HashMap<String, String>`, so each parameter costs two `String` allocations
(`name` and value); matchit returns borrowed slices and allocates nothing. Closing
this means returning borrowed parameters, which changes `Request.path_params` and
every handler signature — an API decision that deserves its own KEP, not a quiet
tweak. Recorded as the loop's next target, with the reason it stopped here.

[KEP-0000 §2]: ../kep/0000-principles.md#2-fast--measured-or-it-is-not-a-claim

**Allocation**: a static match allocates nothing (the returned map is empty and
`HashMap::new` does not allocate until an insert). A param match allocates the
map plus two `String`s per parameter — the target above.

## The full request pipeline — every module together

`cargo bench -p kernway-server --bench pipeline`

The number that matters most, and the one every module below adds up to:
one request start to finish, in process — `kernway-http` parse → `kernway-server`
route → the handler builds a `kernway-core` `Response` → `kernway-http` encode.
No socket, no file I/O, so it is the CPU floor every request pays regardless of
the network.

| Benchmark | Time | What it exercises |
|---|---|---|
| `pipeline/static_get` | 352 ns | parse + static route + handler + encode |
| `pipeline/param_get` | 598 ns | parse (with headers) + param route + param map + JSON build + encode |

352 ns end to end for a static-route request is ~2.8 M requests/sec/core of pure
CPU headroom — the ceiling a real deployment works down from once the network,
the syscalls, and the scheduler are added. `param_get` costs ~1.6× more: a
browser request with more headers to parse, a parameter map to allocate, and a
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

## Precompressed static — the payload win, and its honest cost

`cargo bench -p kernway-static -- negotiate`, plus measured payload sizes.

With `.precompressed()` on, the server serves a `.br`/`.gz` sitting next to a
compressible file. Two things to measure: the CPU it adds per request, and the
bytes it removes from the wire — the second is the whole point.

**Payload** (`examples/web-docker/public/style.css`, a real stylesheet):

| Variant | Bytes | vs identity |
|---|---|---|
| identity | 1055 | — |
| gzip `-9` | 531 | **−50%** |
| brotli `-9` | 479 | **−55%** |

**CPU added on the request path** — the pure negotiation only; the file read is
the same either way:

| Step | Time | Runs |
|---|---|---|
| `negotiate/accept_encoding` (parse `Accept-Encoding`) | 180 ns | once per compressible asset, when precompression is on |
| `negotiate/is_compressible_binary` (skip gate) | 9.7 ns | once per asset — returns early for `image/*`, fonts, PDFs |
| `negotiate/is_compressible_text` | 13 ns | — |

Plus one or two `canonicalize`/`stat` syscalls to find the variant — I/O in the
hundreds of nanoseconds to low microseconds, dwarfing the parse. Those only
happen for a compressible type on a precompressed root; the `is_compressible`
gate (~10 ns) is what keeps a `.png` request from paying any of it.

### What this does and does not prove

It **does** prove the mechanism is cheap where it is CPU (≈200 ns of parsing to
shave 50–55% off the transferred bytes) and that the cost is skipped for media
that would not benefit. It **does not** make Kernway faster than another
framework: precompressed static is table stakes — nginx (`gzip_static`), axum
(`tower-http`), and every CDN do the same thing, and in many deployments the
compression happens at the edge, not the app. The honest framing (KEP-0000 §2):
this is a **bandwidth** feature Kernway now has, done the disciplined way — zero
per-request compression CPU (the `.br`/`.gz` is built ahead of time), the
already-compressed tier skipped, and `Vary: Accept-Encoding` set consistently so
a shared cache stays correct. It is parity with the incumbents on a feature that
matters, not an edge over them.

## htmx handling — head-to-head against the incumbent

`cargo bench -p kernway-htmx`

htmx support is a header layer: read the `HX-*` request headers, write the
`HX-*` response headers. So it can be measured against exactly what a developer
uses on axum today — `axum-htmx` 0.8, the dedicated crate — and against the raw
`http::HeaderMap` you write without any helper. All three do the *same* work per
round, on the same 8-header request profile the codec benches use. axum-htmx's
extractors are `async fn`; they are driven to completion with a no-op waker, so
the figure is the extractor's own work, not an executor's.

| Benchmark | kernway | axum-htmx | http substrate | kernway is |
|---|---|---|---|---|
| `extract` — read `is_request` + `boosted` + `target` + `trigger` | **57.8 ns** | 80.2 ns | 85.1 ns | **1.39× faster** than axum-htmx |
| `respond` — body + content-type + 3 `HX-*` + `Vary` | **240.9 ns** | — | 280.4 ns | **1.16× faster** than the substrate |
| `turn` — extract the request, build the reply | **176.7 ns** | 180.5 ns | — | ~2%, a tie |

The response row has no separate axum-htmx column because its responders *are*
`HeaderMap::insert` calls — the `http substrate` figure is what axum-htmx compiles
to, and the fair floor to beat.

### Where the win comes from, and where it does not

**Reading is the clear win — 1.39× over the dedicated crate.** Two reasons, both
from kernway-core: the one-buffer `Headers` scans a handful of short byte strings
instead of hashing each name (same structure the codec benches favour above),
and `target()`/`trigger()` return a borrowed `&str`. axum-htmx's `HxTarget` /
`HxTrigger` are `Option<String>` — they *allocate* the value out of the header on
every extraction. The kernway extractor allocates nothing.

**Writing is a narrower win — 1.16×.** Both sides allocate the HTML body `Vec`,
which dominates, so the header structure only moves the remainder; `Headers` edges
`HeaderMap` on a small set for the reasons the codec section already measured.

**Over a whole turn it is a tie (~2%),** and that is the honest headline: the
entire htmx layer — read the flags, pick fragment vs page, set the headers — costs
**~180 ns**, next to the ~350–600 ns the request pipeline already spends parsing,
routing, and encoding. htmx handling is not where a request is won or lost on
either framework. What kernway-htmx buys is not throughput; it is a typed,
allocation-free, correct-by-construction API (the `Vary: HX-Request` is automatic,
not a thing you must remember) that is also, measurably, never slower. Faster
where it can be, equal where the body allocation rules — never the bottleneck.

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
| `encode/small_json_close` | 60 ns |
| `encode/eight_headers` | 525 ns |

Parsing a realistic browser request with eight headers takes ~705 ns; encoding a
small JSON response ~47 ns. `incomplete_head` is the partial-read path — the
parser returns "need more bytes" in 133 ns rather than failing, which is what
lets a connection accumulate a request across several reads.

Encoding briefly regressed to 75 ns when `Body` split the head from the body
(KEP-0002) — the head sized its own buffer, then the body was appended, risking a
realloc between them. Fixed by sizing one buffer for head+body up front
(`encode_head_into`), back to ~47 ns.

### Headers: HashMap vs the one-buffer structure — measured both ways

`Response.headers` was a `HashMap<String, String>`; it is now the one-buffer
`Headers`. The first attempt at this **regressed** (75 → 103 ns) because the
encoder walked the headers twice — once to size the buffer, once to write. The
fix is to size in O(1): `Headers::byte_len()` is the buffer length, so the head
size is that plus a fixed per-pair overhead, no walk. With that, measured both
structures on the same responses:

| encode | HashMap | Headers (O(1) size) | winner |
|---|---|---|---|
| 1 header (JSON API) | 46.7 ns | 59.6 ns | HashMap by 13 ns |
| 8 headers (static/secured page) | 885 ns | 525 ns | **Headers by 360 ns (1.7×)** |
| `pipeline/static_get` (1–2 hdr) | 388 ns | 381 ns | tie |

**Neither wins outright — the crossover is ~3–4 headers.** HashMap is faster for
one or two (a single hash, one bucket); `Headers` is faster for many, and the
gap there is far larger (360 ns vs 13 ns) because it sizes in O(1), allocates one
buffer instead of two-per-pair, and writes once. Kernway serves static files
(5 headers) and secured pages (8+), so the many-header case is the common one and
the one that matters. `Headers` adopted.

The 13 ns the JSON-API case gives up is one extra allocation (`Headers` grows two
`Vec`s from empty on the first insert; `HashMap` grows one).

**A small-buffer optimisation was tried to close it, and reverted.** Making
`Fields` inline its bytes (64) and entries (5) — no heap for small sets — did
help the response side (`pipeline/static_get` 383 → 334 ns, `encode/eight_headers`
525 → 431 ns, fewer reallocs). But it *regressed the request side*: parsing an
8-header browser request went 680 → 758 ns (+11%), because a set that big spills
to the heap anyway *and* now pays to copy the larger inline-carrying struct
around. Netted over a browser round trip (parse a big request, encode a small
response) the loss outweighed the gain. The inline array's move cost ate the
allocation it saved — the trade-off `SmallVec` always risks, measured here rather
than assumed. Kept the two-`Vec` `Headers`; the 13 ns JSON-API gap stands, and
SSO is not the way to close it.

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
