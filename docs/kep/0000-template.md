---
kep: 0000
title: The one-line name of the thing
status: Draft            # Draft | Accepted | Rejected | Superseded | Withdrawn
created: YYYY-MM-DD
decided:                 # date the status left Draft
supersedes:              # KEP number(s), if any
superseded-by:           # KEP number, once this stops being current
---

# KEP-0000: Title

## Summary

One paragraph. Somebody who reads only this should be able to say what changes
and why. No motivation here, no alternatives — just the shape of the thing.

## Motivation

What problem exists today? Who hits it, and how often? Show the pain concretely
— a snippet that is awkward to write, a benchmark that disappoints, a class of
bug that keeps recurring.

State the expected outcome. If this ships and works, what is measurably
different?

## Guide-level explanation

Explain it as you would in the documentation, to somebody who has never seen the
implementation. Use examples. Introduce whatever names the feature adds, and say
how an existing user's mental model shifts.

If the change is user-visible, this section is the draft of the docs page.

## Reference-level explanation

The details an implementer needs. Data structures, the algorithm, the edge cases,
how it interacts with what already exists. Precise enough that somebody else
could build it and get the same thing.

Say what you are *not* specifying, too — deliberately open questions belong here,
not hidden.

## Drawbacks

Why might this be the wrong idea? Every real decision costs something. A KEP with
an empty Drawbacks section has not been thought about hard enough — write down
what you are giving up, including the cases where a reader would reasonably
prefer the alternative.

## Rationale and alternatives

- Why this design over the others?
- What else was considered, and what specifically ruled it out?
- What happens if we do nothing?

Be fair to the alternatives. The point is a record a future reader can argue
with, not a case for the conclusion.

## Prior art

How do Spring, Hibernate, tokio, actix, axum, Django, Rails, or anyone else solve
this? What did they learn — including their mistakes, and the papers or issues
where they said so.

Where Kernway deliberately departs from a well-known approach, say why. "Rust
cannot do what Java does here" is a valid reason; it is also one worth spelling
out, since a reader arriving from Spring will assume the Java behaviour.

## Unresolved questions

- What is still open, to be settled before this is Accepted?
- What is out of scope, to be settled in a later KEP?

## Future possibilities

What this makes possible later, whether or not anyone builds it. Useful for
showing that the design does not paint us into a corner.
