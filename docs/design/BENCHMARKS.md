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
Machine   Intel Core i7-1270P, 12 cores / 16 threads
OS        Linux (Debian Bookworm) inside Docker (rust:1-bookworm)
rustc     1.97.1 (8bab26f4f 2026-07-14), release profile
Tool      criterion (statistical, warmed, outlier-filtered)
Date      2026-07-27
Reproduce .\kw.ps1 bench        (Windows/Docker)
          ./kw.sh bench         (macOS/Linux/Docker)
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

| Benchmark | 4 routes | 22 routes | 102 routes | Shape |
|---|---|---|---|---|
| `route/static_hit` | 29.1 ns | 28.9 ns | 28.6 ns | flat |
| `route/param_hit` | 188.1 ns | 183.2 ns | 183.4 ns | flat |
| `route/miss` | 17.6 ns | 17.5 ns | 17.4 ns | flat |

Against `matchit`, same table and machine:

| | kernway | matchit | kernway is |
|---|---|---|---|
| static hit, 22 routes | 29.7 ns | 14.1 ns | **2.1× slower** |
| static hit, 102 routes | 29.4 ns | 13.8 ns | **2.1× slower** |
| param hit, 22 routes | 183.8 ns | 29.3 ns | **6.3× slower** |
| param hit, 102 routes | 182.8 ns | 29.2 ns | **6.3× slower** |

### What the loop bought, and where it stopped

The starting point (a hash map plus a linear scan) was **2.9× slower on static
and up to 77× on param, widening without bound**. Three rounds closed most of it:

1. **Radix trie** replaced the linear scan. Param routing went from O(n) —
   2.22 µs at 102 routes — to O(path), a flat 183 ns. A miss went from 1.87 µs to
   the same order. The gap that grew without bound now does not grow at all.
2. **Walk the path string directly**, no `Vec<&str>` of segments. Static dropped
   52 → 30 ns and became allocation-free.
3. **FNV-1a for the trie's static children** (SipHash is DoS-resistant and slow,
   and a router faces no adversarial keys), and the parameter map filled in place
   rather than collected from a `Vec`. Static 30 → 29 ns, param 186 → 183 ns.

Net: **static went 41 → 29 ns and is now within 2.1× of matchit — competitive;
param went 2.22 µs → 183 ns at a hundred routes, a 12× improvement, and is flat.**

Static is where we set out to be. Param is still 6.3× behind, and the remaining
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
| `pipeline/static_get` | 522.1 ns | parse + static route + handler + encode |
| `pipeline/param_get` | 850.6 ns | parse (with headers) + param route + param map + JSON build + encode |

522 ns end to end for a static-route request is ~1.9 M requests/sec/core of pure
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
| `resolve/plain` | 98.9 ns | `/assets/app.css` → a `PathBuf` under the root |
| `resolve/index` | 70.0 ns | `/` → `index.html` |
| `resolve/…traversal_rejected` | 80.2 ns | `%2e%2e` decoded and refused — the reject path is cheap |
| `resolve/deep` | 198.5 ns | 8 segments; cost grows with depth |
| `etag/build` | 101.5 ns | format the validator from len + mtime |
| `etag/matches_hit` | 19.7 ns | the 304 decision on a matching request |
| `etag/matches_in_list` | 68.3 ns | ours last in a 4-entry `If-None-Match` |
| `mime_for` | 36.5 ns | extension → type |

`resolve` allocates one `PathBuf` (the result), which is most of the ~99 ns —
it is the one place the static path is not allocation-free, and a candidate to
optimise if a large-file load test later shows it mattering. `etag_matches` at
~20 ns means a conditional request's 304 decision is effectively free next to the
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
| `negotiate/accept_encoding` (parse `Accept-Encoding`) | 202.5 ns | once per compressible asset, when precompression is on |
| `negotiate/is_compressible_binary` (skip gate) | 14.9 ns | once per asset — returns early for `image/*`, fonts, PDFs |
| `negotiate/is_compressible_text` | 18.7 ns | — |

Plus one or two `canonicalize`/`stat` syscalls to find the variant — I/O in the
hundreds of nanoseconds to low microseconds, dwarfing the parse. Those only
happen for a compressible type on a precompressed root; the `is_compressible`
gate (~15 ns) is what keeps a `.png` request from paying any of it.

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
| `extract` — read `is_request` + `boosted` + `target` + `trigger` | **91.2 ns** | 101.2 ns | 148.7 ns | **1.11× faster** than axum-htmx |
| `respond` — body + content-type + 3 `HX-*` + `Vary` | **312.0 ns** | — | 352.4 ns | **1.13× faster** than the substrate |
| `turn` — extract the request, build the reply | **215.1 ns** | 227.4 ns | — | **1.06× faster** |

The response row has no separate axum-htmx column because its responders *are*
`HeaderMap::insert` calls — the `http substrate` figure is what axum-htmx compiles
to, and the fair floor to beat.

### Where the win comes from, and where it does not

**Reading is a modest win — 1.11× over the dedicated crate.** Two reasons, both
from kernway-core: the one-buffer `Headers` scans a handful of short byte strings
instead of hashing each name (same structure the codec benches favour above),
and `target()`/`trigger()` return a borrowed `&str`. axum-htmx's `HxTarget` /
`HxTrigger` are `Option<String>` — they *allocate* the value out of the header on
every extraction. The kernway extractor allocates nothing.

**Writing is a narrow win — 1.13×.** Both sides allocate the HTML body `Vec`,
which dominates, so the header structure only moves the remainder; `Headers` edges
`HeaderMap` on a small set for the reasons the codec section already measured.

**Over a whole turn it is 1.06× — a small but consistent win,** and that is the
honest headline: the entire htmx layer — read the flags, pick fragment vs page,
set the headers — costs **~215 ns**, next to the ~520–850 ns the request pipeline
already spends parsing, routing, and encoding. htmx handling is not where a
request is won or lost on either framework. What kernway-htmx buys is not
throughput; it is a typed, allocation-free, correct-by-construction API (the
`Vary: HX-Request` is automatic, not a thing you must remember) that is also,
measurably, never slower. Faster where it can be, equal where the body allocation
rules — never the bottleneck.

## File streaming — the chunk size, measured not guessed

`cargo bench -p kernway-server --bench stream_chunk`

`FILE_CHUNK` was `64 KiB` with a comment admitting it was a guess. A large-file
download over loopback, swept across chunk sizes (128 MiB file, best of 5, one
shard), shows where throughput tops out on this profile:

| chunk | MB/s | % of peak |
|---|---|---|
| 64 KiB | 1547 | 46% |
| 128 KiB | 2241 | 66% |
| 256 KiB **(default)** | 2944 | 87% |
| 512 KiB | 3339 | 99% |
| 1 MiB | **3371** | **100% (peak)** |
| 2 MiB | 3315 | 98% |
| 4 MiB | 3045 | 90% |
| 8 MiB | 2807 | 83% |
| 16 MiB | 2312 | 69% |

Two effects, both visible. Throughput climbs steeply at small sizes because each
chunk is a `spawn_blocking` hop (enqueue, wake a pool thread, run, return), and
that fixed per-chunk cost dominates when the chunk is small — 64 KiB on a 128 MiB
file is 2048 hops, 1 MiB is 128. Past ~1 MiB it falls again: a chunk larger than
the CPU cache stops the read buffer staying hot between the read and the write.

**The default is 256 KiB, not the 1 MiB peak.** A default multiplies by
concurrency — the per-chunk buffer is live per in-flight download, so 1 MiB ×
hundreds of connections is memory a server cannot assume. 256 KiB reaches 87% of
peak while staying memory-bounded; the peak is one `.file_chunk_size(1 << 20)`
away for a download-heavy deployment that has measured its own concurrency. This
is the KEP-0000 §2 loop applied to a constant that had been an assumption since
M2b — and the reason the knob is now public.

## Upload streaming — the inbound chunk size

`cargo bench -p kernway-server --bench upload_chunk`

Uploading 128 MiB over loopback, 5 reps each, best of run:

| chunk | MB/s |
|---|---|
| 64 KiB | 1087 |
| 128 KiB | 1237 |
| 256 KiB **(default)** | 1240 |
| 512 KiB | 1234 |
| 1 MiB | 1307 |
| 2 MiB | 1307 |
| 4 MiB | 1259 |
| 8 MiB | 1246 |

Upload throughput plateaus earlier than download (~1 MiB) because the bottleneck
is the `spawn_blocking` write path, not the read. The default 256 KiB is at 95%
of peak.

## Template rendering — kernleaf vs minijinja

`cargo bench -p kernleaf`

kernleaf is measured against **minijinja 2** — the dynamic-template incumbent that
pays the same runtime-dispatch cost (the fair bar; Askama compiles to the binary
and is a different trade-off, per [KEP-0003]). kernleaf speaks Thymeleaf (`th:*`
attributes), minijinja speaks Jinja — two different surfaces rendering a 50-row
name list to the **same HTML**, which the bench asserts before timing, so it is
the same work and the same escaping both sides.

| Benchmark | kernleaf | minijinja | kernleaf is |
|---|---|---|---|
| `render/user_list_50` (the hot path) | **4.163 µs** | 9.266 µs | **2.22× faster** |
| `parse/user_list` (once, off the request path) | 2.249 µs | 1.457 µs | **1.54× slower** |

Slice B's full Standard Expression engine still leaves render ahead where it
matters: every `${…}` walks an expression AST instead of a bare path lookup, and
kernleaf is still 2.22× faster on the hot path while doing strictly more.

[KEP-0003]: ../kep/0003-template-model.md

### What this measures, and what it does not

**Render is the number that matters** — parsing happens once at `add`/hot-reload,
never per request. On this machine kernleaf renders 2.22× faster because it walks
a parsed DOM directly over a minimal `Value`, where minijinja runs a more general
bytecode VM — and it is faster *while* doing real Thymeleaf work (attribute
processors, natural templates). It is a fair result on identical output, but an
**honest** one: kernleaf today does *less* of the Standard Dialect — no
expression operators, no `@{}`/`#{}`, no utility objects, one escaping context.
Some of the gap is that missing generality, and it will narrow as those slices
land; the benchmark is rerun each time, not quoted as a permanent ratio. (An
earlier `{{ }}` prototype measured 2.74×; adopting the real Thymeleaf attribute
engine — and a simpler name-only template — brought it to 2.22×, which is the
honest current number.)

**Syntax was not a speed decision — the measurement is why kernleaf could adopt
Thymeleaf without a runtime penalty.** A `{{ }}` surface and Thymeleaf's `th:*`
attributes both compile to a cached IR walked the same way; the main difference
is *parse* cost (an attribute grammar needs HTML-aware parsing), and parse is off
the request path even when it is slower here (2.249 µs vs 1.457 µs). The `render`
number above is the Thymeleaf engine's, and it is still 2.22× minijinja. So
Thymeleaf's natural-templates win (a designer previews a `.html` without the
server) came at no measured runtime cost — which is exactly why the surface
choice was made on product merit, with the speed question settled first.

## DI resolution — bean lookup is nearly free

`cargo bench -p di-core`

| Benchmark | Time |
|---|---|
| `resolve/get_concrete` | 14.981 ns |
| `resolve/get_as_trait` | 29.336 ns |
| `resolve/get_missing` | 2.8008 ns |
| `refresh/two_component_graph` | 554.79 ns |

A bean lookup on every `#[inject]` field costs ~15 ns. `refresh` — the topological
wiring of the whole graph — runs once at startup, so its 555 ns for a two-node
graph is a boot cost, not a per-request one.

### The hasher, measured

| Benchmark | Time |
|---|---|
| `siphash_17_lookups` | 185.02 ns |
| `passthrough_17_lookups` | 42.992 ns |

**4.3× faster.** The container keys on `TypeId`, which is already a
well-distributed 128-bit value, so hashing it again with SipHash is wasted work;
a pass-through hasher folds it to 64 bits and stops.

This benchmark exists because a comment in `context.rs` once claimed the win was
"~2×". It was a guess, and it was wrong — the real figure is 4.3×. Both numbers
were assertions until someone measured, which is the whole reason
[KEP-0000 §2](../kep/0000-principles.md#2-fast--measured-or-it-is-not-a-claim)
exists.

## Security — session store and presence

`cargo bench -p kernway-security`

| Benchmark | Time | Notes |
|---|---|---|
| `session_store/get` | 96.5 ns | Token lookup by ID |
| `session_store/verify` | 1.68 µs | HMAC-SHA256 verify + lookup |
| `presence_constant/heartbeat` | 65.2 ns | Record a heartbeat |
| `presence_constant/is_online` | 47.3 ns | Check one user online |
| `presence_read/count/100` | 94.7 ns | Count online users (100 tracked) |
| `presence_read/count/10000` | 9.03 µs | Count online users (10 000 tracked) |
| `presence_read/count/100000` | 146.6 µs | Count online users (100 000 tracked) |
| `presence_read/online_list/100` | 7.73 µs | Collect online list (100 tracked) |
| `presence_read/online_list/10000` | 2.01 ms | Collect online list (10 000 tracked) |
| `presence_read/online_list/100000` | 34.2 ms | Collect online list (100 000 tracked) |

Session lookup and single-user presence checks are sub-100 ns — fast enough to
run on every authenticated request. Presence counting scales linearly with
tracked users; `online_list` at 100 000 users (34 ms) is a bulk query, not a
per-request operation. HMAC verify at 1.68 µs is the bottleneck for token
validation, not the store lookup.

## HTTP codec

`cargo bench -p kernway-http`

| Benchmark | Time |
|---|---|
| `parse/minimal_get` | 266.1 ns |
| `parse/browser_get_8_headers` | 818.1 ns |
| `parse/json_post_with_body` | 411.7 ns |
| `parse/incomplete_head` | 165.4 ns |
| `encode/small_json_close` | 68.1 ns |
| `encode/eight_headers` | 571.6 ns |

Parsing a realistic browser request with eight headers takes ~818 ns; encoding a
small JSON response ~68 ns. `incomplete_head` is the partial-read path — the
parser returns "need more bytes" in 165 ns rather than failing, which is what
lets a connection accumulate a request across several reads.

Encoding briefly regressed to 75 ns when `Body` split the head from the body
(KEP-0002) — the head sized its own buffer, then the body was appended, risking a
realloc between them. Fixed by sizing one buffer for head+body up front
(`encode_head_into`), back to ~68 ns.

## HTTP client — outbound codec

`cargo bench -p kernway-http-client`

| Benchmark | Time | Notes |
|---|---|---|
| `http-client/url_parse` | 88.3 ns | Parse a URL into scheme/host/path/query |
| `http-client/encode_request` | 568.2 ns | Encode a GET with 3 headers |
| `http-client/parse_head/kernway` | 238.9 ns | Parse a response head (kernway parser) |
| `http-client/parse_head/httparse` | 105.8 ns | Same, httparse (incumbent) |
| `http-client/decode_chunked/256` | 57.3 ns | Decode one 256-byte chunk |
| `http-client/decode_chunked/4096` | 520.0 ns | Decode one 4 KiB chunk |
| `http-client/decode_chunked/whole/16384` | 1.59 µs | Collect 16 KiB chunked body |
| `http-client/decode_chunked/whole/65536` | 6.32 µs | Collect 64 KiB chunked body |
| `http-client/percent_encode` | 410.7 ns | Percent-encode a query string |

The kernway response-head parser is **2.26× slower than httparse** (238.9 ns vs
105.8 ns). httparse is the HTTP parsing specialist (`hyper`, `axum`, and most of
the ecosystem use it); the gap is an open item on the client's optimisation list,
not a claim that the client is production-grade today. On the request path the
client is used once per outbound call, so 238 ns is not a bottleneck in practice
— but it is honest to record where we trail.

## Headers structure — HashMap vs one-buffer

`cargo bench -p kernway-core --bench headers`

| Benchmark | HashMap | Headers | Result |
|---|---|---|---|
| `headers/build_iterate/*/5` | 428.42 ns | 348.61 ns | **Headers 1.23× faster** |
| `headers/build_iterate/*/12` | 1148.2 ns | 734.63 ns | **Headers 1.56× faster** |
| `headers/lookup_one/*_12` | 17.859 ns | 16.744 ns | Headers 1.07× faster |

The one-buffer `Headers` wins once the set is more than tiny, and it stays at
least competitive even on single-name lookup. That is why the codec and htmx
paths lean on it instead of a `HashMap<String, String>`.

## Runtime — executor and scheduling

`cargo bench -p rt-core`

| Benchmark | Time | Per unit |
|---|---|---|
| `block_on/ready_future` | 43.1 ns | — |
| `spawn/10` | 3.789 µs | 378.9 ns/task |
| `spawn/1000` | 152.0 µs | **152.0 ns/task** |
| `wake_poll_cycle/1` | 78.7 ns | — |
| `wake_poll_cycle/1000` | 36.636 µs | 36.6 ns/wake |
| `timers/expired_sleep` | 87.9 ns | — |

Spawning scales linearly and cheaply: ~152 ns per task at a thousand tasks. A
wake-and-poll cycle is ~37 ns amortised. These are the operations the runtime
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

## File I/O vs axum and actix — where Kernway trails today

`benchmarks/framework-comparison/` (a crate detached from the workspace so its
axum/actix/tokio tree never enters a normal build) runs three equivalent servers
— Kernway, axum + tower-http, and actix-web — behind one keep-alive load driver,
each doing the *same work*: serve a file, ingest a POST body to disk, and accept
a `multipart/form-data` file part to disk. Reproduce:

```
docker build/run rust:1-bookworm, then:
  cd benchmarks/framework-comparison && cargo build --release
  CONC=32 SECS=5 FILE_MIB=32 PAYLOAD_MIB=8 ./run.sh
```

**Environment**: Docker Linux, 12 cores, loopback, 32 concurrent keep-alive
connections, 5 s per workload, default framework configs. Throughput is body
bytes moved per second (best of the run). All three write uploads to a temp file
and delete it, so the disk work is equal.

| Workload | Kernway | axum + tower-http | actix-web | Kernway vs best |
|---|---|---|---|---|
| **Download** (32 MiB file) | 5246 MB/s | 9682 | 12703 | **0.41×** |
| **Upload** (8 MiB → disk) | 3196 MB/s | 8350 | 8977 | **0.36×** |
| **Multipart** (8 MiB → disk) | 1348 MB/s | 8121 | 8327 | **0.16×** |

**Kernway is 2–6× slower on file I/O today, and the reasons are architectural,
not incidental** — each is a known item, already written down as future work:

- **Download (~2×).** The file streams through userspace with a `spawn_blocking`
  hop *per chunk*. tower-http and actix do not `sendfile` either (both use
  `tokio::fs`), so the gap here is not zero-copy — it is the per-chunk thread-pool
  hop plus the conservative 256 KiB default (87% of Kernway's own peak; see *File
  streaming* above). Integrated async file reads, a bigger default, or `sendfile`
  ([KEP-0002] future work) each close part of it.
- **Upload (~2.6×).** Same shape inbound: `spool_body` does a `spawn_blocking`
  write per chunk, where axum/actix stream straight into an async `tokio::fs`
  file with no per-chunk hop.
- **Multipart (~6×).** The worst case, and expected: [KEP-0008]'s first cut spools
  the whole body to disk, reads it *back* into memory to parse, then writes the
  file part out again — two writes and a full read where axum/actix stream one
  field to one file. Socket-direct parsing (the KEP's deferred optimisation) is
  exactly what removes this.

**Caveats that soften the absolute numbers (not the direction).** The load driver
shares the 12-core box with the server, and Kernway runs more threads under load
(one pinned shard per core plus a blocking pool), so it pays more for CPU
oversubscription than a bounded tokio pool does — a dedicated load box would
narrow the gap. The figures are loopback and memory-bandwidth-bound (multi-GB/s),
so they measure per-byte framework overhead, not disk. Re-run on your own setup
before quoting.

This is the honest state: Kernway's correctness-first file paths are not yet
competitive on raw throughput, and the benchmark now says so with a number rather
than a silence. It also gives the optimisation work ([KEP-0002] async/`sendfile`,
[KEP-0008] socket-direct multipart) a target to beat.

[KEP-0002]: ../kep/0002-response-body.md
[KEP-0008]: ../kep/0008-request-body.md

## Not yet measured — do not quote these

Listed so nobody mistakes silence for a good result. Each is a real question with
no Kernway number behind it today.

| Claim | Status |
|---|---|
| Requests/sec end to end | **partly measured** — file I/O throughput vs axum/actix is above; a general JSON/echo RPS is still open |
| p50 / p99 / p999 latency | **not measured** — the comparison above reports throughput, not a latency distribution |
| Throughput vs an equivalent tokio/axum app | **measured** for file I/O (download/upload/multipart) — see the section above; Kernway trails 2–6× today |
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
