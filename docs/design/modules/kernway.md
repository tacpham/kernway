# kernway — the meta-crate and the feature graph

## Purpose

The front door. One dependency line that gives a user everything they need, and
nothing they do not.

This crate contains almost no code. Its job is the **feature graph**: deciding
what a user gets by default, what they opt into, and what those choices cost in
compile time and binary size. That graph is a public API — changing it breaks
builds — so it is designed here rather than accumulated.

The shape it must deliver:

```toml
# A JSON API — nothing extra is compiled
kernway = "0.6"

# A web UI — everything, without learning the feature names
kernway = { version = "0.6", features = ["web"] }

# Picking precisely
kernway = { version = "0.6", features = ["security"] }
```

**Not** in scope: any behaviour. If logic ends up here, it belongs in a real
crate — the meta-crate re-exports and gates, it does not implement.

## Status

As of 2026-07-23. This is the least finished crate in the workspace, and it is
the one every user meets first.

| Area | State | Notes |
|---|---|---|
| Re-export DI (`di-core`, `di-macro`) | ✅ | `AppContext`, `#[derive(Component)]`, `#[inject]` |
| Re-export `kernway-core` traits | ✅ | via `prelude` |
| Re-export `kernway-server` | ❌ | **`KernwayApp` is not reachable through `kernway`** |
| Re-export `kernway-web` | ❌ | `Json<T>`, `Path<T>`, `Query<T>` not reachable |
| Re-export ORM / cache / openapi / sse | ❌ | not dependencies at all |
| `[features]` table | ❌ | **none declared** |
| Used by any example | ❌ | every example bypasses it |

**Today**: `kernway` gives you dependency injection and some traits. It cannot
build a web application — `KernwayApp::builder()` is not reachable through it.

**Not yet**: everything the README's Quick Start implies. `todo-app` declares
**13 crates by path**; `hello-web` declares 7. The single-dependency story is
aspirational, and because no example uses the meta-crate, nothing tests whether
it works.

That last point is the real risk: an untested front door fails for the first
user rather than for us.

## Standards

Cargo conventions, and they are stricter than they look.

| Convention | Rule | Why it bites |
|---|---|---|
| Features are **additive** | A feature may only *add*. Never remove, never change a default. | Cargo unifies features across the whole graph — see [Security](#security). |
| `dep:` syntax (Cargo ≥ 1.60) | Always write `dep:kernleaf`, never bare `kernleaf` | A bare optional dependency silently creates a same-named feature, so users can enable a feature nobody designed. |
| `default = []` is a promise | Adding to `default` later is a **breaking change** for anyone on `default-features = false` | Start minimal; widening is easy, narrowing is not. |
| SemVer | Removing a feature, or narrowing what one enables, is a major bump | A feature name is public API. |
| `doc_cfg` on docs.rs | Every gated item shows which feature provides it | Otherwise docs.rs (which builds `--all-features`) documents items a user cannot reach, with no hint why. |

MSRV is 1.78 (`docs/VERSIONING.md`), comfortably above the 1.60 needed for `dep:`.

## Architecture

### Two independent axes

The thing that makes this graph readable is keeping two questions apart:

- **Crate boundaries** — an architecture question, governed by
  [KEP-0000 §1](../../kep/0000-principles.md#1-ours--write-it-do-not-import-it). Is this a
  separate replaceable piece?
- **Feature flags** — a compilation question. Does everybody pay for it?

They are not the same axis, and conflating them is the usual mistake. A crate can
be **separate and always compiled in**: `kernway-htmx` stays its own crate so a
third party can replace it and so it can be published alone, and it is also a
non-optional dependency, because it costs almost nothing.

### The baseline — what you get with no features at all

Decided 2026-07-23. `default = []` in the sense that no optional dependency is
pulled — but the always-on set is deliberately larger than a bare JSON API:

```text
kernway  (default = [])
├── kernway-core      Request · Response · StatusCode · the traits
├── di-core           AppContext, bean resolution
├── di-macro          #[derive(Component)], #[inject]
├── kernway-http      HTTP/1.1 parse + encode
├── kernway-server    Router · KernwayApp · Middleware
│   └── rt-core, rt-net       (transitive: runtime and transport)
├── kernway-web       Json<T> · Path<T> · Query<T> · Html · ProblemDetail
├── kernway-static    serve a directory, MIME, ETag, conditional GET
└── kernway-htmx      Htmx extractor · HtmxResponse builder · auto Vary
```

**A fresh `kernway = "0.6"` can already serve a static site with htmx
endpoints.** Drop HTML, CSS, and JS into a folder, write handlers that return
`Html`, and htmx calls work — no feature to discover, no name to learn.

Why these three are baseline rather than opt-in:

| Piece | Cost | Reasoning |
|---|---|---|
| `Html` responses | a few lines | If `Json` is baseline, `Html` has no case for being less so |
| `kernway-static` | small, no deps | The "drop files in a folder and deploy" promise is the point of the framework, not an add-on |
| `kernway-htmx` | ~300 lines, no deps | Paying ~30KB to avoid a discovery step is the right trade; the argument for gating it was weak |

A pure JSON API compiles all three and does not use them. That is a real but tiny
cost, and it buys a default that works for the case Kernway is actually for.

### The `web` tier means security

Everything above serves *content*. The moment templates render user data into
HTML and accept form posts back, a different class of problem starts — and that
is exactly the line `web` draws:

```toml
[features]
default = []

# --- bundles ----------------------------------------------------------
web  = ["kernleaf", "security"]
full = ["web", "orm-sqlite", "cache-memory", "openapi", "sse", "multipart"]

# --- capabilities -----------------------------------------------------
security = ["dep:kernway-security"]          # CSRF · security headers
kernleaf = ["dep:kernleaf", "security"]      # an engine implies the security layer

orm          = ["dep:kernway-orm-core", "dep:kernway-orm-macro"]
orm-memory   = ["orm", "dep:kernway-orm-memory"]
orm-sqlite   = ["orm", "dep:kernway-orm-sqlite"]

cache        = ["dep:kernway-cache-core", "dep:kernway-cache-macro"]
cache-memory = ["cache", "dep:kernway-cache-memory"]

openapi      = ["dep:kernway-openapi"]
sse          = ["dep:kernway-sse"]
multipart    = ["dep:kernway-multipart"]
```

`kernleaf = [..., "security"]` is the important edge: **a template engine
enables the security layer, always.** You cannot get HTML rendering of user data
without CSRF protection and security headers coming with it, because a rendering
engine without them is a vulnerability generator.

Why CSRF belongs here and not in the baseline: CSRF is specifically a
*browser-form* attack. A JSON API authenticated by a bearer token is not
vulnerable to it and gains nothing but a token to carry around. Static files and
htmx `GET`s do not need it either. It becomes necessary precisely when a server
renders forms — which is when `kernleaf` arrives.

`security` is separately selectable so an app using a third-party engine still
gets the layer:

```toml
kernway = { version = "0.6", features = ["security"] }
my-xhtml-renderer = "0.1"
```

`orm` and `cache` are the *spec* crates alone: enabling `orm` gets the traits
with no backend, which is what a library crate defining entities wants. A binary
picks `orm-sqlite` or `orm-memory`.

### `web` and `full` exist so nobody has to choose

**Bundles** are what documentation and `kernway new` use — a beginner never
learns a capability name. **Capabilities** are one per crate, for anyone who
cares about compile time or binary size.

## Public surface

### The prelude is feature-aware

```rust
pub mod prelude {
    pub use crate::{AppContext, DiError, Component, component, inject, controller, route};
    pub use crate::{IntoResponse, FromRequest, Layer, Next};
    pub use kernway_core::error::StatusCode;
    pub use kernway_server::{KernwayApp, Router};      // ← missing today
    pub use kernway_web::{Json, Path, Query};          // ← missing today
    pub use std::sync::Arc;

    pub use kernway_static::StaticFiles;                // baseline
    pub use kernway_htmx::{Htmx, HtmxResponse, Swap};   // baseline

    #[cfg(feature = "kernleaf")]
    pub use kernleaf::View;

    #[cfg(feature = "orm")]
    pub use kernway_orm_core::{Entity, Repository, Page, OrmError};
}
```

`use kernway::prelude::*` should be all a normal app needs. If a user has to add
a second `use` for something the feature they enabled provides, the prelude is
incomplete.

### Gated re-exports carry their gate into the docs

```rust
#![cfg_attr(docsrs, feature(doc_cfg))]

#[cfg(feature = "kernleaf")]
#[cfg_attr(docsrs, doc(cfg(feature = "kernleaf")))]
pub use kernleaf;
```

```toml
[package.metadata.docs.rs]
all-features = true
rustdoc-args = ["--cfg", "docsrs"]   # ← to add
```

Without this, docs.rs documents `kernway::kernleaf` for everyone and says nothing
about how to get it.

## Integration

**Depends on** — everything, some of it optionally. That is the point of the
crate, and it is the one place in the workspace where a wide dependency list is
correct rather than a smell.

**Depended on by**: applications, `kernway new` scaffolding, and — once fixed —
every example.

**Must never be depended on by** another workspace crate. A crate reaching *up*
to the meta-crate would make the dependency graph cyclic and would drag optional
features into a place that cannot see them. Worth adding to
`scripts/check-core.sh`: no `crates/*/Cargo.toml` other than examples may list
`kernway` as a dependency.

## Speed

For this crate, speed means **compile time**. It is the whole argument for
`default = []`, and it is currently unmeasured.

| Configuration | Clean build | Incremental | Binary (release) |
|---|---|---|---|
| `default` | ❓ | ❓ | ❓ |
| `web` | ❓ | ❓ | ❓ |
| `full` | ❓ | ❓ | ❓ |

**Budget**, to be confirmed by measurement rather than asserted:

- `default` clean build stays competitive with an equivalent Axum project — the
  15–20s target in `FULL_PLAN.md`.
- The baseline is charged as one number, and it is the one that matters most —
  it is what every user pays. `kernway-static` and `kernway-htmx` are in it, so
  if either is slow to compile, that is a bug rather than a trade-off.
- `full` may be slow. Nobody deploys `full`; it exists for examples and docs.

A `benches/` entry cannot measure this — it needs a script that builds each
configuration cold and records the numbers, run in CI on a schedule rather than
per commit.

**Until those cells have numbers, the compile-time argument for `default = []`
is a hypothesis.** It is a well-founded one, but this charter should not claim
otherwise.

## Generic — the extension points

The feature graph *is* the extension mechanism, and it extends past this crate:
a user who wants something Kernway does not ship adds their own crate beside it.

```toml
kernway = { version = "0.6", features = ["security"] }
my-weird-xhtml-renderer = { path = "../mine" }
```

Nothing in `kernway` needs to know that crate exists — see
[the extension contract](kernway-server.md#the-extension-contract). A feature in
this crate is a convenience for the implementations Kernway happens to ship, not
a gate on what is possible.

`kernleaf` gets a feature flag because it lives in this repository. It gets no
capability that a third-party engine lacks.

## Security

**One rule, and it is the sharpest in this document: a feature may only add.**

Cargo unifies features across the entire dependency graph. If any crate anywhere
in a build enables `kernway/kernleaf`, then *every* crate in that build gets it. The
user cannot opt out, and usually cannot even see who asked.

Now imagine a feature named `no-escape` that turned off template auto-escaping
for a crate that wanted raw output. One transitive dependency enabling it would
disable escaping **for the entire application**, silently, with no diagnostic and
nothing in the user's own `Cargo.toml` to hint at it. That is a cross-crate XSS
switch.

So:

| Rule | Consequence if broken |
|---|---|
| A feature never disables a safety default | A dependency can weaken the app's security |
| A feature never changes behaviour, only adds capability | Behaviour depends on the whole graph, not on your code |
| Opt *out* of safety is a type or a builder call, never a feature | Stays local and visible where it is used |

Where raw output or relaxed checking is genuinely needed, it is a distinct type
the author reaches for at the call site (`kw:utext`, `Html::raw(...)`) — visible
in the code that takes the risk, and scoped to it.

The `dep:` prefix belongs here too: a bare optional dependency creates an
undeclared feature of the same name, and an undeclared feature is one nobody
reviewed.

## Direction

| Phase | Goal | Blocked by |
|---|---|---|
| **1** | Make the front door real: depend on and re-export `kernway-server` + `kernway-web`; complete the prelude | — |
| **2** | Convert every example to `kernway = { features = [...] }` | Phase 1 |
| **3** | Declare the feature table with `dep:`; add `doc_cfg` and `rustdoc-args` | Phase 1 |
| **4** | Add `kernway-static` and `kernway-htmx` to the baseline; add `security` and `kernleaf` as features | those crates |
| **5** | Compile-time measurement script + CI job; fill in the Speed table | Phase 3 |

**Phase 2 is the test.** If the examples cannot be expressed as one dependency
plus a feature list, then neither can a user's application, and the graph is
wrong. Converting them is how the front door stops being untested.

**Deliberately out of scope**: any behaviour. A bug fixed here is a bug fixed in
the wrong crate.

## Open questions

- **Does `web` include `kernleaf`?** It is Kernway's own engine, so bundling it
  is natural — but a user who wants Tera then compiles kernleaf for nothing.
  They can reach for `features = ["security"]` instead, which is why `security`
  is separately selectable. Leaning toward keeping kernleaf in `web`, since `web`
  exists for people who do not want to choose.
- **Is the baseline already too wide?** Static files and htmx are in it by the
  2026-07-23 decision, so a pure JSON API compiles both and uses neither. The
  cost is believed small and is **not yet measured** — if the Speed table shows
  otherwise, this is the first thing to revisit.
- **Should `kernway-http` be optional?** An application that only wants DI does
  not need an HTTP parser. But `kernway` without HTTP is just `di-core`, which
  users can depend on directly — probably not worth a feature.
- **Version alignment.** Every re-exported crate is `version.workspace`, so they
  release together. Does a user pinning `kernway = "0.6"` get a guarantee about
  `kernway-htmx`'s version if they also depend on it directly?

## Related KEPs

| KEP | Bearing on this module |
|---|---|
| [0002](../../kep/0000-principles.md#1-ours--write-it-do-not-import-it) | Why spec and implementation are separate crates, which is what makes the graph gateable |
| *(planned)* 0005 | Static-binary deployment — the compile-time extension model this graph implements |
