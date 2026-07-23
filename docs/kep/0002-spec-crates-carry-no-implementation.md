---
kep: 0002
title: Spec crates carry no implementation dependency
status: Accepted
created: 2026-07-23
decided: 2026-07-23
---

# KEP-0002: Spec crates carry no implementation dependency

> Backfilled. The decision predates this process; see the note in
> [README](README.md#a-note-on-the-first-four).

## Summary

Each subsystem is split in two: a `*-core` crate that defines traits and plain
data, and separate crates that implement them. A spec crate depends on
`thiserror` and, at most, one other minimal external crate. It never depends on
`serde`, a database driver, a TLS library, a runtime — or on another Kernway
crate, unless the dependency is genuinely load-bearing.

Four spec crates exist today: `kernway-core` (web), `kernway-orm-core`,
`kernway-cache-core`, and `di-core`.

## Motivation

Two problems, one cause.

**Forking to swap a piece.** A framework whose abstractions import their own
implementations cannot be extended from outside. If `kernway-core` imported
`kernleaf`, then supporting Tera means patching `kernway-core` — which means a
PR, a review, a release, and a maintainer's agreement. The framework becomes a
gatekeeper for work that has nothing to do with it.

**Compile time.** Everything depends on the spec crate, so the spec crate's
dependency tree is charged to every build in the workspace, and to every user's
build. A `serde` in `kernway-core` is a `serde` in every crate that touches a
`Request`.

Expected outcome: a spec crate compiles in under a second, and a third party can
add a template engine, a database backend, or a cache without touching this
repository.

## Guide-level explanation

Depend on the spec, receive an implementation:

```rust
#[component]
pub struct UserService {
    // Not SqliteRepository — the trait.
    #[inject] repo: Arc<dyn Repository<User>>,
}
```

Which implementation arrives is decided in `Cargo.toml` and at wiring time, not
in the service. Swapping SQLite for Postgres is a dependency change; this file
does not move.

Adding your own is symmetrical — implement the trait in your own crate:

```rust
pub struct TeraEngine { /* ... */ }

impl kernway_core::template::TemplateEngine for TeraEngine {
    fn render(&self, template: &str, ctx: &dyn TemplateContext)
        -> Result<String, TemplateError> { /* ... */ }
}
```

Nothing in Kernway needs to know your crate exists. There is no registry to be
added to and no PR to open.

## Reference-level explanation

**What a spec crate may contain:** trait definitions, plain data types the traits
pass around, error enums, and metadata types. No I/O, no runtime, no `serde`
derive, no driver.

**Current dependency budgets:**

| Crate | Role | External deps |
|---|---|---|
| `kernway-core` | Web/HTTP spec | `futures-core`, `thiserror` |
| `kernway-orm-core` | ORM spec | `thiserror` |
| `kernway-cache-core` | Cache spec | `thiserror` |
| `di-core` | DI container | `thiserror` |

**Two hard rules**, checked mechanically by `scripts/check-core.sh` rather than by
review, because a dependency edge is easy to add and very hard to remove:

1. A spec crate must not depend on another subsystem's spec crate.
   `kernway-orm-core` importing `kernway-core` would drag web types into data
   access, and the ORM would stop being usable on its own.
2. `kernway-core` must not gain an implementation dependency. The check greps for
   `serde`, `serde_json`, `rusqlite`, `mio`, and `libc` in its manifest.

The macro crates take this further. `kernway-orm-macro` emits
`::kernway_orm_core::` paths but does **not** depend on `kernway-orm-core` —
proc-macro output is text, so the token paths resolve in the user's crate. The
macro therefore adds no edge at all.

**Where edges are kept**, because they are real: `kernway-http` depends on
`kernway-core` (it parses bytes into a `Request`); `kernway-server` depends on
`di-core` and `rt-net` (it wires and runs). These are not violations — the rule
is "no dependency without need", not "no dependencies".

## Drawbacks

**Trait objects cost a virtual call.** `Arc<dyn Repository<T>>` dispatches
dynamically where a concrete type would inline. Small, but not zero, and it is
paid on every call rather than once.

**The trait is the ceiling.** A backend cannot expose anything the spec did not
anticipate without going outside the abstraction. `execute_raw` on the SQLite
repository is exactly this: an escape hatch, deliberately non-portable, admitting
that the trait does not cover everything.

**Indirection to read through.** "Where does `find_by_id` actually run?" is two
hops in this design and zero in a monolithic one. For a newcomer that is a real
cost, and no amount of documentation fully removes it.

**A spec change breaks every implementation at once.** The cost of the split is
that the trait becomes a public contract with third-party implementors, so
changing it is a breaking change for people who are not in this repository. That
is the intended trade — but it does mean spec crates must be designed slowly, and
it is the reason a KEP is required to touch one.

## Rationale and alternatives

**One crate, feature flags.** Keep everything together, gate implementations
behind `features = ["orm-sqlite", "template-kernleaf"]`. Simpler to navigate, and
common practice. Rejected on the extension point: a feature flag can only select
among implementations *that are in the crate*. An outsider still cannot add one
without a PR, which is the problem this KEP is about. It also does not fix
compile time as cleanly — feature unification across a workspace pulls in more
than expected.

**Traits in the same crate as the default implementation.** Halfway: define
`TemplateEngine` next to `kernleaf`. Rejected because the dependency direction
inverts — anyone implementing the trait now compiles `kernleaf` to get it.

**Do nothing.** Viable while Kernway is one team's project. Rejected because the
architecture the framework wants — community backends, à la carte subsystems — is
not reachable later if the edges are wrong now. Dependency edges are the hardest
thing in a workspace to remove after the fact, which is why they are checked by a
script rather than left to good intentions.

## Prior art

- **JPA / Hibernate.** The clearest precedent: JSR-338 is a specification, and
  Hibernate, EclipseLink, and OpenJPA implement it independently. `kernway-orm-core`
  is deliberately the same shape.
- **Spring's `ViewResolver`, `DataSource`, `CacheManager`.** Interfaces in the
  framework, implementations anywhere — which is why Spring has a large third-party
  ecosystem rather than a large core.
- **`log` / `tracing` in Rust.** A tiny facade crate everyone depends on, with
  implementations chosen at the binary. The clearest Rust-native evidence that the
  split works, and that a facade's value is proportional to how little it depends
  on.
- **Servlet API.** The cautionary version: a specification that became hard to
  evolve precisely because so many implementations depended on it. The cost named
  in Drawbacks is not hypothetical.

## Unresolved questions

- `kernway-core` still depends on `futures-core` for `BoxFuture`. Could a local
  type alias remove even that?
- How is a spec crate versioned once third parties implement it? Any trait change
  is a major bump for them. `docs/VERSIONING.md` covers the Rust toolchain, not
  this.
- Is there a mechanism for a backend to advertise capabilities beyond the trait,
  short of `OrmError::Unsupported` at runtime?

## Future possibilities

- Publish the spec crates on their own release cadence, slower than the framework.
- A conformance test suite a third-party backend can run to check it implements
  the spec correctly — the thing JPA's TCK does, and the natural next step once
  there is a backend Kernway did not write.
