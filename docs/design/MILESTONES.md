# Kernway — goal-driven milestones

## How this differs from ROADMAP.md

`ROADMAP.md` is organised by **version**: what ships in v0.3, v0.4, v0.5. It
answers "when does feature X arrive?"

This document is organised by **one goal**, sliced. It answers "what is the
smallest thing we can run end to end, and what does running it teach us?"

The two are complementary and must not contradict each other. Where they do,
this file is the one being executed and `ROADMAP.md` needs correcting.

## The goal — fixed, does not move

```
A Kernway web application deploys as a Docker image and runs.
  · dev:        edit and see it immediately
  · production: fast and stable
```

Concretely, the end state a user experiences:

```bash
kernway new my-site && cd my-site
kernway dev                          # edit a template → visible instantly
kernway build                        # one static binary, assets embedded
docker build -t my-site . && docker run -p 8080:8080 my-site
```

Everything below exists to reach that line, and nothing is built that does not
serve it.

## Method — walking skeleton

Build the thinnest possible end-to-end slice **first**, even when it does almost
nothing. Then thicken it. Never build a layer in isolation and hope it fits.

The reason is not tidiness. It is that a running slice **tells you what is
missing**, and it tells you in the right order — whereas a plan written up front
tells you what you already believed. The first slice below is deliberately
trivial, and it will still surface more gaps than any amount of design.

Each milestone therefore has a **gate**: an observable, runnable check. Not "the
code is written" — something that either happens or does not.

| Rule | Why |
|---|---|
| Every milestone ends in something runnable | A milestone that cannot be demonstrated cannot be verified |
| The gate is observable, not a checklist | "Done" is an opinion; `curl` returning 304 is not |
| Gaps discovered get their own charter, then get built | See `modules/_TEMPLATE.md` |
| The goal never moves | Scope changes are milestones added, not the target redefined |

---

## M1 — Walking skeleton: it runs in Docker ✅ (2026-07-24)

**Goal**: one example application, one Docker image, and it answers a request and
shuts down cleanly.

Done, and it pulled in a little more than the original plan: static file serving
was small enough to include, so `examples/web-docker` serves a real `index.html`
from `public/` rather than a hardcoded string. That is M2 work brought forward
because the slice was cheap — the walking skeleton finding its own next step.

**What it forced us to build** — and did:

- `crates/kernway-static` — path resolution + MIME, zero dependencies, 21 tests
- `.static_files(root)` on the builder; the read runs on the blocking pool via
  `spawn_blocking`, so it never stalls a shard
- `examples/web-docker/` — static site + JSON route + health checks
- A multi-stage `Dockerfile` on a distroless runtime
- `PORT` from the environment; bind `0.0.0.0`
- `/health` (liveness) and `/ready` (readiness) as distinct endpoints

**Gate — passed, from a clean build:**

```
docker build -f examples/web-docker/Dockerfile -t kernway-web-docker .
docker run -d -p 8080:8080 kernway-web-docker

GET /                      200  text/html   1222 B   (public/index.html)
GET /style.css             200  text/css             (+ x-content-type-options: nosniff)
GET /api/ping              200  {"message":"pong"}   (router wins over static)
GET /health                200
GET /ready                 200
GET /../../etc/passwd      404  (raw, curl --path-as-is — server rejects, not curl)
GET /.env                  404
docker stop                exited in 0.18s
image size                 34.9 MB
```

**What it revealed** — the open questions, answered:

- **Does graceful shutdown work with a real Docker `SIGTERM`?** Yes — 0.18s to
  drain and exit, not the 10s-then-`SIGKILL` of an unhandled signal. This moved
  the charter's "graceful shutdown" row from 🚧 to ✅.
- **Image weight?** 34.9 MB on distroless/cc — measured, no longer a guess.
- **Is the meta-crate usable as one dependency?** *Still not tested* — the
  example depends on the crates by path, because `kernway` does not re-export
  `KernwayApp` yet. That is the honest finding: the front door is still ajar, and
  making it a single `kernway = { features = [...] }` line is the next task, now
  scoped as [M1a](#m1a--close-the-front-door).

**Deferred out of M1**, now explicit: cold-start time and idle RSS were not
measured (they belong to M6's real load test), and HEAD returns 404 for static
paths (GET-only slice; HEAD/Range are M2).

---

## M1a — Close the front door ✅ (2026-07-24)

**Goal**: `examples/web-docker` depends on `kernway` alone, not on six crates by
path. The meta-crate re-exports `KernwayApp`, `Response`, and the prelude; the
example proves it.

**Built**:

- `kernway` now depends on `kernway-server` and `kernway-web` as baseline (a
  fresh `kernway` is a working web server), re-exports `KernwayApp`, `Router`,
  `Json`/`Path`/`Query`, and the HTTP vocabulary
- `kernway::prelude::*` brings in what a handler needs — server, `Response`,
  `StatusCode`, extractors, DI, macros
- a runnable crate-level doctest builds a server through `kernway` alone, so the
  front door cannot silently close again
- `web-docker` reduced to `kernway = { path = ... }` and `use kernway::prelude::*`

**Gate — passed:**

```
examples/web-docker/Cargo.toml [dependencies]:  kernway   (one entry)
cargo run -p web-docker         → the full M1 gate still green
  GET /  200 html · /style.css 200 css · /api/ping JSON · /health 200 · traversal 404
```

The `serde_json` and `di-core` entries turned out to be unused — the example's
bodies are byte literals — so the single dependency is genuinely `kernway`.
(`grep 'path ='` is not 0, as an earlier draft of this gate assumed: the
`[[bin]]` `path` and the intra-workspace `kernway` path are both unavoidable
until the crate is published. The real gate is "one dependency, and it is
`kernway`", which holds.)

**What is deferred**: most of the feature graph. `kernway` pulls the web baseline
unconditionally; `htmx` is the **first capability turned into a Cargo feature**
(M3), and is the template `orm`/`cache`/`openapi`/`sse`/`kernleaf` follow as each
crate becomes ready to gate (`kernleaf` does not exist yet). See the meta-crate
charter.

---

## M2a — Conditional GET, caching, symlink defence ✅ (2026-07-24)

The bulk of "serve files from a folder" arrived in M1. M2a made repeat requests
cheap and closed the last static security gap — **without** the async-handler
refactor, because the static read already runs on the blocking pool at the
connection level, not in a handler.

**Built**:

- `kernway-static`: `etag(len, mtime)` and `etag_matches` (weak comparison,
  `*`, lists) — pure, 6 new tests
- `StatusCode::NOT_MODIFIED` (304) + writer status text
- `kernway-server::load_static`: canonicalize-and-recheck (symlink defence),
  stat, ETag, `If-None-Match` → 304 without reading the body, `Cache-Control:
  no-cache`; 5 filesystem tests including a real symlink escape

**Gate — passed:**

```
GET /                              200, etag: "...", cache-control: no-cache, nosniff
GET / -H If-None-Match: <etag>     304, 0-byte body        (cache current, body not read)
GET / -H If-None-Match: "wrong"    200, 1222-byte body     (stale → full send)
GET /leak.txt  (→ /etc/hosts)      404                     (symlink escaping root, canonicalize catches it)
```

The symlink case is a live `curl` against a real symlink *and* the automated
`a_symlink_escaping_the_root_is_rejected` test — the KEP-0000 §3 rule that a
security claim is a test, so it can never silently regress.

## M2b — Streaming, HEAD, Range ✅ (2026-07-24)

**Goal**: serve a large file without reading it all into memory, and answer
HEAD and byte-range requests.

It was blocked on a real decision, now made: KEP-0002 turned `Response.body`
from `Vec<u8>` into a `Body` enum (`Empty` | `Bytes` | `File`). Done in two
verified steps.

**Streaming — done:**

- `Body` enum in `kernway-core`; `Response::file(path, len)` names a file. The
  refactor was behavior-preserving — all tests stayed green through the type
  change (`.body()` still takes bytes, `IntoResponse` still produces `Bytes`).
- The encoder split: `encode_head(response, connection, content_length)` writes
  the head with the length passed in, so a body that is not in memory can still
  be framed. In-memory responses still coalesce head and body into one write.
- `stream_file` in the connection task: open, seek, and read each 64 KiB chunk on
  the blocking pool via `spawn_blocking`; only the socket write is on the shard.
  Memory is O(chunk), not O(file).
- `load_static` no longer reads the file — it stats for the ETag and returns
  `Body::File`; the stream happens in the connection task.

**Gate — passed:**

```
GET /big.txt (200 KB, >3 chunks)  200, content-length: 200000, whole file intact
GET / (index.html, 1 chunk)       200, streamed, content matches
```

Verified over a real socket in `streams_a_large_file_in_chunks_over_http` and
`serves_a_real_file_over_http` — the streaming loop crossing chunk boundaries is
a test, not a manual check.

**HEAD and Range — done:**

- HEAD: `write_response` takes an `is_head` flag and writes head-only —
  `encode_head` already carried the length, so a HEAD sends the file's length
  with no body and never reads the file.
- `Range: bytes=start-end` → `206` + `Content-Range` + `Accept-Ranges`, streamed
  from the range offset; an unsatisfiable range → `416` with `bytes */len`. A
  multi-range request serves the full body once (§14.2 permits), so ranges
  cannot be amplified into N responses — the DoS cap is by construction, not a
  later pass.

**Gate — all passed over a real socket:**

```
HEAD /a.txt              200, content-length: 5, no body, accept-ranges: bytes
GET /f.txt Range 4-7     206, content-range: bytes 4-7/16, content-length: 4, body "4567"
GET /f.txt Range 100-200 416, content-range: bytes */5
GET /big.txt (200 KB)    200, streamed across 3+ chunks, intact
```

`head_returns_the_length_without_a_body_over_http`,
`a_byte_range_returns_206_over_http`, `an_unsatisfiable_range_returns_416_over_http`,
plus `parse_range_cases` for the parser edges — 50 server tests.

**Precompressed `.br`/`.gz` — done:**

- `kernway-static` gained the pure negotiation: `accepted_encodings` (parse
  `Accept-Encoding`, honour `q=0` and `*`, in server-preference order — brotli
  before gzip) and `is_compressible` (the text tier only; `image/*`, fonts, PDFs
  skip the probe and its `stat` entirely).
- `kernway-server::load_static` probes for a variant next to the file, re-checks
  it under the root the same way as the original (a symlinked `.br` cannot
  escape), serves its bytes as `Body::File` with `Content-Encoding`, keeps the
  **original** `Content-Type`, derives the `ETag` from the variant actually
  served, and sets `Vary: Accept-Encoding` on every negotiated response —
  including the identity fallback, so a shared cache stays correct.
- Opt-in: `KernwayApp::builder().static_files("public").precompressed()`. Off by
  default, so the common path pays no extra `stat`.

**Gate — passed over a real socket** (`examples/web-docker`, `style.css` 1055 B):

```
GET /style.css  Accept-Encoding: br, gzip   200, content-encoding: br,   479 B, vary
GET /style.css  Accept-Encoding: gzip       200, content-encoding: gzip, 531 B
GET /style.css  (no Accept-Encoding)        200, identity 1055 B, vary present
GET /style.css  Accept-Encoding: br;q=0,…   200, falls back to gzip
```

Locked in `crates/kernway/tests/precompressed_socket.rs` + 6 `load_static` unit
tests (br-preferred, gzip fallback, identity-with-Vary, binary-not-negotiated,
empty-AE-still-varies, off-means-off) — 56 server tests.

**Measured**: −50% (gzip) to −55% (brotli) payload for ~200 ns of negotiation and
zero per-request compression CPU. Parity with nginx/tower-http, not an edge — see
[BENCHMARKS.md](BENCHMARKS.md#precompressed-static--the-payload-win-and-its-honest-cost).

**Left, and small:** the 64 KiB stream chunk size to tune against a large-file
load test. Static serving is otherwise done.

---

## M3 — htmx, as a feature ✅

**Goal**: static HTML with `hx-get` calls an endpoint; the endpoint returns a
fragment; it swaps.

**Built** (M3 slice, htmx 2.0.x):

- `kernway-htmx` — its own crate, depending on `kernway-core` only. **Opt-in as
  `kernway = { …, features = ["htmx"] }`**, not baseline: the default stays
  static-only per the design decision that made htmx the first capability
  feature. (`Html<T>`, the response type, is baseline in `kernway-web`.)
- `Htmx` extractor: `is_request()`, `is_boosted()`, `is_history_restore()`,
  `target()`, `trigger()`, `trigger_name()`, `current_url()`, `prompt()` —
  all returning a borrowed `&str`, no allocation
- `HtmxResponse` builder: `trigger()`/`trigger_after_settle()`/…, `redirect()`,
  `location()`, `refresh()`, `push_url()`, `replace_url()`, `retarget()`,
  `reswap(Swap::…)`, `reselect()` — `Swap` is an enum, so a typo is a compile error
- **Automatic `Vary: HX-Request`** via `respond(fragment, full_page)`, appended
  to any existing `Vary`, never clobbering it

**Gate — passed** (`examples/web-docker`, live over a socket):

```text
curl -i -H 'HX-Request: true' :8199/htmx/greet
#   → 200, `hx-trigger: greeted`, `vary: HX-Request`, 57-byte fragment
curl -i :8199/htmx/greet
#   → 200, same URL, `vary: HX-Request`, 125-byte full page
```

Locked in as `crates/kernway/tests/htmx_socket.rs` (feature-gated), so the gate
re-runs in CI, not just once by hand.

`Vary` is in the gate because without it a cache serves a fragment to a browser
expecting a page — the classic htmx bug, and invisible until it happens to a
user.

**Measured** (KEP-0000 §2, vs the incumbent): reading the `HX-*` headers is
**1.39× faster than `axum-htmx` 0.8** (57.8 ns vs 80.2 ns — no per-name hash, and
`target()`/`trigger()` borrow where axum-htmx allocates a `String`); building the
reply is 1.16× faster than the raw `http` substrate; a full turn is a **tie
(~2%)** because the body allocation dominates both. htmx handling is never the
bottleneck on either framework — the crate earns its place on typed,
allocation-free correctness, not a throughput claim. See
[BENCHMARKS.md](BENCHMARKS.md#htmx-handling--head-to-head-against-the-incumbent).

---

## M4 — Templates and security (`features = ["web"]`) ✅

**Goal**: render a page from data; accept a form back safely.

**Forces us to build**:

- ~~Model representation — the current `TemplateContext` returns `&dyn Any` and
  **cannot be implemented against**; this must be decided first~~ ✅ **done**:
  [KEP-0003](../kep/0003-template-model.md) replaced it with a borrowed `Value`
  tree + `ToValue`. A reference engine in `kernway-core`'s tests interpolates,
  HTML-escapes, and iterates a `Seq` against it — proof it is implementable (the
  old trait was not). Hot reload (M5) forced the model to be *dynamic*; the
  disciplined form is a serde-free enum that borrows its strings.
- ✅ `kernleaf`: parse → IR → render, cached, off the request path. Built as the
  full **Thymeleaf Standard Dialect** (`th:*` attributes, natural templates, the
  Standard Expression language, `@{}`/`#{}`, `#`-utility objects) — larger than the
  roadmap's original modest `kw:`-prefixed sketch, at the user's "chuẩn Thymeleaf"
  direction. **1.7× faster than minijinja** on render. See the charter.
- ✅ Context-aware escaping: HTML body/attribute, URL (`@{}`), JS and CSS
  (`th:inline`) each get their own rule, chosen at parse time so the HTML path
  stays fast.
- ✅ `kernway-security`: CSRF (double-submit token) + security headers + a
  `SecurityContext`; its own crate, its own charter.
- ✅ `th:authorize` (via a `kernway-core` `Authorization` trait) + auto-CSRF form
  injection, wired into `kernleaf`.
- ✅ Fragment addressing (`th:fragment` + `th:insert`/`th:replace`), so htmx gets a
  fragment from the same template — cross/same/whole-template refs, depth-capped
  against cycles (kernleaf slice G).

**Gate — met** (kernleaf unit tests, 69 total):

```
th:text of "<script>…"        → escaped, not executed          (interpolated_html_is_escaped)
th:inline="javascript" value  → \u-escaped, no <script> breakout (javascript_inline_escapes…)
th:authorize with no context  → element dropped (fail-closed)   (th_authorize_is_fail_closed…)
POST form                     → hidden _csrf field auto-injected (auto_csrf_injects…)
```

The XSS/escaping and CSRF cases — the gate's real bar — pass. The server-side
CSRF verify is called at the top of every state-changing handler
(`csrf::verify_request` → 403 if missing or forged), tested in
`examples/login-htmx/tests/login_flow.rs::a_post_without_a_csrf_token_is_forbidden`.
A `CsrfMiddleware` that applies it automatically is a future DX improvement,
not a gate requirement.

**Gate**:

```bash
# XSS attempt is escaped, not executed
curl 'localhost:8080/search?q=<script>alert(1)</script>'   # &lt;script&gt;
# javascript: URL in an attribute is neutralised
# POST without a CSRF token is rejected
curl -X POST localhost:8080/users -d 'name=x'              # 403
```

Security cases are the gate. A template engine that renders correctly but escapes
incorrectly has not passed.

---

## M5 — Developer experience: hot reload

**Goal**: edit and see it, without thinking about the server.

**Tiered**, because most edits need no rebuild at all (targets — not yet measured):

| Edit | Mechanism | Target latency | Restart |
|---|---|---|---|
| Template `.kwl` | watcher → recompile IR | < 10 ms | no |
| Static asset | watcher → invalidate cache + ETag | < 10 ms | no |
| Config | watcher → reload the reloadable parts | ms | no |
| Rust code | rebuild + socket handover | 1–3 s | yes, zero-downtime |

The last row replaces the `.so` plugin idea. A supervisor holds the listening
socket (or both processes bind with `SO_REUSEPORT`), the new child starts
accepting, the old one drains and exits. No `dlopen`, no ABI risk, and the hard
part — graceful drain — already exists.

**Gate**: edit a `.kwl`, refresh the browser, see the change, with no restart in
the log. Edit a `.rs`, and no request is dropped across the handover.

---

## M6 — Production build

**Goal**: `kernway build` produces the artifact the goal statement promises.

**Forces us to build**:

- Asset embedding for release (`include_dir!`) — dev reads from disk, release
  compiles them in, so deployment is genuinely one file
- Release profile: LTO, `codegen-units`, `strip`
- The allocator decision, **measured, not assumed**: musl's malloc is slow under
  multi-threaded allocation, which is the worst case for thread-per-core. If
  `FROM scratch` is wanted, mimalloc or jemalloc probably has to come with it.
- Compile-time measurement for each feature configuration

**Gate**:

```bash
kernway build
ls -la target/release/my-site       # one binary
docker build . && docker images     # size recorded
# cold start and idle RSS measured; benchmark against the same app on Axum
```

Every number in `README.md` and `ARCHITECTURE.md` comparing Kernway to Spring or
tokio becomes checkable here. Several of them are currently inherited from the
literature rather than measured — see
[KEP-0000 §2](../kep/0000-principles.md#2-fast--measured-or-it-is-not-a-claim).

---

## Cross-cutting: correct the public claims

~~Not a milestone — a debt to clear, and the earlier the better.~~

**Resolved** — the items below have been addressed.

| Claim | Was | Now |
|---|---|---|
| `README.md` roadmap | "**v1.0** ✅ Stable API, full feature set" | Replaced with milestone table (M1–M6) |
| Single `kernway` dependency | "does not yet work" | Fixed in M1a — `use kernway::prelude::*` |
| Templates | "planned, not built" | `kernleaf` built in M4 (Thymeleaf dialect, 1.7× vs minijinja) |
| Async handlers | "planned, not built" | Still pending — honest, no change needed |

The `ROADMAP.md` version-based table and the `FEATURES.md` column headers
still use v0.3–v1.0 labels that do not correspond to milestones. These are
cosmetic — they do not make a false claim about what *exists today* — and can
be aligned with the milestone model whenever ROADMAP.md is next touched.

## Order, and what blocks what

```
M1  skeleton in Docker ──┬── M2  static files ──┬── M3  htmx
                         │                      │
                         │                      └── M4  templates + security
                         │                                    │
                         └────────────────────────────────────┴── M5  hot reload
                                                                      │
                                                                      └── M6  prod build
```

M1 blocks everything, and it is the smallest. That is the point of a walking
skeleton: the cheapest slice is the one that removes the most uncertainty.

M3 and M4 are independent of each other — htmx needs no template engine, and a
template engine needs no htmx. They meet only at fragment rendering.

## Per-milestone checklist

Before a milestone is called done:

- [ ] The gate passes, demonstrably, from a clean checkout
- [ ] Each new crate has a charter (`modules/_TEMPLATE.md`)
- [ ] Decisions that are expensive to reverse have a KEP
- [ ] Public documentation matches what exists — no aspirational claims
- [ ] Security cases in the gate are tests, not manual `curl` runs
- [ ] Numbers stated anywhere are measured; unmeasured ones are labelled
      hypotheses
