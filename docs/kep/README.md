# Kernway Enhancement Proposals (KEP)

A KEP records a decision that is hard to reverse, along with the reasoning that
produced it — so that a year from now the answer to "why is it like this?" is a
document rather than an archaeology session through git log.

Modelled on [Rust's RFC process][rust-rfcs], which is itself a descendant of
Python's [PEP][pep]. The two differ in emphasis, and Kernway follows Rust:
a KEP must argue against itself. **Drawbacks** and **Rationale and alternatives**
are required sections, not decoration. A proposal that cannot name what it costs
has not been thought through.

[rust-rfcs]: https://github.com/rust-lang/rfcs
[pep]: https://peps.python.org/

## When you need one

Write a KEP when the change is **expensive to undo**:

- a public API in a `*-core` spec crate — other people implement those traits
- a crate dependency edge, especially one that breaks module independence
- the behaviour of an annotation, or a new one
- an architectural commitment: the runtime model, the DI resolution rules, the
  error contract
- a deliberate departure from what Spring/JPA does, where a user will arrive with
  the wrong expectation

You do **not** need one for: bug fixes, performance work behind an unchanged API,
documentation, tests, new examples, or an implementation crate that adds no new
concept (a new ORM backend implementing existing traits is just work).

When unsure: if the change would need a migration note, it needs a KEP.

## Status

| Status | Meaning |
|---|---|
| `Draft` | Written, under discussion. Nothing is settled. |
| `Accepted` | The decision stands. Implementation may not exist yet. |
| `Rejected` | Considered and declined. Kept — a recorded "no" saves the next person from re-proposing it. |
| `Superseded` | Was right, then something replaced it. Points at what. |
| `Withdrawn` | The author pulled it before a decision. |

`Accepted` says the *decision* is made, not that the code exists. Track the
implementation in the issue tracker, not by editing the status.

## Process

1. Copy `TEMPLATE.md` to `NNNN-short-kebab-title.md`, taking the next free
   number. Fill it in — including the sections that argue against you.
2. Open a PR. The PR is the discussion; keep amending the document as the
   argument moves, so the file ends up reflecting the conclusion rather than the
   first draft.
3. When discussion goes quiet and nobody has an outstanding objection, a
   maintainer proposes acceptance and leaves a **final comment period** of one
   week. This exists so that "nobody replied" cannot be mistaken for "everybody
   agrees".
4. Merge with `status: Accepted` and `decided:` filled in.

A rejected KEP is merged too, as `Rejected`. It is the record that matters.

## Changing an accepted KEP

Small corrections — a typo, a clarification that changes no meaning — go in as a
normal PR.

A change of substance does not get edited in. Write a new KEP that supersedes the
old one, and set `superseded-by` on the original. The history of a decision is
part of the decision; overwriting it destroys exactly what this directory exists
to keep.

## Index

| # | Title | Status |
|---|---|---|
| [0000](0000-principles.md) | Founding principles — write it ourselves, fast, solid, stable | Accepted |
| [0001](0001-respect-rust.md) | Respect Rust — inspiration is not translation | Accepted |
| [0002](0002-response-body.md) | A response body that can be bytes, a file, or a stream | Accepted |
| [0003](0003-template-model.md) | A template model an engine can actually render | Accepted |
| [0004](0004-sessions.md) | Sessions — a signed token backed by a revocable registry | Accepted |
| [0005](0005-request-scoped-beans.md) | Request-scoped DI beans — a per-request scope over the app context | Accepted |
| [0006](0006-async-handlers.md) | Async handlers — a handler that can await | Accepted |

## The first two are different from the rest

KEP-0000 and KEP-0001 are not decisions about a feature. They are the standing
rules every later decision is checked against — what a new core is written to,
and how to behave when a Spring shape meets a Rust constraint. Read them before
writing a KEP; most proposals are resolved by one of them.

Numbering starts at 0000 for that reason: it precedes everything.

An earlier set of KEPs (0001–0004) recorded four architectural decisions
*retroactively* — thread-per-core, spec crates, compile-time bean override, no
lazy loading. They were removed. Written after the code shipped, they documented
conclusions rather than arguments, and several of their premises changed during
later design work. What survived is in KEP-0000 and KEP-0001 as principles, and
in the module charters under `../design/modules/` as decisions recorded where
they are acted on.

From KEP-0002 onward, a KEP is written **before** the code. That is the only way
the format is worth its cost — a document that records what you already did
cannot change what you do.
