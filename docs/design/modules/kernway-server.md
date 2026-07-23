# kernway-server — HTTP server, routing, and the view pipeline

## Purpose

The crate that turns registered components into a running HTTP server: bind an
address, accept on every core, match a request to a handler, run the middleware
chain, write the response.

It is the only crate that knows about *both* the transport (`rt-net`) and the
application (`di-core`).

### Scope — decided 2026-07-23

**The server's built-in capability stops at static resources.** HTTP, routing,
middleware, and serving files off disk. That is the whole of it.

Everything that turns data into a page — template engines, htmx, Markdown,
XHTML, anything anyone invents later — is **outside the server**, in a separate
crate, enabled by a Cargo feature or simply by depending on it:

```toml
kernway = { version = "0.6", features = ["htmx"] }   # now the app speaks htmx
kernway-view-tera = "0.1"                            # someone else's engine
my-weird-xhtml-renderer = { path = "../mine" }       # yours, no permission needed
```

The consequence that matters: **the server does not know what a view is, and
must never learn.** A third party can render pages any way they like — XHTML,
a hand-rolled DSL, something nobody has thought of — and Kernway hands the work
over rather than trying to anticipate it. What Kernway supplies in exchange is
not permission but a contract; see [The extension contract](#the-extension-contract).

**Not** in scope: parsing HTTP bytes (`kernway-http`), the async runtime
(`rt-core`/`rt-net`), extractors (`kernway-web`), or **any** form of rendering.

> **Naming collision, unresolved.** This charter covers the crate that exists at
> `crates/kernway-server`. An earlier version of this document described a
> *different* thing under the same name: the v0.5 pre-compiled host that
> `dlopen`s the user app for hot reload. That plan is preserved below under
> [Future: the hot-reload host](#future-the-hot-reload-host), but one of the two
> needs renaming before v0.5 — see [Open questions](#open-questions).

## Principle: async everywhere, no exceptions

**Nothing on the request path may block.** Not "should not" — may not. This is
the first constraint every design in this charter is checked against, and it
outranks convenience, familiarity, and how much code it costs to hold.

The reason is [KEP-0000 §4](../../kep/0000-principles.md#4-stable--never-block-never-surprise), and it is
sharper than the usual argument for async. A work-stealing runtime survives a
blocking call: the task holds one worker, and other workers keep serving. Under
thread-per-core there are no other workers. A blocking call inside a handler
stops **every connection on that core** — a single `std::fs::read` of a slow disk
stalls a thousand unrelated requests that happened to land on the same shard.

So blocking is not a performance issue here. It is a correctness issue, and it
fails in the worst possible way: invisibly, under load, on one core at a time,
with a tail latency graph that looks like someone else's problem.

**What this rules out, concretely:**

| Never on the request path | Use instead |
|---|---|
| `std::fs::*` — file read, metadata, directory | async I/O, or `Body::File` and let the connection task do it |
| `std::net::*`, blocking HTTP clients | async transport |
| `std::sync::Mutex` held across an await | `RefCell` — the task never migrates, so this is sound |
| `std::thread::sleep` | `rt_core::sleep` |
| A synchronous database or cache driver | `rt_core::spawn_blocking` |
| A slow pure-CPU pass (image resize, large parse) | `spawn_blocking` — CPU work blocks a core just as effectively as I/O |

The last row is the one people forget. `spawn_blocking` is not only for I/O; the
core cannot tell the difference between waiting on a disk and computing for
80 ms, and neither can the connections queued behind it.

**Where blocking is still allowed**: startup and shutdown. Reading config,
compiling templates, opening a connection pool, and binding the listener all run
before the first request is accepted, and blocking there costs nothing.

**Two things follow that are easy to miss:**

*The handler future is deliberately not `Send`.* A task never leaves its shard,
so its future does not need to cross threads — see
[Public surface](#target--the-async-handler). This is not an oversight to be
tidied up later; it is the property that lets request-scoped state be an `Rc`
where every other Rust framework forces an `Arc`.

*Template rendering stays synchronous, and that is not an exception to the rule.*
Rendering is CPU work over an already-compiled IR with the model already in
memory — microseconds, no I/O, no waiting. The blocking risk in a view engine is
not rendering, it is **reading the template file**, and that is why compilation
happens at startup or on a background watcher and never on the request path. An
engine that reads from disk during `render` violates this principle no matter how
fast the rendering itself is.

## Status

As of 2026-07-23.

| Area | State | Notes |
|---|---|---|
| Routing — exact paths | ✅ | `HashMap` lookup, O(1) regardless of route count |
| Routing — `{param}` patterns | ✅ | Linear scan over dynamic routes only |
| Routing — prefix mounts (`/assets/**`) | ❌ | Static serving works without it — see below |
| Middleware chain | ✅ | Synchronous, nested |
| Keep-alive, pipelining | ✅ | RFC 9112 §9.3, idle timeout, request cap |
| Graceful shutdown / drain | ✅ | Verified under a real `SIGTERM` from `docker stop`: drained and exited in 0.18s (M1) |
| Panic isolation | ✅ | `catch_unwind` per request → 500, core survives |
| **Static file serving** | 🚧 M1 | GET only, whole-file read on the blocking pool via `.static_files(root)`. Traversal/dotfile rejected. HEAD, Range, ETag, streaming are M2. |
| **Async handlers** | ❌ | Handlers are `Fn(...) -> Response`, blocking. Static files sidestep this (the read is on the blocking pool, not in a handler). |
| **Response body streaming** | ❌ | `body: Vec<u8>` — whole file in memory. M2. |
| **View / template pipeline** | ❌ | Deliberately out of scope — it lives in a renderer crate, not here |
| **htmx support** | ❌ | Its own crate, not started |

**Today**: it serves handler responses and static files over a sharded async
transport, with panic isolation, and shuts down gracefully in a container
([the M1 slice](../MILESTONES.md#m1--walking-skeleton-it-runs-in-docker--2026-07-24) —
`examples/web-docker`).

**Not yet**: templates, HEAD/Range/conditional static requests, and streaming
large files. A reader from Spring Boot will assume a full view resolver exists;
it does not, and by the [scope decision](#scope--decided-2026-07-23) it never
will here — that belongs to a renderer crate.

## Standards

| Spec | Scope | Compliance |
|---|---|---|
| RFC 9112 §9.3 | Connection persistence, keep-alive defaults | full — HTTP/1.0 closes, 1.1 persists |
| RFC 9112 §3 | Request line parsing | full (in `kernway-http`) |
| RFC 9110 §5.1 | Case-insensitive header names | full |
| RFC 9110 §8.7 | Conditional requests — `ETag`, `If-None-Match` | ❌ not started — needed for static |
| RFC 9110 §14 | Range requests — `206`, `Content-Range` | ❌ not started — needed for static |
| RFC 9111 | Caching — `Cache-Control`, `immutable` | ❌ not started — needed for static |
| RFC 6265 | Cookies | ❌ not started |
| IANA media types | `Content-Type` by extension | partial — curated table in `kernway-static::mime_for`, ~18 types |

Rule for this module: an RFC section listed as implemented has a test named after
it. `keep_alive_tests` in `app.rs` is the existing model to follow.

## Architecture

### Today

```text
socket bytes
   │
   ▼
kernway-http::parse_bytes ──► Request
   │
   ▼
middleware chain (sync, nested)
   │
   ▼
Router::find(method, path)
   │         ├── static_index: HashMap<path, Vec<idx>>   ← O(1)
   │         └── dynamic:      Vec<idx>                  ← linear, segment match
   ▼
handler(&Request, &AppContext) -> Response      ← SYNCHRONOUS
   │
   ▼
kernway-http::encode_response_with ──► socket
```

A static route always beats a dynamic one that could also match, whatever the
registration order: `/users/me` is written precisely because it is not an id.

### Target

```text
                         Request
                            │
                  ┌── Middleware chain ──┐
                  │  security headers · CSRF · compression
                  ▼
                Router::find
                            │
   ┌────────────┬───────────┴────────┬──────────────────┐
   │            │                    │                  │
 exact       dynamic              mount            (no match)
 O(1)        /users/{id}          /assets/**            │
   │            │                    │                  ▼
   └─────┬──────┘                    ▼          404, or opt-in
         │                    kernway-static     SPA fallback
         ▼                           │                  │
   async handler(&Request, &AppContext) -> impl IntoResponse
         │                           │                  │
╌╌╌╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌ handover ╌╌╌╌╌┼╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌┼╌╌╌╌╌
         │   the server's knowledge ends at this line   │
         ▼                           │                  │
  .into_response()                   │                  │
  ┌──────────────────────────────┐   │                  │
  │ Json<T>      kernway-web     │   │                  │
  │ Html<T>      kernway-web     │   │                  │
  │ View<T>      kernleaf        │   │  engine + request│
  │ Xhtml<T>     someone's crate │   │  captured in the │
  │ …            anything        │   │  handler above   │
  └──────────────┬───────────────┘   │                  │
                 │                   │                  │
                 └─────────┬─────────┴──────────────────┘
                           ▼
                Response { status, headers, Body }
                           │
                           ▼
                serve_connection (async, per shard)
                   Body::Bytes → write
                   Body::File  → async read / sendfile
```

The dashed line is the scope decision made visible. Above it is
`kernway-server`; below it is anyone's crate. The server routes, serves files,
and writes bytes — it never learns what produced them.

### The four structural gaps

Each blocks the target architecture, and each is a change to a shared type
rather than a feature that can be added on the side.

**1. Handlers are synchronous.** `Handler = Fn(&Request, &AppContext) -> Response`.
Under thread-per-core ([KEP-0000 §4](../../kep/0000-principles.md#4-stable--never-block-never-surprise))
there is no other worker to take up the slack: a blocking call in a handler
stalls *every* connection on that core, not just its own. File I/O in a
synchronous handler is therefore not a performance question, it is a correctness
one.

**2. `Response.body` is `Vec<u8>`.** Serving a 200 MB file means reading 200 MB
into memory, per request. `encode_response_with` also builds head and body into a
single buffer.

**3. The router cannot match a prefix.** `/assets/**` has no representation.
`{path}` matches exactly one segment, so `/assets/css/app.css` would not match
`/assets/{path}`.

**4. `TemplateContext` cannot be implemented against.** The current trait is:

```rust
pub trait TemplateContext {
    fn get(&self, key: &str) -> Option<&dyn std::any::Any>;
}
```

An engine receives `&dyn Any` and can do nothing with it — downcasting requires
knowing the concrete type, which a generic engine by definition does not.
`${user.profile.name}` needs to descend through nested values and `kw:each` needs
to iterate; `&dyn Any` supports neither. This trait must be replaced before any
engine is written against it.

## Public surface

### Today

```rust
pub type Handler = Arc<dyn Fn(&Request, &AppContext) -> Response + Send + Sync>;

pub trait Middleware: Send + Sync + 'static {
    fn handle(&self, req: &mut Request, next: &dyn Fn(&mut Request) -> Response) -> Response;
    fn name(&self) -> &'static str;
}

KernwayApp::builder()
    .bind("0.0.0.0:8080")
    .get("/users/{id}", handler)
    .layer(middleware)
    .build()
    .run()
```

### Target — the async handler

The subtle part, and the place where [KEP-0000 §4](../../kep/0000-principles.md#4-stable--never-block-never-surprise)
pays out in user code:

```rust
/// A future that never leaves its shard — deliberately NOT `Send`.
pub type LocalBoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

pub type Handler = Arc<
    dyn for<'a> Fn(&'a Request, &'a AppContext) -> LocalBoxFuture<'a, Response>
        + Send + Sync,
>;
```

The **handler** is `Send + Sync` because one `Arc<Router>` is shared by every
shard. The **future it returns** is not, because a task never migrates. That
asymmetry is the whole point of thread-per-core reaching the API: request-scoped
state can be an `Rc`/`RefCell` where a work-stealing runtime would have forced
`Arc`/`Mutex`.

`kernway-core::layer::BoxFuture` currently carries `+ Send`. It needs a non-`Send`
sibling for this; the `Send` one stays for anything genuinely crossing threads
(`spawn_blocking` results, timers).

### Target — the response body

```rust
pub enum Body {
    Empty,
    Bytes(Vec<u8>),
    /// The handler names a file; the connection task reads it.
    File { path: PathBuf, len: u64, range: Option<(u64, u64)> },
    Stream(LocalBoxStream<'static, io::Result<Vec<u8>>>),   // later
}
```

`Body::File` is what keeps a handler honest: the handler describes what to send,
and the *connection task* — which is already async — performs the I/O. No
blocking call ever runs inside a handler.

**Stability**: none of the above is stable. `Handler`, `Middleware`, and
`Response` all change in the phases below. Treat every signature here as a
proposal until its KEP lands.

## Integration

**Depends on**:

| Module | Why |
|---|---|
| `kernway-core` | `Request`, `Response`, `StatusCode` — the vocabulary |
| `kernway-http` | Byte-level parse and encode |
| `rt-core` / `rt-net` | Executor, shards, timers, shutdown |
| `di-core` | `AppContext` handed to every handler |

**Depended on by**:

| Module | What it uses |
|---|---|
| `kernway` (meta) | Re-exports the builder |
| examples | `KernwayApp::builder()` |
| *(planned)* `kernway-view` | Registered as a resolver on the builder |
| *(planned)* `kernway-static` | Registered as a mount on the router |

**Must never depend on**:

| Edge | Why |
|---|---|
| `kernway-server` → `kernleaf` or any engine | An engine is chosen by the app, not by the server. Depending on one would make it un-swappable and defeat [KEP-0000 §1](../../kep/0000-principles.md#1-ours--write-it-do-not-import-it). |
| `kernway-server` → `kernway-orm-*` | The HTTP layer has no business knowing about persistence. |

Add both to `scripts/check-core.sh` alongside the existing `forbidden_dep`
entries, so review is not the only thing preventing them.

## Speed

Measured figures come from [BENCHMARKS.md](../BENCHMARKS.md); do not restate a
number here that is not quoted from there.

| Path | Runs | Measured | Budget | Bench |
|---|---|---|---|---|
| `Router::find`, static route | every request | **41 ns, flat from 4 to 102 routes** | O(1), must not grow with route count | ✅ `route/static_hit` |
| `Router::find`, dynamic route | every request with a param | 331 ns @ 4, 2.17 µs @ 102 | — | ✅ `route/param_hit` |
| `Router::find`, miss | every unmatched request | 80 ns @ 4, 1.87 µs @ 102 | should collapse with a mount tree | ✅ `route/miss` |
| Mount match | every static-asset request | — | O(log n) or better on mount count | ❌ to write |
| Template render, cached IR | every page request | — | no disk I/O, no re-parse | ❌ to write |
| Model → `Value` conversion | every render | — | to measure; suspected hot | ❌ to write |
| Static file, cache hit (304) | every repeat asset request | — | no file read at all | ❌ to write |

The static-route row is the one to defend. It is flat across route count today,
and it is measured — the split router keeps a hash lookup off the linear scan, so
every ordinary path (`/`, `/health`, assets, pages) stays at ~41 ns no matter how
large the application grows. Any change to routing has to preserve that shape.

**Allocation policy on the hot path**: the router is already at zero allocations
for a matched static route, and one `HashMap` only for a route that actually has
parameters — `matches_pattern` walks two `split('/')` iterators in step rather
than collecting them, and `extract_params` runs only on the winner. Any change to
routing must hold that line.

The template path has no policy yet, and it needs one before an engine is
written: a render that allocates per interpolation will dominate everything else
on this list.

## Generic — the extension points

| Extension point | Trait | Owned by | Replaceable by |
|---|---|---|---|
| Response type — **the rendering handover** | `IntoResponse` | `kernway-core` (exists) | anyone, no registration |
| Middleware | `Middleware` | `kernway-server` (exists) | any crate |
| Plugin registration | `KernwayPlugin` | `kernway-core` (⚠️ stub) | any crate |
| Shared renderer state | a bean in `AppContext` | `di-core` (exists) | any crate |
| Static file source | `FileSource` *(planned)* | `kernway-static` | embedded, S3, CDN origin |

**Currently hardcoded, and it is a bug**: the 404 body is a JSON string baked
into `handle()`. It should be an overridable handler — an HTML app wants an HTML
404, and per [KEP-0001](../../kep/0001-respect-rust.md) every
framework default is supposed to be overridable.

## The extension contract

This is where the scope decision becomes concrete. Kernway accepts *any* way of
producing a page. It does so not by anticipating them but by defining a narrow
handover and a set of obligations on the other side of it.

### The handover already exists

Two primitives, both shipped:

```rust
pub trait IntoResponse: Send {
    fn into_response(self) -> Response;
}
```

Anyone can define a type and implement this. The server never learns the type
exists — it receives a `Response` and writes bytes. Nothing needs registering,
and no trait in `kernway-server` needs changing.

The second is `AppContext`: a renderer's expensive state — compiled templates,
an escaping table, a fragment index — is a bean, resolved by DI like anything
else.

### The pattern: the handler is where the two meet

`into_response` takes no request and no context, deliberately. A renderer needs
both, and the handler is the one place that already has them — so the handler
loads them into the view type, and `into_response` then has everything it needs:

```rust
async fn users(req: &Request, ctx: &AppContext) -> impl IntoResponse {
    let model = /* ... */;

    Xhtml::new("users/list", model)
        .engine(ctx.get::<MyXhtmlEngine>()?)   // shared, compiled at startup
        .request(req)                          // for HX-Request, Accept, locale
}
```

`Xhtml` is a third-party type. `MyXhtmlEngine` is a third-party bean. Neither
appears anywhere in `kernway-server`.

The alternative — widening `IntoResponse` to
`into_response(self, req: &Request, ctx: &AppContext)` — was rejected: it couples
every response type in the framework, including `Json` and `StatusCode`, to two
things they do not need, to serve a minority that can capture them instead.

### The obligations

"Any renderer is welcome" is only safe because acceptance is conditional. A crate
that plugs into this handover must uphold all six. These are the *reasonable
internal constraints* the scope decision refers to:

| # | Obligation | Why it is not negotiable |
|---|---|---|
| 1 | **Never block on the request path** | Thread-per-core: a blocking render stalls every connection on that core. See [Principle](#principle-async-everywhere-no-exceptions). |
| 2 | **No file I/O during render** | Same reason. Compile at startup or on a background watcher; the request path touches memory only. |
| 3 | **Escape by default** | An engine that interpolates raw by default makes XSS the default. Raw output must be a *differently named* construct the author had to reach for. |
| 4 | **Context-aware escaping, or say you do not do it** | HTML body, attribute, URL, and JS need different rules. An engine that only does HTML escaping must document that `href="${url}"` is not safe in it. |
| 5 | **Set `Content-Type` explicitly** | Never rely on the browser sniffing. `X-Content-Type-Options: nosniff` is set by the server; a wrong or missing type then breaks visibly rather than silently. |
| 6 | **Typed errors, never panic** | A panic becomes a 500 via `catch_unwind`, which loses the template name and line. A render failure is data. |

Obligations 1 and 2 are what make "we accept anything" compatible with the
performance the framework claims. Obligations 3–5 are what make it compatible
with the security it claims. Without them the handover would be an open door
rather than a contract.

**Enforcement is a gap.** Nothing today checks any of this — the obligations are
prose. The intended answer is a conformance suite a renderer crate can run
against itself, in the spirit of the TCK idea in
[KEP-0000 §1](../../kep/0000-principles.md#1-ours--write-it-do-not-import-it).
Until that exists, this table is documentation, not a guarantee, and this charter
should not pretend otherwise.

### What Kernway ships on top

Nothing above is specific to a template engine, which is the point. Kernway's own
crates are simply the first users of the same handover:

| Crate | Provides | Status |
|---|---|---|
| `kernway-web` | `Json<T>`, `Html<T>` — `IntoResponse` impls | ✅ exists |
| `kernway-static` | path resolution + MIME (no I/O); read wired here | ✅ M1 |
| `kernway-htmx` | `Htmx` extractor + `HX-*` response builder | ❌ planned |
| `kernleaf` | a template engine | ❌ planned |
| *anyone's crate* | whatever they want | — |

`kernleaf` gets no privilege the XHTML renderer does not. If it ever needs a hook
that a third-party crate cannot also use, that is a design bug in the hook.

## Security

| Threat | Mitigation | Tested |
|---|---|---|
| Handler panic takes down a core | `catch_unwind` per request → 500 | ✅ |
| Slowloris — connection held open, nothing sent | idle timeout covers first byte and mid-request stall | ✅ |
| Unbounded connection hold | `max_requests` per connection | ✅ |
| Unbounded request head/body | `MAX_HEAD_BYTES`, `MAX_BODY_BYTES` in `kernway-http` | 🚧 |
| **Path traversal** — `../`, `%2e%2e%2f`, `..\`, NUL | percent-decode first, then reject `..`/control/backslash segments lexically, before any I/O | ✅ `kernway-static` unit tests + M1 live `curl --path-as-is` |
| **Dotfile exposure** — `.env`, `.git/` | any segment starting `.` denied as a class | ✅ tested |
| **MIME sniffing** | `Content-Type` from extension only, plus `X-Content-Type-Options: nosniff` | ✅ tested (M1 live: header present) |
| **Directory listing** | no listing exists — a directory request serves its index or 404s | ✅ by construction |
| **Symlink escape** from the static root | a file inside the root linking outside is *not* caught by lexical checks; needs canonicalize-and-recheck at open time | ❌ **M2 — do not serve untrusted symlinked roots until then** |
| **Range amplification** DoS | cap the number of ranges per request | ❌ |
| **XSS via template** | auto-escape by default; raw output is a differently-named syntax | ❌ |
| **XSS via context confusion** | context-aware escaping — HTML body, attribute, URL, and JS need different rules. `<a href="${url}">` with `javascript:` is still XSS after HTML escaping. | ❌ |
| **SSTI / template path traversal** | a template name must never come from user input | ❌ |
| **CSRF** | token auto-injected into POST forms, verified by middleware | ❌ |
| **htmx: `HX-Request` trusted for authorisation** | it is a client-set header. Use it to choose a rendering, never to decide access. | ❌ |

The htmx row is worth stating loudly because it is an easy mistake: every `HX-*`
request header is attacker-controlled.

## Direction

| Phase | Goal | In this crate? | Blocked by |
|---|---|---|---|
| **0** | KEP-0005: static binary — async handlers, `Body`, tiered hot reload, link-time extensions | — | — |
| **1** | Async handlers, `Body::File`, router mounts, `kernway-static` | **yes** | KEP-0005 |
| **2** | Real hooks on `KernwayPlugin`; overridable 404 | **yes** | Phase 1 |
| **3** | `kernway-htmx`: `Htmx` extractor + `HX-*` builder | no — its own crate | Phase 1 |
| **4** | `kernleaf`: parse → IR → render, context-aware escaping | no — its own crate | Phase 1 |
| **5** | Asset embedding for `kernway build`; template watcher for `kernway dev` | shared with `kernway-cli` | Phase 4 |

**The scope decision shortened this list.** The earlier plan had `kernway-view`
as a resolution layer *inside* the request pipeline — the server would learn what
a view is, hold a registry, and negotiate an engine per request. None of that is
needed. A view type implements `IntoResponse` and captures its engine from
`AppContext` in the handler; the server keeps receiving a `Response` and knows
nothing.

So `kernway-server`'s own remaining work is **Phase 1 and 2 only**. Phases 3–5
are separate crates that happen to be written by the same people, and any of them
could be written by someone else instead without this crate changing.

Phase 1 alone delivers the whole of "drop HTML/CSS/JS into a folder and deploy".

Phase 4 is small on purpose. htmx is a *client* library; the server does not
render htmx, it renders HTML. What htmx actually asks of a server is three
things — recognise `HX-Request`, return a fragment instead of a full page, and
speak the `HX-*` response vocabulary (`HX-Trigger`, `HX-Redirect`, `HX-Retarget`,
`HX-Push-Url`, `HX-Reswap`, …). None of that is a template engine. Treating
"thymeleaf + htmx" as two engines to combine would be a design error; it is one
engine with fragment addressing, plus a header vocabulary.

**Deliberately out of scope**: HTTP/2 and TLS (`http2-proto`, `tls-adapter`),
WebSocket (its own crate), session storage (`kernway-cache`), authentication.

## htmx version support

htmx is a client library on a release cadence Kernway does not control, and its
header vocabulary has grown across releases. A server that claims "htmx support"
without saying *which* htmx is making an unverifiable claim — so every Kernway
release states the htmx versions it speaks, and the matrix is part of the release
notes, not folklore.

### Compatibility matrix

| Kernway | htmx supported | Notes |
|---|---|---|
| ≤ v0.5 | — | No htmx support at all |
| v0.6 | 2.x (target), 1.9+ (best effort) | First release with `kernway-htmx` |

> ⚠️ **The version boundaries in the tables below are unverified.** They are
> written from memory of the htmx changelog and **must be checked against
> <https://htmx.org/reference/> and the htmx CHANGELOG before any code depends on
> them.** Getting a boundary wrong means silently ignoring a header a client is
> sending, which is exactly the failure this section exists to prevent. Treat
> "since" columns as *to be confirmed*, not as fact.

### Request headers — what the client sends us

| Header | Since | Meaning | Kernway |
|---|---|---|---|
| `HX-Request` | 1.0 | Always `true` on an htmx request | plan: full page vs fragment |
| `HX-Trigger` | 1.0 | `id` of the element that triggered | plan: expose typed |
| `HX-Trigger-Name` | 1.0 | `name` of that element | plan: expose typed |
| `HX-Target` | 1.0 | `id` of the target element | plan: expose typed |
| `HX-Current-URL` | 1.0 | The browser's current URL | plan: expose typed |
| `HX-Prompt` | 1.0 | User's response to `hx-prompt` | plan: expose typed |
| `HX-Boosted` | 1.6 (TBC) | Request came from `hx-boost` | plan: expose typed |
| `HX-History-Restore-Request` | 1.6 (TBC) | History cache miss restore | plan: expose typed |

**Every one of these is attacker-controlled.** A `curl` can send
`HX-Request: true`. They select a *rendering*; they never decide access. This is
repeated from [Security](#security) because it is the single easiest mistake to
make with htmx on the server.

### Response headers — what we send back

| Header | Since | Effect | Kernway |
|---|---|---|---|
| `HX-Trigger` | 1.0 | Fire client-side events | plan: typed builder |
| `HX-Trigger-After-Settle` | 1.0 | Fire after the settle step | plan: typed builder |
| `HX-Trigger-After-Swap` | 1.0 | Fire after the swap | plan: typed builder |
| `HX-Redirect` | 1.0 | Full browser redirect | plan: typed builder |
| `HX-Refresh` | 1.0 | Full page reload | plan: typed builder |
| `HX-Push-Url` | 1.6 (TBC) | Push a URL into history | plan: typed builder |
| `HX-Replace-Url` | 1.8 (TBC) | Replace the current history entry | plan: typed builder |
| `HX-Location` | 1.6 (TBC) | Client-side navigation without reload | plan: typed builder |
| `HX-Retarget` | 1.7 (TBC) | Override the target element | plan: typed builder |
| `HX-Reswap` | 1.8 (TBC) | Override the swap strategy | plan: typed builder |
| `HX-Reselect` | 1.9 (TBC) | Choose what part of the response to swap | plan: typed builder |

### The 1.x → 2.x break

htmx 2.0 is a major version and did move things. What matters for a server:

- The **core request and response header vocabulary is stable across 1.x and 2.x**
  — which is why one `kernway-htmx` can serve both.
- What changed in 2.x is largely client-side: dropped legacy browser support,
  some attribute syntax, and moving WebSocket and SSE out of the core into
  extensions. A server that speaks the `HX-*` headers is mostly unaffected.
- The SSE and WebSocket extension move **does** matter to us, because
  `kernway-sse` is a sibling crate. Which extension version an app is running
  changes what the SSE endpoint should emit, and that needs pinning down before
  Phase 4.

### The rule

Two things, both enforceable:

1. **Never claim a version we do not test.** A row in the matrix is backed by an
   integration test that drives the real htmx build, or it says "best effort".
2. **An unknown `HX-*` request header is ignored, never rejected.** A newer htmx
   sending a header this Kernway does not know about must degrade to plain HTML,
   not to a 400. Forward compatibility is the whole reason the matrix can stay
   honest instead of aspirational.

## Future: the hot-reload host

Preserved from the previous version of this document. This describes a **v0.5
plan**, not the current crate, and is subject to the naming question below.

A pre-compiled binary loads the user app as a dynamic library (`.so`/`.dll`/
`.dylib`), so a rebuild reloads without restarting the server.

```text
kernway-server (pre-compiled, never rebuilds)
│
├── libloading::Library    ← dlopen user app .so
├── notify::Watcher        ← watch target/debug/*.so
├── Arc<Library>           ← reference-counted for graceful drain
│
└── On file change:
    1. Build new .so (cargo build --lib, ~2-5s)
    2. Wait for in-flight requests to drain (Arc refcount = 0)
    3. dlclose old .so
    4. dlopen new .so
    5. App ready with new code
```

The user app exposes entry points through a stable C ABI defined in
`kernway-abi`, since the Rust ABI is not stable across compilations:

```rust
#[no_mangle]
pub extern "C" fn kernway_create_app() -> *mut dyn KernwayApp { ... }

#[no_mangle]
pub extern "C" fn kernway_destroy_app(app: *mut dyn KernwayApp) { ... }
```

**State across a reload**: in-memory state resets; database state persists;
sessions survive only if they are in Redis rather than in memory; environment
variables persist. Seeding the database on reload is the practical development
pattern.

**Limitations**: debug symbols do not reload, so attaching a debugger needs a
restart. Hot reload is a `kernway dev` feature only — `kernway build` produces a
single static binary with no dynamic loading.

## Open questions

- **Naming.** Two different things are called `kernway-server`: the HTTP server
  crate that exists, and the hot-reload host planned for v0.5. One must be
  renamed before v0.5 — `kernway-host` or `kernway-dev-server` for the latter is
  the obvious direction, since the HTTP server owns the established name.
- **Model representation.** A materialised `Value` tree is simple and lets any
  engine walk it, at the cost of allocating per request. A lazy `ValueSource`
  trait avoids the allocation and is markedly harder to render against,
  especially for iteration. Undecided; needs a KEP and a benchmark, in that
  order.
- **Fragment addressing syntax.** Thymeleaf uses `template :: selector`. Adopt
  it, or use something more Rust-shaped?
- **SPA fallback default.** Automatic `index.html` when a static mount exists is
  convenient but silently turns a mistyped `/api/usres` into a 200 with HTML,
  hiding a routing bug. Recommended default is opt-in
  (`.spa_fallback("index.html")`); not yet decided.
- **404 override.** Related to the hardcoded JSON 404 above — is it a bean, a
  builder method, or a mount?
- **`KernwayPlugin` is in the wrong crate, and does nothing.** As shipped it
  declares `name()` and `version()` and no hook; there are zero implementations
  and zero call sites, and `AppBuilder` has no `.plugin()` method — yet it is
  re-exported in two preludes, so it reads as a feature. Worse, it *cannot* grow
  a useful hook where it lives: a plugin's job is to contribute to app assembly
  (routes, layers, beans, mounts), and `AppBuilder` lives in `kernway-server`,
  which `kernway-core` must never depend on
  ([KEP-0000 §1](../../kep/0000-principles.md#1-ours--write-it-do-not-import-it)). Either
  move the trait to `kernway-server`, or define an `AppRegistrar` abstraction in
  `kernway-core` for it to take. Moving looks right — assembly is not vocabulary
  — but it is a decision, not an obvious cleanup.

## Related KEPs

| KEP | Bearing on this module |
|---|---|
| [0001](../../kep/0000-principles.md#4-stable--never-block-never-surprise) | Why handlers must not block, and why the handler future need not be `Send` |
| [0002](../../kep/0000-principles.md#1-ours--write-it-do-not-import-it) | Why the engine is a trait and the server must not depend on one |
| [0003](../../kep/0001-respect-rust.md) | Why the 404 handler and every other default must be overridable |
| *(planned)* 0005 | Async handlers and `Body` streaming |
| *(planned)* 0006 | Model representation for templates |
