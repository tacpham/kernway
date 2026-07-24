---
kep: 0005
title: Request-scoped DI beans — a per-request scope over the application context
status: Accepted
created: 2026-07-24
decided: 2026-07-24
---

# KEP-0005: Request-scoped DI beans

## Summary

`di-core` gains a **request scope**. `RequestScope` is a [`Container`] layered over
the application `AppContext`: it holds beans that live for exactly one request — the
`SecurityContext` (KEP-0004), the CSRF token, a request id, the `Request` itself —
resolving those locally and falling back to the parent for application singletons.
Handlers and middleware receive a `&RequestScope` instead of a bare `&AppContext`,
so per-request state becomes `#[inject]`-able rather than smuggled through the
request by hand. Because a Kernway task is pinned to its core for its whole life, the
scope is created and dropped on one thread with no cross-thread sharing — none of
the `ThreadLocal` + scoped-proxy machinery Spring needs.

[`Container`]: ../../crates/di-core/src/container.rs

## Motivation

KEP-0004 ends with a `SecurityContext` that an auth middleware builds and a handler
and a template both need to read. There is nowhere to put it. A handler is called
with `&AppContext`, which holds only **application-scoped singletons** — every bean
lives for the life of the process. Per-request state has three bad homes today:

- **A field on `Request`** — works, but every new kind of per-request state (auth,
  csrf, request id, a per-request DB transaction) grows the `Request` struct and
  couples it to features it should not know about.
- **A string-keyed extension map** on the request — the actix approach; untyped, and
  the handler reaches in by a magic key instead of asking for a type.
- **A closure capture** — the login example captures an `Arc<Kernleaf>`; fine for one
  thing, but it does not scale to "the auth middleware produced a value the handler
  needs" without threading it manually.

The `Container` trait in `di-core` already anticipates the fix — its doc names "a
child/scoped context that falls back to a parent" as a thing `Buildable::build` is
abstracted over. This KEP builds that child.

Expected outcome, concrete: an auth middleware writes `scope.set(security_context)`,
and a handler declares `#[inject] user: SecurityContext` (or reads
`scope.get::<SecurityContext>()`) and gets *this request's* value; the template's
`RenderContext` is filled from the scope, not passed by hand. Nothing per-request
lands on `Request` or in a string map.

## Guide-level explanation

A bean can now be **request-scoped**: it is created (or set) at the start of a
request and dropped at its end, and each request sees its own.

```rust
// The auth middleware turns the session cookie into a SecurityContext and puts it
// in the request scope.
fn handle(&self, req, scope, next) {
    let ctx = self.sessions.authenticate(req.header("cookie"));
    scope.set(ctx);                 // request-scoped
    next.call(req, scope)
}

// A handler asks for it — the container gives it this request's value.
#[route(GET, "/me")]
fn me(#[inject] user: SecurityContext) -> Json<Profile> { … }
```

Application singletons keep working exactly as before: the `SessionManager`, a
repository, a `Kernleaf` engine are registered once and resolved from the parent
context. The scope only adds a layer in front for the per-request handful.

The mental model: resolution walks **request scope first, then the application
context**. A request-scoped `SecurityContext` shadows nothing (there is no app-wide
one); an application `UserRepository` resolves straight through to the parent.

## Reference-level explanation

### The scope

```rust
pub struct RequestScope<'a> {
    local:  TypeIdMap<Arc<dyn Any + Send + Sync>>, // beans set for this request
    parent: &'a AppContext,                         // application singletons
}

impl<'a> RequestScope<'a> {
    pub fn new(parent: &'a AppContext) -> Self;
    pub fn set<T: Any + Send + Sync>(&mut self, value: T);   // put a request bean
    pub fn insert<T: Any + Send + Sync>(&mut self, value: Arc<T>);
}

impl Container for RequestScope<'_> {
    fn get<T>(&self) -> Result<Arc<T>, DiError> {
        // request-local first, then fall back to the application context
        self.local.get(&TypeId::of::<T>()).cloned()
            .and_then(downcast)
            .map(Ok)
            .unwrap_or_else(|| self.parent.get::<T>())
    }
    // get_as / get_all likewise: union of local + parent.
}
```

Reusing `Container` is the point: `#[inject]` and `Buildable::build` already work
against any `Container`, so a component that depends on both a singleton repo and the
request `SecurityContext` builds against the `RequestScope` with no new resolution
path.

### The request lifecycle

`serve_connection` (or the dispatcher) creates one `RequestScope` per request over
the shared `AppContext`, threads it through the middleware chain and into the
handler, then drops it when the response is written:

```text
request → RequestScope::new(&app_ctx)
        → Layer::handle(req, &mut scope, next)   // may scope.set(...)
        → … → handler(req, &scope)               // #[inject] resolves from scope
        → Response
        → drop(scope)                            // request beans freed
```

This changes two signatures — the **breaking part**, and the cost the decision
accepted:

- Handler: `Fn(&Request, &AppContext) -> Response` becomes
  `Fn(&Request, &RequestScope) -> Response`.
- `Layer::handle` gains the scope, so middleware can set beans before the handler
  runs. `Next::call` carries it forward.

Every handler, middleware, and example is touched. Since `RequestScope` derefs to /
falls back to the parent, a handler that used the `AppContext` only for singletons
needs no logic change — just the parameter type.

### Why no `ThreadLocal`, and Arc vs Rc

Spring's request scope needs a `ThreadLocal` and a scoped proxy because a request can
hop threads and a singleton must resolve a per-request bean lazily. A Kernway task is
**pinned to its core and never migrates** (thread-per-core), so the scope is a plain
value threaded down one call stack on one thread — no thread-local lookup, no proxy.
This is the concrete cash-out of the "request-scoped state needs no synchronization"
row in [BENCHMARKS.md](../design/BENCHMARKS.md).

Request beans are stored as `Arc<dyn Any + Send + Sync>` to reuse the `Container`
trait unchanged. On one core with one task, that `Arc`'s refcount is **uncontended**
— the atomics never bounce between cores — so the theoretical `Rc` win is a couple of
uncontended atomic ops, not a real cost. Keeping `Arc` avoids a second, parallel
resolution path just for the scope. If a bench later shows the atomics, an `Rc`-typed
scope is a contained change behind the same `set`/`get` surface.

### Setting vs auto-constructing

Two ways a request bean appears:

- **Set explicitly** by middleware — the `SecurityContext` and CSRF token, which are
  *derived* from the request (the session cookie), not default-constructed. This is
  the first cut.
- **Auto-constructed** per request from its `Buildable` — `#[component(scope =
  request)]`, lazily built on first `get` against the scope. A follow-on; not needed
  to wire sessions.

## Drawbacks

**It is a breaking change to every handler and middleware signature.** The
`&AppContext` parameter becomes `&RequestScope`, and `Layer`/`Next` grow the scope.
This touches every handler, every `#[route]`, every example, and the server's
dispatch loop — the largest blast radius of any KEP so far, and the reason it was
weighed as "big but worth it". The mitigation is that the *body* of most handlers is
unchanged (the scope falls back to the parent), but the churn is real and mechanical.

**A per-request allocation.** Each request now allocates a `RequestScope` with a
`TypeIdMap`. It is empty until a bean is set, so the common cost is a small struct on
the stack plus a map that only allocates on first `set` — but it is not nothing, and
it lands on the per-request path the pipeline benchmark measures. It must be measured
there, not assumed free.

**`Arc` on a single thread.** Request beans pay an atomic refcount they would not need
if the design committed to `Rc`. Uncontended, but present; a reader who wants the
last nanosecond would prefer `Rc`, and we are trading it for one resolution path.

**Two contexts in flight.** A handler now has a `RequestScope` *and*, transitively,
the `AppContext` behind it. "Which context do I hold?" is a new question a developer
did not have when there was only one. Documented, but a concept to learn.

## Rationale and alternatives

**A string-keyed request extension map (actix-style).** Put a typemap on `Request`,
`req.extensions().get::<SecurityContext>()`. Rejected as the primary mechanism: it is
a *second* resolution system next to DI, untyped at the call site by convention, and
it does not compose with `#[inject]` — a component that wants both a singleton and a
request value would resolve them two different ways. A request-scoped `Container`
unifies them under the DI the framework already has.

**A field on `Request`.** Simplest, and fine for one or two things. Rejected because
it couples `Request` (a `kernway-core` type) to every feature that wants per-request
state — auth, csrf, tracing, a request-scoped transaction — and grows without bound.
The scope keeps `Request` about the HTTP request.

**Thread-local, Spring-style.** Rejected because thread-per-core makes it unnecessary
machinery: the task never leaves its core, so a plain threaded value is enough, and a
thread-local would be a slower, spookier way to pass something we can pass directly.

**Do nothing — keep threading it by hand.** The login example already shows the pain:
the handler captures the engine in a closure, and there is no path for a middleware to
hand the handler a value. Every app would invent its own request-extension convention,
incompatibly. A framework that has DI should extend it to the request, not leave a
gap beside it.

## Prior art

- **Spring `@RequestScope`** — a `ThreadLocal`-backed scope with a scoped proxy so
  singletons can hold a request bean. The canonical design, and the one thread-per-core
  lets Kernway simplify away.
- **ASP.NET Core scoped services** — a per-request child DI container, disposed at
  request end. Almost exactly this design; `RequestScope` is that child container.
- **actix-web request extensions** — a typemap on the request. Fast and simple, but
  separate from DI, which is the seam this KEP closes.
- **Rails `ActiveSupport::CurrentAttributes` / Go `context.Context`** — per-request
  state carried explicitly (Go) or via thread-local (Rails). Go's explicit `Context`
  is the closest in spirit to threading a `RequestScope` down the call stack.

## Unresolved questions

- **Auto-constructed request beans** — the `#[component(scope = request)]` lazy-build
  path is deferred; the first cut is middleware-set beans only.
- **Signature shape** — pass `&RequestScope` alone (it falls back to the parent), or a
  `(&RequestScope, &AppContext)` pair? Leaning on the scope alone, since it *is* a
  `Container` over the parent.
- **`Rc` vs `Arc`** — kept `Arc` for one resolution path; revisit if the pipeline
  bench shows the atomics.
- **Nested scopes** — a background job spawned from a request wanting its own scope.
  Out of scope here; the parent-fallback design already allows it later.

## Future possibilities

- **Request id, tracing span, access log context** as request-scoped beans — the
  logging MDC story, made injectable.
- **A request-scoped DB transaction** — open on first inject, commit/rollback on
  response, the `@Transactional`-per-request shape.
- **The `Request` itself as an injectable** — `#[inject] req: Request` instead of the
  positional parameter, once the scope holds it.
- **Auto-wired `SecurityContext`** — a standard auth layer that sets it, so an app
  gets `#[inject] user: SecurityContext` by enabling a feature, not writing middleware.
