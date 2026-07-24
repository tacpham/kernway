# kernway-htmx — the `HX-*` header vocabulary, typed both ways

## Purpose

Give a handler typed access to the `HX-*` headers [htmx](https://htmx.org) uses,
in both directions: read the ones a client sends ([`Htmx`]), build the ones the
server sends back ([`HtmxResponse`]). No string header names in application code,
a typo is a compile error, and the one caching mistake every htmx server can make
— returning both a fragment and a full page from one URL without a `Vary` — is
handled by construction.

The crate rests on one honest observation: **htmx is a client library, and a
server does not "render htmx."** It renders HTML — a full page or a fragment — and
speaks a small, fixed header vocabulary. That vocabulary is the entire surface
here. There is no runtime, no engine, nothing to keep in sync with a JavaScript
release.

**Not** in scope: rendering HTML (that is a template engine's job, or a plain
string), escaping (the producer of the markup owns that), routing, or shipping the
htmx script (it is a client asset the app serves like any other static file).

## Supported htmx version

**htmx 2.0.x** — the current major release, stated not implied
([KEP-0000 §2](../../kep/0000-principles.md#2-fast--measured-or-it-is-not-a-claim):
a claim of "htmx support" with no version is unverifiable). The `HX-*` header
vocabulary is **stable across htmx 1.9+ and 2.0**, so a 1.9 client is served
correctly by the same code; what 2.0 changed is client-side (dropped legacy
browsers, WebSocket/SSE moved to extensions) and does not touch a server that
speaks these headers. Every header name in the crate was verified against the
htmx 2.0.x reference and against `axum-htmx` 0.8's constants.

Forward-compatibility rule: an `HX-*` header this crate does not model is
**ignored, never rejected** — a newer htmx degrades to plain HTML, never a 400.

## Status

As of 2026-07-24. Landed in the M3 slice.

| Area | State | Notes |
|---|---|---|
| Request extraction (`is_request`, `boosted`, `history_restore`, `target`, `trigger`, `trigger_name`, `current_url`, `prompt`) | ✅ | borrowed `&str`, no allocation |
| `respond(fragment, full_page)` — pick shape + auto `Vary` | ✅ | the caching-correct combinator |
| Response headers (trigger ×3, redirect, location, refresh, push/replace URL, retarget, reswap, reselect) | ✅ | `HtmxResponse` builder |
| `Swap` enum (8 variants) | ✅ | `HX-Reswap` / `hx-swap` tokens |
| `Vary: HX-Request` appended, not clobbered | ✅ | preserves an existing `Vary` |
| Benchmarked vs `axum-htmx` 0.8 + `http` substrate | ✅ | see Speed |
| Wired into the meta-crate as `features = ["htmx"]` | ✅ | off by default, zero cost when off |

## Standards

| Spec | Scope | Compliance |
|---|---|---|
| htmx 2.0.x request headers | `HX-Request`, `HX-Boosted`, `HX-Target`, `HX-Trigger`, `HX-Trigger-Name`, `HX-Current-URL`, `HX-Prompt`, `HX-History-Restore-Request` | full |
| htmx 2.0.x response headers | `HX-Trigger{,-After-Settle,-After-Swap}`, `HX-Redirect`, `HX-Location`, `HX-Refresh`, `HX-Push-Url`, `HX-Replace-Url`, `HX-Retarget`, `HX-Reswap`, `HX-Reselect` | full |
| RFC 9110 §12.5.5 (`Vary`) | mark responses that differ by `HX-Request` | full — appends to any existing `Vary` |

Every row is a unit test — the extraction accessors, the fragment/page choice, the
`Vary` append, each `HX-*` header — per
[KEP-0000 §3](../../kep/0000-principles.md#3-solid--correct-at-the-edges-or-not-correct).

## Architecture

```text
request                                    response
   headers                                    HtmxResponse::new(html)
      │                                            │  .trigger(...) .retarget(...)
      ▼  Htmx::from(&req)                          ▼  .reswap(Swap) ...
   is_request()? boosted()? target()? ...       set HX-* on Response.headers
      │                                            │
      ▼  hx.respond(|| fragment, || full_page)     ▼  .into_response()
   is_request && !history_restore                append Vary: HX-Request
      ? fragment()  : full_page()                   (never clobber existing)
      │                                            │
      └──────────────► HtmxResponse ◄──────────────┘
                       (Vary set)
```

`Htmx` borrows the request; every accessor is one lookup on the one-buffer
`Headers` from `kernway-core`. `HtmxResponse` wraps a `Response` and each builder
method is one `Headers::insert`. Both sides are thin — the data structures they
lean on already exist and are already measured.

## Public surface

```rust
pub const HTMX_VERSION: &str = "2.0.x";

pub struct Htmx<'r> { /* &Request */ }
impl<'r> Htmx<'r> {
    pub fn from(req: &'r Request) -> Self;         // also `new`
    pub fn is_request(&self) -> bool;
    pub fn is_boosted(&self) -> bool;
    pub fn is_history_restore(&self) -> bool;
    pub fn target(&self) -> Option<&str>;          // borrowed, no alloc
    pub fn trigger(&self) -> Option<&str>;
    pub fn trigger_name(&self) -> Option<&str>;
    pub fn current_url(&self) -> Option<&str>;
    pub fn prompt(&self) -> Option<&str>;
    pub fn respond(&self, fragment: impl FnOnce() -> String,
                          full_page: impl FnOnce() -> String) -> HtmxResponse;
}

pub struct HtmxResponse { /* Response + vary flag */ }
impl HtmxResponse {
    pub fn new(html: impl Into<String>) -> Self;
    pub fn from_response(inner: Response) -> Self;
    pub fn vary_on_request(self) -> Self;
    // events: trigger / trigger_after_settle / trigger_after_swap
    // nav:    redirect / location / refresh / push_url / replace_url
    // target: retarget / reswap(Swap) / reselect
}
impl IntoResponse for HtmxResponse { /* appends Vary: HX-Request */ }

pub enum Swap { InnerHtml, OuterHtml, BeforeBegin, AfterBegin,
                BeforeEnd, AfterEnd, Delete, None }
```

**Stability**: the shape is stable. `Swap` and the builder may gain methods as
htmx adds headers — additive, not breaking.

## Integration

**Depends on**: `kernway-core` only. htmx is a header vocabulary, not a runtime,
so there is nothing external to import
([KEP-0000 §1](../../kep/0000-principles.md#1-ours--write-it-do-not-import-it)).

**Depended on by**: the meta-crate, behind `features = ["htmx"]` — off by default,
pulls in nothing when off. `Html<T>` (the response type htmx handlers return) lives
in `kernway-web` and is baseline, so the plain-HTML case needs no feature at all.

**Must never depend on**: `kernway-server`, a template engine, or the htmx script.
Extraction and header-building are pure and stay unit-testable without a socket.

## Speed

Measured head-to-head against `axum-htmx` 0.8 (the dedicated Rust crate) and the
raw `http::HeaderMap` substrate every axum/actix/warp app shares. Same 8-header
request each round.

| Path | kernway | axum-htmx | http substrate | Bench |
|---|---|---|---|---|
| extract 4 values (`is_request`+`boosted`+`target`+`trigger`) | **57.8 ns** | 80.2 ns | 85.1 ns | ✅ `htmx/extract` |
| build reply (body + CT + 3 `HX-*` + `Vary`) | **240.9 ns** | — | 280.4 ns | ✅ `htmx/respond` |
| full turn (extract + reply) | **176.7 ns** | 180.5 ns | — | ✅ `htmx/turn` |

Numbers from [BENCHMARKS.md](../BENCHMARKS.md).

**Reading is the clear win, 1.39× over the dedicated crate**: the one-buffer
`Headers` does not hash each name, and `target()`/`trigger()` return a borrowed
`&str` where `axum-htmx`'s `HxTarget`/`HxTrigger` allocate an `Option<String>`
every time. **Writing is a narrower 1.16×** — both allocate the HTML body, which
dominates. **Over a whole turn it is a tie (~2%)**, and that is the honest reading:
the htmx layer is ~180 ns next to the ~350–600 ns the pipeline already spends
parsing and routing, so it is never the bottleneck on either framework. The crate
earns its place on *correctness-by-construction and zero allocation*, and is
faster where it can be — not on a throughput claim it cannot make.

**Allocation policy**: extraction allocates nothing. `HtmxResponse` allocates the
body once and grows the header buffer; no per-header allocation.

## Generic — the extension points

None, deliberately. This is a leaf crate over a fixed vocabulary. A team that
speaks a private `X-…` protocol on top of htmx sets those headers through the
`Response` directly — there is nothing to configure here, and that is the point.

## Security

Every `HX-*` **request** header is attacker-controlled — a `curl` can send
`HX-Request: true`. The crate's contract is that these headers decide *how to
render*, never *whether to allow*.

| Threat | Answer | Tested |
|---|---|---|
| Forged `HX-Request` used for authz | out of contract — these headers gate rendering only, never authorisation | doc-stated |
| Fragment served into a bare page by a cache | `respond()` sets `Vary: HX-Request` automatically | ✅ |
| An existing `Vary` clobbered (e.g. `Accept-Encoding`) | appended to, not replaced | ✅ |
| Header-injection via a value with CRLF | the `Headers` encoder's concern, not bypassable here | (server-side) |

No ❌ rows — per
[KEP-0000 §3](../../kep/0000-principles.md#3-solid--correct-at-the-edges-or-not-correct),
this repository is public, so an unmitigated security row would be a published
attack recipe and may not ship.

## Direction

| Phase | Goal | Blocked by |
|---|---|---|
| M3 | extraction + response builder + auto `Vary`, benchmarked vs incumbent | — (done) |
| M4 | pairs with a template engine (kernleaf) so a fragment is a rendered partial, not a hand-built string | template engine |
| later | an optional `auto-vary` layer that sets `Vary` for a whole route group without the `respond()` combinator | a real need + the layer API |

**Deliberately out of scope**: rendering, escaping, and serving the htmx script.
A fragment is just HTML; who produces it is the app's choice.

## Open questions

- `is_request()` checks `== "true"`; `axum-htmx` checks header *presence*. htmx
  always sends `true`, so both agree in practice — is the stricter check worth the
  one-in-a-million divergence, or should presence be enough?
- Should `respond()` grow a variant that takes already-rendered `Html<T>` values
  instead of `String` closures, once a template engine lands in M4?

## Related KEPs

| KEP | Bearing |
|---|---|
| [0000 §1](../../kep/0000-principles.md#1-ours--write-it-do-not-import-it) | Why the only dependency is `kernway-core` |
| [0000 §2](../../kep/0000-principles.md#2-fast--measured-or-it-is-not-a-claim) | Why the version is stated and the crate is benchmarked against `axum-htmx` |
| [0000 §3](../../kep/0000-principles.md#3-solid--correct-at-the-edges-or-not-correct) | Why every header and the `Vary` rule is a test |
| [0001](../../kep/0001-respect-rust.md) | Borrowed `&str` extraction — respecting Rust's ownership rather than allocating like a GC'd port would |

[`Htmx`]: ../../../crates/kernway-htmx/src/lib.rs
[`HtmxResponse`]: ../../../crates/kernway-htmx/src/lib.rs
