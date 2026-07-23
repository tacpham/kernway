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

1. Copy `0000-template.md` to `NNNN-short-kebab-title.md`, taking the next free
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
| [0001](0001-thread-per-core-runtime.md) | Thread-per-core runtime instead of work-stealing | Accepted |
| [0002](0002-spec-crates-carry-no-implementation.md) | Spec crates carry no implementation dependency | Accepted |
| [0003](0003-compile-time-bean-override.md) | Bean override resolved at compile time | Accepted |
| [0004](0004-no-lazy-loading.md) | The ORM has no lazy loading | Accepted |

## A note on the first four

KEP-0001 through 0004 are **backfilled**. The decisions were made and shipped
before this process existed; the documents were written afterwards from
`ARCHITECTURE.md`, `FULL_PLAN.md`, and the code.

They are therefore tidier than a real proposal would have been — nobody argued
against them in a PR thread, and the alternatives are reconstructed rather than
recorded live. Treat them as accurate about *what* was decided and honest, but
necessarily second-hand, about *why*. KEP-0005 onward should be written before
the code, not after.
