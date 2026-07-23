---
kep: 0004
title: The ORM has no lazy loading
status: Accepted
created: 2026-07-23
decided: 2026-07-23
---

# KEP-0004: The ORM has no lazy loading

> Backfilled. The decision predates this process; see the note in
> [README](README.md#a-note-on-the-first-four).

## Summary

`kernway-orm-core` has no lazy loading and no `EntityManager`. A relation is
loaded when you ask for it with `.with("relation")`, and not otherwise. There is
no proxy, no dirty tracking, no first-level cache, and no persistence context.

This is the largest deliberate departure from JPA, and a developer arriving from
Hibernate will expect otherwise.

## Motivation

**The mechanism is unavailable.** Hibernate returns a subclass generated at
runtime by cglib or Javassist; touching a field triggers an interceptor that runs
a query. Rust has no runtime bytecode generation, so there is nothing to
intercept a field access with. Field access is a memory read, and it will stay a
memory read.

**The mechanism is also the source of JPA's worst bug class.** Lazy loading is
where N+1 comes from: a loop over 100 users that reads `user.getPosts()` issues
101 queries, and the code that does it looks like ordinary field access. The
related failure is `LazyInitializationException` — the proxy outliving the
session it needed. Neither is a misuse; both are the feature working as designed.

So the constraint and the preference point the same way. The decision is to treat
the absence as the design rather than to simulate the feature.

Expected outcome: the number of queries a piece of code runs is visible in that
code.

## Guide-level explanation

**JPA:**

```java
@OneToMany(fetch = FetchType.LAZY)
private List<Post> posts;

user.getPosts();   // ← this line runs a query. Nothing about it says so.
```

**Kernway:**

```rust
// Ask for the relation, get one query with a JOIN:
let user = repo.query()
    .filter_eq("id", &id)
    .with("posts")
    .fetch_one()?;

// Don't ask, don't get. posts is empty; no query ran; no N+1 is possible.
let user = repo.find_by_id(&id)?;
```

The rule is: **queries happen at terminal calls**, and nowhere else.
`filter_eq`, `order_by_desc`, `with`, and `limit` build a description.
`fetch_all`, `fetch_one`, `fetch_count`, and `fetch_page` execute it. Reading a
field never does.

**No `EntityManager`.** Inject a `Repository<T>` instead. Instead of `persist`
plus flush-on-commit, call `save`. Nothing tracks your changes, so nothing
surprises you by writing them.

## Reference-level explanation

**`.with(relation)` is a JOIN, not a second query.** The backend renders one
statement per terminal call. Two `.with()` calls mean two joins in one statement,
not two round trips.

**The unloaded state is the empty value.** A `Vec` relation that was not requested
is empty; an `Option` one is `None`. There is deliberately no third state
distinguishing "not loaded" from "loaded and genuinely empty" — see Drawbacks.

**`Repository<T>` is stateless.** No identity map, so two `find_by_id` calls for
the same row return two independent values. `save` writes what you pass it.

**Everything is synchronous.** Under thread-per-core
([KEP-0001](0001-thread-per-core-runtime.md)) a blocking database call belongs on
a blocking pool, and the caller is the one that knows where that is. Keeping the
trait synchronous also keeps `kernway-orm-core` free of a runtime dependency,
which [KEP-0002](0002-spec-crates-carry-no-implementation.md) requires.

## Drawbacks

**More typing, and the compiler will not remind you.** Every relation you want is
a `.with()` you must write. Forget one and you get an empty `Vec` — not an error,
not a warning, just absence. The failure mode is silent, and it is the direct cost
of removing the silent failure mode on the other side.

**Empty and not-loaded are indistinguishable.** `user.posts.is_empty()` is true
both when the user has no posts and when you forgot to load them. Hibernate at
least throws `LazyInitializationException` in the second case. This is the
sharpest edge in the design, and a `Loaded<T>` wrapper would fix it at the cost of
a wrapper on every relation field.

**Over-fetching becomes the easy mistake.** Since asking is explicit and
forgetting is punished by silence, the natural habit is to `.with()` everything —
which loads data nobody uses. Lazy loading's actual benefit was not fetching what
you do not touch, and that benefit is genuinely lost.

**JPA knowledge partially misleads.** A developer who knows Hibernate has
intuitions here that are wrong rather than merely absent, which is worse. Much of
`kernway-orm-jpa-compat.md` exists because of this KEP.

## Rationale and alternatives

**Simulate proxies with `RefCell` + interior mutability.** A `Lazy<Vec<Post>>`
holding a connection handle, loading on first `deref`. Technically possible.
Rejected on three counts: it needs a live connection inside every entity, which
fights ownership and makes entities un-`Send`; `deref` cannot fail, so a query
error has nowhere to go but a panic; and it reintroduces N+1 precisely as JPA has
it. Trading Rust's clarity for JPA's worst bug is a bad exchange.

**A `Loaded<T>` / `NotLoaded` marker type.** Make the distinction explicit in the
type: `Loaded::Yes(vec)` versus `Loaded::No`. This directly fixes the sharpest
drawback above, and it is the strongest alternative in this KEP. Not adopted
because it puts a wrapper on every relation field and costs ergonomics on the
common path — but the trade is close enough that it belongs in Future
possibilities rather than being ruled out.

**A `DataLoader`-style batching layer.** Collect the relation accesses, issue one
batched query. Solves N+1 without proxies — but it needs a place to collect
accesses, which means a persistence context, which is the `EntityManager` this KEP
declines to have.

**Do nothing — no relation loading at all.** Make the user write joins by hand.
Rejected: `.with()` is a small, honest abstraction, and removing it would push
people to raw SQL for an ordinary case.

## Prior art

- **Hibernate / JPA** — the design being departed from, and the source of both the
  ergonomics being given up and the bugs being avoided. N+1 and
  `LazyInitializationException` are among the most-asked JPA questions anywhere.
- **Django ORM** — `select_related` / `prefetch_related` are opt-in eager loading
  and near-exact analogues of `.with()`, but Django *also* has lazy loading
  underneath, so forgetting them silently costs queries instead of silently
  returning nothing. Kernway removes the underneath.
- **Ent (Go), Diesel (Rust)** — both explicit-load-only, for the same reason: no
  runtime proxying available.
- **Rails** — `includes` plus a `Bullet` gem that detects N+1 at runtime. Evidence
  that even with lazy loading available, teams end up wanting the explicit version
  and tooling to enforce it.

## Unresolved questions

- Is `Loaded<T>` worth its ergonomic cost? The silent-empty case is the most
  likely source of real bugs in this design.
- Nested relations: does `.with("posts.comments")` work, and what SQL should it
  render?
- Should `#[entity]` be able to warn when a relation field is read on a value that
  was fetched without the corresponding `.with()`? The macro knows the field is a
  relation; it does not know the query.

## Future possibilities

- `Loaded<T>`, if the empty/not-loaded confusion shows up in practice.
- A debug-build counter that reports queries per request, making over-fetching
  visible — the `Bullet` idea, pointed the other way.
- Compile-time checked relation names, so `.with("psots")` fails to build rather
  than returning nothing at runtime.
