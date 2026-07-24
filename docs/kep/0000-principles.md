---
kep: 0000
title: Founding principles — write it ourselves, fast, solid, stable
status: Accepted
created: 2026-07-23
decided: 2026-07-23
---

# KEP-0000: Founding principles

The rules a new Kernway core is written to. Not aspirations — checks. Every one
below can be failed, and failing one is a reason to stop and reconsider rather
than to ship and note it.

Four words, in priority order when they conflict: **ours · fast · solid ·
stable**.

They rarely conflict. Where they do, the later word wins: a fast core that
panics is worthless, and a solid core that blocks a shard is not stable.

---

## 1. Ours — write it, do not import it

**Everything above the operating system is written here.**

The reason is not pride. It is that a framework built on someone else's
abstractions inherits their performance ceiling, their bugs, their release
cadence, and their idea of what the problem is. When a hot path is slow and it
belongs to a dependency, there is nothing to do but wait or fork.

Writing it ourselves means every line is one we can measure, change, and delete.

### The rule

A dependency is acceptable only if **all four** hold:

1. It wraps something the OS owns and we cannot portably reimplement
2. Reimplementing it would be reckless, not merely tedious
3. It is small, stable, and widely audited
4. It sits at the edge, never on the hot path we control

### What that admits today, and why

| Crate | Where | Test 4 (edge / hot path) | Justification |
|---|---|---|---|
| `mio` | `rt-core` | ⚠️ **fails** — `Poll::poll` is the reactor loop | epoll/kqueue/IOCP behind one API. Admitted anyway, on the strength of the other three tests and the Windows case (see below). This is the one dependency on a hot path we control, and the honest exception. |
| `libc` | `rt-core/sys` | ✅ edge | CPU affinity syscalls, called at shard startup. |
| `thiserror` | spec crates | ✅ compile-time | Derives `Display`/`Error`. No runtime footprint. |
| `syn`/`quote` | macro crates | ✅ compile-time | Rust parsing. Reimplementing this would be a project, not a crate. |
| `serde` | edge crates only | ✅ edge | The de facto serialization contract. **Never** in a `*-core` spec crate. |

### The mio exception, stated plainly

`mio` does not pass the rule cleanly, and a principles document that pretends
otherwise is not worth having. `Poll::poll` runs once per reactor iteration —
that is the hot path, and test 4 says a dependency must stay off it.

It is admitted because the alternative is unbalanced by platform:

- **Linux `epoll` and macOS `kqueue`** are ~250 lines each over `libc` — writable,
  and test 2 ("reimplementing would be reckless") does *not* hold for them. On
  these two platforms mio is convenience, not necessity.
- **Windows IOCP is completion-based, not readiness-based.** mio emulates
  readiness on top of it using the undocumented `\Device\Afd` interface. That is
  a genuine project to reproduce, and test 2 holds firmly.

So the honest position is: mio earns its place on Windows and is tolerated on
Unix. The direction, recorded in the `rt-core` charter, is to own the Unix
`sys/` poller and keep mio behind `#[cfg(windows)]` — but only when there is a
reason (an `io_uring` backend, whose completion model fits mio poorly; or a
measurement showing mio on the request path costs something). Rewriting a working
poller for purity alone would fail test 2 in the other direction.

### What it excludes, and this is the point

**No tokio.** `rt-core` and `rt-net` are ours: reactor, executor, waker, task,
timers, shards. See §4 for why the runtime model is not negotiable.

**No hyper, no axum, no tower.** `kernway-http` parses and encodes HTTP/1.1 from
`kernway-core` and `thiserror` and nothing else.

**No template engine.** `kernleaf` is written here, which is what lets templates
compile to an IR at build time and lets fragment names be checked by the
compiler. An imported engine could do neither.

**No ORM, no cache client, no router crate.**

### The honest part

"Write everything" is a direction, not a fact. Five dependencies are listed
above, and each is a small admission that the line has to sit somewhere. What
the principle forbids is drifting: a dependency added because it was convenient,
on a path we care about, doing work we could have done.

**Adding a dependency to a core crate needs a KEP.** Removing one never does.

---

## 2. Fast — measured, or it is not a claim

Speed is not a quality anyone can assert. It is a number, taken from a benchmark,
compared against a baseline.

### The rules

**Every hot path has a benchmark before it has an optimisation.** `benches/`
exists so that a comment saying "this is ~2× faster" can be checked. One already
was, and the comment was wrong: `TypeIdHasher` turned out to be ~6× on lookups,
not the ~2× the code claimed. Both numbers were guesses until someone measured.

**An unmeasured claim is written as a hypothesis and labelled.** The p999 figures
in `ARCHITECTURE.md` are inherited from the thread-per-core literature, not from
a Kernway benchmark. Until `examples/echo-server` runs against tokio, that table
is a belief. Documents say so rather than implying otherwise.

**Allocation policy is explicit per hot path.** How many allocations one pass is
allowed, and which are deliberate. The router is at zero for a matched static
route and one map for a route that actually has parameters — that line is held by
knowing it exists.

**The optimisation that matters is the one that removes work, not the one that
does work faster.** A static route became a hash lookup instead of a linear scan;
that is worth more than any amount of tuning the scan.

**A problem that recurs on the hot path earns a structure built for it.** When
the same shape of work runs on every request, the question is not "is this
general-purpose type good?" — `std`'s are excellent — but "is it the *right
shape* for this specific, repeated problem?" Often it is not, and the win is a
purpose-built structure, not a faster general one:

- Request headers and query params are short-keyed entries, parsed in one pass
  then read by name. `Headers`/`QueryParams` keep everything in one buffer, so
  the parser allocates once per request instead of once per pair — the reason
  they are not `HashMap`s.
- The DI container keys on `TypeId`, already a well-distributed value; a
  pass-through hasher beats SipHash **6×**.
- The router keys on short path segments with no adversarial input; FNV beats
  SipHash **2×**.

**But measure it in context, not in isolation — this rule has a trap, and we
fell in it.** A micro-benchmark said the one-buffer `Headers` built and iterated
5 entries 1.35× faster than a `HashMap`, so `Response.headers` was migrated from
`HashMap` to `Headers`. In the *actual encode path* it was **slower** — 75 → 103 ns
per response — because the encoder iterates the headers twice (size estimate,
then write) and real responses set fewer headers than the micro-benchmark used.
The isolated number pointed one way; the number that ships pointed the other. The
migration was reverted. The same structure that wins on the request side (parse
once, look up) lost on the response side (set a few, iterate to encode) — a
structure is right *for a use case*, not in the abstract.

So the discipline is: name the problem precisely (few entries? short keys?
build-then-iterate? how many? read back or not?), pick or write the structure
that answers *that*, and **benchmark it where it runs** — in the pipeline, not in
a micro-benchmark that flatters it. This is also **not** licence to reimplement
`std`: rewriting `HashMap` to match `hashbrown` is the reckless-not-tedious line
in §1.

**Fast enough for itself is not the bar. The bar is the incumbent.** A number in
isolation only says the code is not embarrassing — it does not say it is good. We
chose to write our own router, parser, and runtime instead of using `matchit`,
`httparse`, and `tokio`; that choice is only justified if what we wrote is at
least as fast as what we declined. So a hot path is benchmarked **against the
crate a mainstream framework uses for the same job**, on the same machine, same
input, in the same process — and the target is to match it or beat it. If we are
slower, either we optimise until we are not, or we write down why the gap is an
acceptable trade (and it rarely is, for a thing we chose to own).

### The per-core loop

Every core follows the same four steps, in order, and the last one repeats:

1. **Write it.**
2. **Test the core alone** — `cargo test -p <crate>`, including the edges (§3).
3. **Run it for real** — over a socket, from disk, in a container: the thing a
   deployment does, not just a function call. A unit test proves the algorithm;
   a real run proves the wire.
4. **Benchmark, compare to the incumbent, optimise — and loop.** Measure the hot
   path, put the number beside the crate a mainstream framework uses, and if it
   is behind, optimise and measure again. Stop when it matches or beats the
   incumbent, not when it "seems fine".

Step 4 is a loop, not a checkbox. A first cut that is 3× slower than `matchit`
is a starting point, not a failure — the loop is what closes the gap, and a
recorded "we are at 1.2× and here is why" is a legitimate place to stop.

### Compile time counts as speed

A framework that takes four minutes to rebuild is slow, whatever it does at
runtime. Spec crates compile in under a second and stay that way. Feature flags
exist so nobody compiles what they do not use.

---

## 3. Solid — correct at the edges, or not correct

Anyone can be correct in the middle. A core earns the word at its edges: bad
input, hostile input, resource exhaustion, and the paths nobody runs by hand.

### The rules

**A spec section implemented has a test named after it.** RFC 9112 §9.3 is
implemented, and `keep_alive_tests` proves it. A charter listing an RFC with no
test is claiming, not stating.

**Malformed input gets an answer, never a crash and never a hang.** A parse
failure is a 400 and a closed connection. Every buffer is bounded — an unbounded
request head is a memory-exhaustion vector, not an edge case.

**Errors are typed. `unwrap` on the request path is a bug.** A panicking handler
is caught and becomes a 500 because on a shared shard the alternative kills every
other connection on that core — but the catch is a safety net, not a design.

**Security cases are tests in the same commit as the feature.** Path traversal
belongs to whoever writes the static file server, not to a hardening pass later.
Escaping belongs to whoever writes the engine.

**A ❌ in a charter's Security table may never reach a release.** Charters list
threats before they are mitigated, which is useful while building and dangerous
once shipped — this repository is public, so an unmitigated row is a published
description of how to attack anyone running that version. Before a release, every
Security row for a shipping feature is ✅, or the feature does not go in the
release. There is no third option, and "we will harden it next version" is not
one.

**`unsafe` is confined and justified.** Only `rt-core` and `rt-net` may use it,
every block carries a `SAFETY:` note, and miri runs over it. The waker vtable is
where undefined behaviour would live if it lived anywhere — and one design that
read fine on paper was in fact unsound, which is exactly why this rule exists.

---

## 4. Stable — never block, never surprise

Stable means behaviour that does not change under load, and a process that
degrades rather than falls over.

### Never block — the one that is easiest to get wrong

Kernway is **thread-per-core**: each thread owns a reactor, an executor, and a
task queue. A task is created on a core and finishes on that core. No shared
queue, no work stealing, no lock on the request path.

The consequence is not a performance note. **A blocking call stops every
connection on that core**, because there is no other worker to take up the slack.
A work-stealing runtime tolerates a blocking call; this one does not.

So blocking is a correctness failure, and it fails in the worst way available:
invisibly, under load, one core at a time.

| Never on the request path | Instead |
|---|---|
| `std::fs::*` | async I/O, or name the file and let the connection task read it |
| blocking network calls | async transport |
| `std::sync::Mutex` across an await | `RefCell` — the task never migrates, so this is sound |
| `std::thread::sleep` | `rt_core::sleep` |
| a synchronous driver | `rt_core::spawn_blocking` |
| a slow pure-CPU pass | `spawn_blocking` — a core cannot tell computing from waiting |

The last row is the one people forget. `spawn_blocking` is not only for I/O.

Blocking **is** allowed at startup and shutdown: reading config, compiling
templates, opening pools, binding listeners. Nothing is being served yet.

A payoff worth naming: because a task never migrates, the future a handler
returns does **not** need to be `Send`. Request-scoped state can be an `Rc` where
every other Rust framework forces an `Arc`.

### Stable also means

**One static binary.** `cargo build --release` produces a file that runs with no
runtime, no VM, no loader. That is Rust's largest deployment advantage and
nothing may trade it away — which is why extensions are Cargo features and
crates, resolved at link time, and not `.so` modules loaded at runtime.

**Graceful degradation.** `SIGTERM` drains in-flight requests and then exits.
A crashed core is restarted by a supervisor. A cache being unreachable means
slower, not broken.

**Defaults are safe, and safety is never a feature flag.** Cargo unifies features
across the whole dependency graph, so a feature that *disabled* something — say
auto-escaping — could be switched on by one transitive dependency and silently
weaken the entire application. Features may only add. Opting out of a safety
default is a type or a call at the site that takes the risk, where it is visible.

**Every default is overridable.** A framework decision the user cannot change is
a fork waiting to happen.

---

## Checklist for a new core

Before a new `kernway-*` crate is considered started:

- [ ] Every dependency passes the §1 test, and each is justified in the charter
- [ ] Charter written from `docs/design/modules/_TEMPLATE.md`
- [ ] Hot paths named, with a benchmark, before any optimisation
- [ ] Allocation policy stated for each hot path
- [ ] Relevant specs listed, with a test per implemented section
- [ ] Nothing on the request path can block — checked, not assumed
- [ ] Malformed and hostile input covered by tests, not by intent
- [ ] Public API documented; `cargo doc` clean; doctests run
- [ ] Extension points identified — what a third party can replace
- [ ] Every claimed number measured, or labelled a hypothesis

## Process

This is KEP-0000 because it precedes every other decision. KEP-0001 onward record
decisions that are expensive to reverse, using
[`TEMPLATE.md`](TEMPLATE.md) — see [README](README.md).

Earlier KEPs 0001–0004 recorded four architectural decisions retroactively. They
were removed: written after the code shipped, they documented conclusions rather
than arguments, and several of their premises changed during the design work that
produced this document. The decisions themselves live where they are acted on —
in the module charters under `docs/design/modules/`. Future KEPs are written
**before** the code, which is the only way the format earns its cost.
