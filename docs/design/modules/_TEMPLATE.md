# <crate-name> — <one line: what it is>

> **Module charter.** Every core crate has one. It records where the module is
> going and why, not only what it does today — so a decision made six months ago
> is still legible, and so the next person can see which way to push.
>
> Three properties are asserted in every charter, because they are the ones that
> silently rot: **speed**, **standards**, and **generic** (a third party can
> replace the piece without forking Kernway). Each has its own section, and each
> is expected to name evidence rather than intent.
>
> **This repository is public.** Nothing here is confidential — write for a
> contributor who has never seen the code, and assume a stranger will read it.
> That is a feature: honest design notes are worth more than a tidy README. The
> one thing it constrains is the Security table — see
> [KEP-0000 §3](../../kep/0000-principles.md#3-solid--correct-at-the-edges-or-not-correct):
> a ❌ there is fine while building and must never reach a release.
>
> Delete this block when you fill the template in.

## Purpose

One paragraph. What this module is for, and what would be missing without it.
Say what it is *not* — the boundary is usually the useful part.

## Status

Honest, and dated. A charter that claims more than the code does is worse than
no charter.

| Area | State | Notes |
|---|---|---|
| ... | ✅ done / 🚧 partial / ❌ not started / ⚠️ known broken | ... |

**Today**: what actually works, in one or two sentences.
**Not yet**: what a reader might reasonably assume exists and does not.

## Standards

Which specifications this module must comply with, and where compliance is
partial. An RFC number with no scope is not a claim anyone can check.

| Spec | Scope | Compliance |
|---|---|---|
| RFC xxxx §y | what it governs here | full / partial — say what is missing |

Rule: every relevant section of a spec that the module implements gets at least
one test named after it. A spec listed here with no test is a claim, not a fact.

## Architecture

The flow, as a diagram. Where a request/value enters, what transforms it, where
it leaves. Prefer one accurate diagram over three paragraphs.

```text
...
```

Then the parts, with the reasoning that shaped each — especially anywhere the
obvious design was rejected.

## Public surface

The API other modules and users depend on. Keep this short; the detail lives in
rustdoc. What belongs here is the *shape* and what is guaranteed about it.

```rust
...
```

**Stability**: what may still change, and what is now a contract.

## Integration

How this module composes, in both directions. This is the section that stops a
workspace from quietly turning into a ball of mud.

**Depends on** — and why each edge is genuinely needed:

| Module | Why |
|---|---|

**Depended on by** — who breaks if this changes:

| Module | What it uses |
|---|---|

**Must never depend on** — the edges that would be very hard to remove later,
and the reason. Add them to `scripts/check-core.sh` so review is not the only
thing standing in the way.

## Speed

The hot paths, named. For each: what it costs now, what the budget is, and where
the measurement lives.

| Path | Runs | Current | Budget | Bench |
|---|---|---|---|---|

Numbers come from `benches/`, not from intuition. An unmeasured claim is written
here as a hypothesis and labelled as one.

**Allocation policy on the hot path**: state it explicitly — how many
allocations a single pass is allowed, and which ones are deliberate.

## Generic — the extension points

What a third party can replace without forking, per
[KEP-0000 §1](../../kep/0000-principles.md#1-ours--write-it-do-not-import-it).

| Extension point | Trait | Default impl | Replaceable by |
|---|---|---|---|

If a behaviour has no extension point, say so and say whether that is deliberate.
"Hardcoded, and that is a bug" is a legitimate entry.

## Security

The threats specific to this module, and what answers each. Generic advice does
not belong here; the attack that applies to *this* code does.

| Threat | Mitigation | Tested |
|---|---|---|

## Direction

Where this goes next, in order, with the reason for the order.

| Phase | Goal | Blocked by |
|---|---|---|

**Deliberately out of scope**: what this module will not grow into, and which
module owns it instead.

## Open questions

Things genuinely undecided. Move each one out as it is settled — into a KEP if
the answer is expensive to reverse, into the text above if it is not.

## Related KEPs

| KEP | Bearing on this module |
|---|---|
