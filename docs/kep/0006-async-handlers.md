---
kep: 0006
title: Async handlers — a handler that can await
status: Accepted
created: 2026-07-25
decided: 2026-07-25
---

# KEP-0006: Async handlers

## Summary

Handlers become **uniformly async**. The [`Handler`] type changes from
`Fn(&Request, &RequestScope) -> Response` to one returning a **future**, so a
handler can await a database query, the Redis session store (KEP-0004), or an
upstream HTTP call without blocking its core. There is **no sync handler path** —
the framework is async from the socket to the handler, one model, matching its
"async everywhere, never block" premise rather than hedging with a dual
sync/async surface. The sync `Middleware` chain becomes async (adopting
kernway-core's already-async `Layer`/`Next`), `handle` becomes an `async fn` the
connection task awaits, and an `IntoHandler` helper boxes a handler's `impl
Future` so authors write an `async {}` block, not a manual `Box::pin`. This
finishes the handler-signature question KEP-0002 deferred, and is the piece the
async `SessionStore` (Redis) has been waiting on.

**This is a hard break**: every handler is rewritten to async. That is the
deliberate price of one async model over a compatibility hedge, taken while the
project is pre-1.0 and every handler is ours.

[`Handler`]: ../../crates/kernway-server/src/router.rs

## Motivation

Handlers are synchronous: `Fn(&Request, &RequestScope) -> Response`. The connection
task is async and the socket I/O is async, but the moment a request is dispatched,
`handle` runs to completion synchronously. So a handler that needs I/O has only bad
options:

- **Block the core.** A blocking DB or Redis call on a thread-per-core runtime
  stalls *every* connection pinned to that core until it returns — the one thing
  KEP-0000 §4 forbids.
- **`spawn_blocking`.** Offload to the blocking pool, as the static file read does.
  Right for a genuinely blocking API, but a thread hop per call is the wrong cost
  for something that is *already* async (a Redis client, an async DB driver, an
  HTTP client) — you would be un-asyncing an async thing.

Concretely, this blocks KEP-0004: the async `SessionStore` (Redis) "turns the trait
async", but a sync handler and a sync `Middleware` cannot `.await` it. Redis
sessions are stuck behind this KEP. More broadly, a framework whose pitch is "async
everywhere, never block" cannot have its handlers be the one place you cannot await.

Expected outcome: a handler can `await` an async call and the core keeps serving
other connections while it is pending; the async `SessionStore` becomes reachable;
and — the compatibility bar — every handler written today still compiles.

## Guide-level explanation

Every handler is async — it returns an `async {}` block, whether or not it awaits:

```rust
.get("/health", |_req, _scope| async { Response::new(StatusCode::OK) })

.get("/users/{id}", |req, scope| async move {
    let db = scope.get::<Db>().unwrap();
    let user = db.find(path_id(req)).await;          // await, no blocking
    Json(user).into_response()
})
```

`.get(...)` takes anything implementing `IntoHandler` — a `Fn(&Request,
&RequestScope) -> F where F: Future<Output = Response>` — and boxes the future, so
the author writes the `async` block and not the `Box::pin`. A handler that does no
I/O still writes `async { … }`; the `#[route]` macro generates it for the attribute
form, so `async fn get_health(...) -> Response` reads naturally there.

Middleware likewise gains `await`:

```rust
async fn handle(&self, req, scope, next) -> Response {
    let ctx = self.sessions.authenticate(req.cookie()).await;   // async session store
    scope.set(ctx);
    next.call(req, scope).await
}
```

The mental model shift: "the handler returns a `Response`" becomes "the handler
returns a `Response`, eventually." Everything downstream — the middleware chain, the
dispatcher — is now `.await`ed, which the connection task (already async) does
naturally.

## Reference-level explanation

### The handler type

A trait object cannot return `impl Future`, so the handler returns a boxed future
that borrows the request and scope for the duration of the call:

```rust
pub type Handler =
    Arc<dyn for<'r> Fn(&'r Request, &'r RequestScope<'_>) -> BoxFuture<'r, Response>
        + Send + Sync>;
```

The `for<'r>` is the higher-ranked lifetime tying the returned future to the borrows
it holds; `BoxFuture<'r, Response>` is `Pin<Box<dyn Future<Output = Response> + Send +
'r>>` (the type kernway-core's `layer` already defines). `Send` because the
connection task's future must be `Send` (rt-net requirement), which is why
`RequestScope` is `Send + Sync` (KEP-0005).

### `IntoHandler` — one impl, boxing the future

`.get(...)` and friends take `impl IntoHandler`, which produces the boxed `Handler`
from a `Fn(&Request, &RequestScope) -> F where F: Future<Output = Response> + Send`
by wrapping the return as `Box::pin(fut)`. One blanket impl, not two — there is no
sync `-> Response` shape to also accept, because handlers are uniformly async. The
helper exists only so the author writes an `async` block rather than the `Box::pin`
themselves.

### Middleware becomes async — unify on `Layer`

The sync `kernway-server::Middleware` and the async `kernway-core::Layer`/`Next`
(which already return `BoxFuture`) are the same concept at two speeds. Async
handlers make the sync one untenable, so the server adopts the async `Layer`: `Layer::handle(&self, req, scope, next) -> BoxFuture<Response>`, `Next::call(req,
scope) -> BoxFuture<Response>`. `Layer` gains the `RequestScope` parameter (KEP-0005)
it did not have. The two built-ins (request-id, logging) and any user middleware
become async — trivial (`Box::pin(async move { … next.call().await … })`).

### The dispatcher and the connection task

`handle` becomes `async fn handle(request, router, context, middlewares) -> Response`.
It creates the `RequestScope` (KEP-0005), walks the async middleware chain, and
`.await`s the handler's future. The one call site in `serve_connection` (already
async) changes from `handle(...)` to `handle(...).await`. The `RequestScope` lives in
`handle`'s async frame and the handler future borrows it — awaited within the same
frame, dropped after, so the lifetimes close.

### What this KEP does not specify

- **Async `FromRequest` extractors.** Extractors stay sync for now; an extractor
  that needs I/O is a later extension.
- **Streaming request bodies.** The body is still fully buffered before dispatch;
  async streaming *in* is separate from an async handler.
- **The `Body::Stream` variant** for async-generated response bodies (KEP-0002's
  future note) — an async handler returns a whole `Response`, not a stream, here.

## Drawbacks

**A boxed future per request.** Every handler allocates a `Box<dyn Future>`, even
one that does no I/O — the cost of a dynamic router of async handlers (a
trait-object cannot return an unboxed `impl Future`). On the ~350 ns pipeline that
box is a real, measurable addition on the hot path the pipeline bench guards. It is
**inherent to a dynamic router**, not something a sync path could dodge (there is no
sync path); axum and tower box the same way and are fast enough, which is the
evidence it is affordable. The first cut measures it; the only escape is giving up
the dynamic router (monomorphised routes), which a web framework will not do.

**Async ergonomics leak in.** A handler that awaits is `|req, scope| async move {
… }`, which needs async closures (stable in recent Rust) or the `#[route]` macro to
generate the boxing. A reader coming from the sync `-> Response` form has a new
shape to learn, and lifetime errors on futures that outlive their borrows are a
sharper edge than sync code has.

**Every middleware migrates.** Folding the sync `Middleware` into the async `Layer`
touches both built-ins, the dispatcher, and any user middleware — churn on top of
the handler change, in the same release.

**The `Send` bound bites.** The handler future must be `Send`, so anything a handler
holds across an `.await` must be `Send` — a `!Send` value (an `Rc`, a `RefCell`
guard) held across an await is a compile error the sync form never produced. This is
the standard async-Rust tax, arriving in handler code.

## Rationale and alternatives

**Hybrid: `IntoHandler` accepting both a sync `-> Response` and an async handler.**
The earlier draft of this KEP, to avoid rewriting every existing handler: a sync
handler wraps as a ready future, an async one boxes. Rejected in favour of pure
async because it dilutes the "async everywhere" model into two shapes a reader must
hold, and it does not even save the box (the ready future boxes too). Pure async is
one model at the cost of a one-time rewrite of handlers that are all ours; the
compatibility a hybrid buys is not worth a permanent dual surface in a framework
whose whole pitch is async.

**Keep handlers sync; use `spawn_blocking` for I/O.** The status quo plus a thread
hop. Rejected as the default: it un-asyncs async drivers, and the thread hop per
Redis call is exactly the cost thread-per-core exists to avoid. `spawn_blocking`
stays for genuinely blocking APIs (the file read), not for async ones.

**Make only middleware async, keep handlers sync.** Then the auth middleware could
await the session store, but a handler still could not await its own DB call.
Half a solution; the async need is not middleware-only.

**Do nothing.** Redis sessions stay unreachable, and every handler that needs I/O
either blocks a core or reaches for `spawn_blocking`. A framework that renders a
page, guards it, and logs a user in, but cannot `await` a query, has left the most
common real handler in the cold.

## Prior art

- **axum / tower** — handlers are `async fn` returning `impl IntoResponse`; the
  service is a `tower::Service` returning a future. The `IntoHandler` idea here is
  axum's `Handler` trait: many function shapes, one boxed service. axum boxes too,
  and it is fast enough — evidence the box is affordable.
- **actix-web** — async handlers, `Responder` futures, on an arbiter-per-core model
  close to thread-per-core. Confirms async handlers and per-core scheduling compose.
- **Spring WebFlux** — the reactive `Mono<ResponseEntity>` handler, the async twin
  of the servlet `@Controller`. Kernway does not keep two worlds (KEP-0004 rationale)
  — `IntoHandler` folds sync and async into one registration instead.
- **Go `net/http`** — handlers are sync on a goroutine-per-request model, where a
  blocking call parks a cheap goroutine, not an OS thread. Kernway lacks green
  threads, so it needs the explicit future that Go hides behind the scheduler.

## Unresolved questions

- **Box vs enum** — *resolved: keep the box.* With the first cut landed, the
  `pipeline` bench measures the full parse → route → handle → encode path at
  ~404 ns (static) / ~668 ns (param), the boxed future + request scope + a single
  poll included. The box is a small, bounded addition that leaves the pipeline in
  the same ~400 ns range it sat in before, and the dynamic router needs *a* boxed
  future regardless — an enum would not remove the allocation, only the vtable.
  Not worth the complexity.
- **Async closures vs macro boxing** — whether `.get(|req, scope| async { … })`
  relies on stable async closures or the `#[route]` macro wraps a plain `async fn`.
  Leaning on async closures where the toolchain allows, with the macro for the
  attribute form.
- **`Layer` ordering and short-circuit** semantics are unchanged, but the async
  `Next` makes "don't call next" (reject early) an early `return` of a ready future
  — confirm the ergonomics.

## Future possibilities

- **Async `SessionStore` + Redis** (KEP-0004) — the immediate unlock.
- **Async `FromRequest`** extractors — an extractor that awaits (a body parse that
  streams, a lookup that hits a cache).
- **`Body::Stream`** — an async handler that streams its response body frame by
  frame (SSE without buffering, chunked proxying), the KEP-0002 future note.
- **A request-scoped async DB transaction** — opened on first inject, awaited to
  commit/rollback on the response, now that both the scope (KEP-0005) and awaiting
  (this KEP) exist.
