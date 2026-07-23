---
kep: 0001
title: Respect Rust — inspiration is not translation
status: Accepted
created: 2026-07-23
decided: 2026-07-23
---

# KEP-0001: Respect Rust

## Summary

Kernway is Spring-inspired. It is not Spring ported to Rust.

When a Spring or JPA mechanism depends on something the JVM provides and Rust
does not — a garbage collector, runtime reflection, bytecode generation, a stable
ABI — Kernway **does not simulate it**. It redesigns around what Rust actually
has, accepts that the resulting API is different, and documents why.

The corollary, and the reason this is KEP-0001 rather than a style note: **the
plan will keep changing.** Each time we meet a place where the Java shape does
not fit, the direction moves. That is the process working, not the process
failing.

## Motivation

The failure mode is specific and seductive. Take a Spring feature, want it in
Rust, and reach for `RefCell` plus `unsafe` plus a runtime registry to make the
same API appear. It compiles. It even demos. Then it leaks, or it panics under
concurrency, or it turns out to have re-imported the very bug the Java version is
famous for.

The clearest case is lazy loading. Hibernate returns a proxy subclass generated
at runtime; touching a field fires a query. Rust has no runtime bytecode
generation, so the honest options are: build something elaborate with interior
mutability that holds a live connection inside every entity, or **do not have the
feature**.

We do not have the feature — and the result is better, because lazy loading is
where N+1 comes from and where `LazyInitializationException` comes from. Removing
it removed both. That is what respecting the language looks like: the constraint
pointed at a design Java would have chosen too, if it could have.

Expected outcome: an API that reads as native Rust to a Rust developer, and reads
as *recognisable* to a Spring developer — without pretending to be the same
thing.

## Guide-level explanation

### What the JVM has that Rust does not

Each row is a Spring or JPA feature that rests on it.

| JVM capability | Spring/JPA relies on it for | Kernway instead |
|---|---|---|
| Garbage collection | Object graphs with cycles, shared mutable state everywhere, `EntityManager`'s identity map | Ownership. `Arc` where sharing is real, and cycles are a design smell rather than free |
| Runtime reflection | `@Component` scanning, `@Autowired` resolution at startup | Compile-time codegen — `#[derive(Component)]` emits the wiring; a missing bean is a build error |
| Bytecode generation | Lazy-loading proxies, AOP interception, `@Transactional` weaving | Explicit `.with()`; macros that rewrite at compile time |
| Stable ABI + classloaders | Hot-deploying a WAR into Tomcat | One static binary; extensions are crates resolved at link time |
| Exceptions with stack unwinding | `throws`, `@ControllerAdvice` | `Result` + `?`; a panic is a bug, not control flow |
| Class inheritance | `@Inheritance` entity hierarchies | Traits for behaviour, `enum` for closed variants |

### What Rust has that the JVM does not

The trade runs both ways, and this half is easy to forget while working around
the first half.

| Rust capability | What it buys | Spring's version |
|---|---|---|
| Deterministic destruction (`Drop`) | Cleanup happens exactly when the value dies | `@PreDestroy`, which the container has to be trusted to call |
| Ownership and borrow checking | Data races are compile errors | `synchronized`, and hoping |
| Compile-time codegen | Wiring errors surface at build, not at 3am startup | `NoSuchBeanDefinitionException` |
| `!Send` types are expressible | A task pinned to a core can use `Rc`/`RefCell` soundly | Everything is heap-shared and thread-safe by assumption |
| Sum types (`enum`) | Closed hierarchies with exhaustive matching | `@Inheritance` + discriminator columns |
| Zero-cost abstractions | A trait object costs a vtable call, not a proxy | Layers of indirection |

`!Send` is the sharpest one, and it is unavailable to Java entirely. Because a
Kernway task never migrates between cores, the future a handler returns need not
be `Send` — so request-scoped state can be an `Rc` where every other Rust web
framework forces an `Arc`. That is a capability, not a workaround.

### The rule

When a Spring shape meets a Rust constraint:

1. **Do not simulate.** If the mechanism needs a JVM capability, the mechanism is
   not available. Say so.
2. **Ask what the constraint is protecting.** Often the thing Rust will not let
   you build is the thing that was going wrong in Java — see lazy loading.
3. **Redesign around what Rust has**, and let the API differ.
4. **Document the difference where a Spring developer will hit it**, because they
   arrive with an intuition that is wrong rather than merely absent. Wrong
   intuitions are more expensive than missing ones.
5. **Keep the recognisable name** when the concept survives — `#[inject]`,
   `#[primary]`, `Repository<T>`. Familiar vocabulary for a different mechanism is
   fine. Familiar vocabulary for a *pretend* mechanism is not.

## Reference-level explanation

### Decisions this principle has already produced

Every one of these came from the same collision. They are listed together because
the pattern only becomes visible in aggregate.

| Decision | The JVM capability that is missing | What we do instead |
|---|---|---|
| No lazy loading in the ORM | Bytecode proxies | Explicit `.with("relation")` — one JOIN, no N+1 possible |
| No `EntityManager` | GC-backed identity map, dirty tracking | Stateless `Repository<T>`, explicit `save` |
| DI resolved at compile time | Runtime annotation scanning | `#[derive(Component)]` emits `Buildable`; missing dep = build error |
| No `.so` plugin host | Stable ABI, classloaders | Cargo features and crates, linked at build time |
| Own runtime, thread-per-core | — (this one is *using* a Rust strength) | `!Send` futures, `Rc` request state, no cross-core locks |
| `Drop` instead of `@PreDestroy` | — (Rust is simply better here) | Cleanup is deterministic and cannot be forgotten |

The last two rows matter as much as the first four. Respecting Rust is not only
declining what it cannot do; it is taking what it can.

### The direction changes, and that is expected

This is the part worth stating plainly, because it looks like instability from
outside.

A plan drawn from Spring's shape will be wrong in places that are only visible on
contact. Kernway has already turned twice on exactly this — the ORM lost lazy
loading and `EntityManager`; the deployment model lost `.so` hot-swapping. Both
reversals came from the same source: a design that assumed a JVM affordance, met
Rust, and had to move.

So:

- **A plan that changes on contact with the language is working.** The
  alternative is a plan defended past the point where it fits, which produces the
  `RefCell`-and-`unsafe` simulation this KEP exists to prevent.
- **Charters and KEPs are written to be revised.** A superseded decision is kept,
  not deleted, so the next person can see the collision that caused it.
- **Expect more turns.** Async handlers, template model representation, and the
  extension contract are all places where the Java shape has not been fully
  tested against Rust yet.

### The failure mode, concretely

What "not respecting Rust" looks like in a diff:

```rust
// Simulating Hibernate's lazy loading.
pub struct Lazy<T> {
    loaded: RefCell<Option<T>>,
    conn: Arc<Connection>,          // ← a live connection inside every entity
}

impl<T> Deref for Lazy<T> {
    fn deref(&self) -> &T {
        // Deref cannot fail, so a query error has nowhere to go but a panic.
        self.load_if_needed().unwrap()
    }
}
```

Three problems, and all three are consequences of the same refusal: the entity is
no longer `Send`, a database error becomes a panic because `Deref` has no error
channel, and N+1 is back exactly as JPA has it. The API looks like Hibernate. The
behaviour is worse than either language alone.

## Drawbacks

**A Spring developer's knowledge partially misleads.** Absent knowledge is
cheaper than wrong knowledge, and this principle guarantees the wrong kind: they
know `@OneToMany(fetch = LAZY)` and will assume it exists. Much of
`kernway-orm-jpa-compat.md` exists to pay this cost, and documentation only ever
pays part of it.

**"Respect the language" is not a decision procedure.** It gives no answer for a
case where Rust *can* express the Java shape awkwardly. Reasonable people will
disagree about where awkward becomes simulation, and this KEP does not settle
that — it only insists the question be asked.

**Rejecting a feature is easy to overuse.** "Rust cannot do that" is available as
an excuse for work nobody wants to do. The check is §2 of the rule: name the
missing *capability*, specifically. If you cannot, the constraint is effort, not
the language.

**It costs marketing.** "Spring for Rust" is a clearer pitch than "Spring-shaped
until Rust says otherwise." The second one is true.

## Rationale and alternatives

**Port Spring faithfully.** Maximum familiarity; a Spring developer would be
productive immediately. Rejected: several central mechanisms are unimplementable
without proxies or reflection, so the port would be a facade over `unsafe`, and
the failure modes would be new and worse than either framework's.

**Ignore Spring entirely, design from Rust first.** Cleanest result, and it is
roughly what axum and actix did. Rejected because Kernway's reason to exist is
the Spring developer arriving at Rust. Discarding the vocabulary discards the
audience.

**Case by case, no stated principle.** What happens by default. Rejected because
the pressure is always local and always toward simulation — "just this once,
behind a `RefCell`" is a persuasive argument in a single PR and a bad one across
fifty.

## Prior art

- **Kotlin's Java interop** — deliberately not "Java with better syntax": null
  safety, no checked exceptions, coroutines instead of threads. Familiar
  vocabulary, different mechanisms.
- **Diesel and Ent (Go)** — both explicit-load-only, both because runtime proxying
  is unavailable. Independent arrivals at the same conclusion.
- **ASP.NET Core after Web Forms** — abandoning the stateful abstraction rather
  than porting it, and getting a better framework out of it.
- **Django's ORM** — the counter-example. It kept lazy loading, added
  `select_related` as an opt-in, and inherited N+1 anyway. A reminder that the
  Java shape has costs even in a language that *can* express it.

## Unresolved questions

- Where exactly is the line between "Rust cannot express this" and "expressing it
  is unpleasant"? `#[transactional]` is the next test: a macro can rewrite a
  function body, so AOP is possible — the question is whether it stays legible.
- Do we accept a *worse* API to keep a Spring name? Present answer: keep the name
  when the concept survives, change it when only the spelling would.
- How much of this belongs in user-facing documentation rather than here? A
  Spring developer needs the *consequences*; they do not need the philosophy.

## Future possibilities

- A "coming from Spring" guide organised by *broken expectation* rather than by
  feature — the mapping table shows what exists, not what will surprise you.
- Compile-time detection of the anti-patterns above: a lint for a database handle
  held inside an entity, or for `unwrap` on a request path.
