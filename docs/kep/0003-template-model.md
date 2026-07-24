---
kep: 0003
title: A template model an engine can actually render
status: Accepted
created: 2026-07-24
decided: 2026-07-24
---

# KEP-0003: Template model — a borrowed `Value` tree

## Summary

`TemplateContext` is replaced. Today it is:

```rust
pub trait TemplateContext {
    fn get(&self, key: &str) -> Option<&dyn std::any::Any>;
}
```

which hands a template engine a `&dyn Any` it can do nothing with — it cannot
downcast a type it does not know, cannot iterate a list, cannot ask "is this
truthy". It is unimplementable, and it is the first thing M4 must fix.

In its place, a concrete dynamic model:

```rust
pub enum Value<'a> {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(Cow<'a, str>),
    Seq(Vec<Value<'a>>),
    Map(Vec<(Cow<'a, str>, Value<'a>)>),   // insertion-ordered
}

pub trait ToValue {
    fn to_value(&self) -> Value<'_>;
}
```

`TemplateEngine::render` takes `&Value` (a `Map` at the root). A handler builds
the model from its data; the engine walks it. Values **borrow** where they can
(`Cow`), so building a model does not clone every string.

## Motivation

The model is the decision everything else in M4 rests on, and the current one is
a dead end. An engine receiving `Option<&dyn Any>` has three things it needs and
cannot get:

- **Read a value it did not define.** `Any::downcast_ref::<T>()` requires naming
  `T`. The engine is generic over user types by definition, so it never can.
- **Iterate.** `{% for u in users %}` needs the engine to see a sequence and walk
  it. `&dyn Any` is opaque — there is no "is this a list, and what is in it".
- **Decide truthiness / render a scalar.** `{% if user %}` and `{{ user.name }}`
  need "is this empty/false" and "give me the text". `Any` answers neither.

So no engine can be written against this trait — `kernleaf` cannot start until it
is replaced. Expected outcome, and the gate for this KEP: a **reference engine in
the tests** that interpolates `{{ key }}` and iterates a `Seq`, implemented
purely against `Value`, with no `downcast` anywhere. If that compiles and passes,
the model is implementable; that is the whole claim.

## The forcing constraint: hot reload

The shape of the model is not a free choice. M5 requires editing a `.kwl`
template and seeing it **without a rebuild** — a runtime watcher recompiles the
template's IR in <10 ms. A template read and compiled *at runtime* cannot be
type-checked against a Rust struct at compile time, which is exactly what the
zero-cost, compile-time approach (Askama, `sailfish`) does. So compile-time
typing is off the table for the templating story Kernway has committed to.

That leaves a **dynamic** model — the engine discovers the shape of the data at
render time. This KEP is about making that dynamic model the disciplined,
Rust-respecting kind rather than a bag of `Box<dyn Any>`.

## Guide-level explanation

A handler builds a model and renders:

```rust
let model = Value::map([
    ("title", "Our users".to_value()),
    ("users", users.to_value()),          // Vec<User>: User derives the model later
]);
let html = engine.render("users.kwl", &model)?;
Html(html)
```

Scalars convert with `.to_value()` or `Value::from`; `Value::map` and
`Value::seq` build the containers. A `User` struct becomes a `Value::Map` — by
hand today, by a derive macro later (Future possibilities). Nothing in the model
is `Box<dyn Any>`; every value is one of seven concrete shapes the engine knows
how to render.

The engine side, in full, is walkable:

```rust
match value {
    Value::Str(s)  => escape_into(out, s),
    Value::Seq(xs) => for x in xs { /* {% for %} */ },
    Value::Map(kv) => /* field lookup for {{ a.b }} */,
    Value::Bool(b) => /* {% if %} */,
    // …
}
```

## Reference-level explanation

### The type

`Value<'a>` lives in `kernway-core::template`. Design points, each with a reason:

- **Borrowed (`Cow<'a, str>`), not owned.** A model is built from data the
  handler already holds — a `&User`, a `&str` field — and rendered synchronously
  in the same call, so the borrow is always valid. `Cow` lets a field pass by
  reference (`Cow::Borrowed`) while still allowing a computed string
  (`Cow::Owned`). This is the KEP-0001 move: don't clone what you can borrow.
- **`Map` is a `Vec<(Cow, Value)>`, not a `HashMap`.** A template touches a
  handful of fields; an insertion-ordered vector of pairs is smaller, keeps
  render output stable, and is faster to build and to scan at these counts — the
  same finding as `Headers`/`Fields` (see BENCHMARKS.md). Lookup is a linear
  scan, which at a struct's field count beats hashing.
- **Closed, seven variants.** `Null`/`Bool`/`Int`/`Float`/`Str` are the scalars a
  template renders or tests; `Seq`/`Map` are the two containers it walks. No
  `bytes`, no `datetime` — those render *through* `Str` (formatted by the handler
  or a filter), keeping the core model minimal. More variants are additive.
- **`Int(i64)` + `Float(f64)`, not one number type.** Templates do render integers
  and floats differently (`1` vs `1.0`), and keeping them apart avoids a lossy
  round-trip. `u64`/`i128` that do not fit convert through `Str`; documented.

### The trait

```rust
pub trait ToValue {
    fn to_value(&self) -> Value<'_>;
}
```

Implemented in `kernway-core` for `bool`, the integer types (via `Int`), `f32`/
`f64`, `&str`/`String`/`Cow<str>`, `Option<T: ToValue>` (`None` → `Null`), and
`[T]`/`Vec<T>` (→ `Seq`). A `#[derive(Model)]` for structs is a later, separate
piece (it belongs with the macro crate, not the core), and is not required to
unblock the engine — a struct can build its `Value::Map` by hand until then.

**Not `serde::Serialize`.** `kernway-core` carries no serde and is not going to
([KEP-0000 §1](0000-principles.md#1-ours--write-it-do-not-import-it)); the model
is a handful of variants we own, not a re-export of someone's data-model crate.
An app that already has `Serialize` types can write a five-line adapter, or use
the derive when it lands — but the *core* stays serde-free, so it keeps compiling
in under a second.

### The engine trait

```rust
pub trait TemplateEngine: Send + Sync {
    fn render(&self, template: &str, model: &Value<'_>) -> Result<String, TemplateError>;
}
```

`TemplateContext` is deleted. The root model is a `Value` (a `Map` in practice,
though the type does not force it — an engine can decide a non-map root is an
error). `render` stays synchronous and returns a `String`: IR compilation and
caching happen inside the engine at load/watch time, never on the request path
([KEP-0000 §4](0000-principles.md#4-stable--never-block-never-surprise)).

Escaping is still the engine's contract (default HTML-escape, context-aware per
M4), and is deliberately **not** in the model — a `Value::Str` is plain text, and
what escaping it needs depends on where the template puts it (body vs attribute
vs URL vs JS), which only the engine knows.

### Blast radius

Small, and that is the point of doing it now. Nothing implements `TemplateEngine`
yet (`kernleaf` does not exist), and only the `kernway-core` doc table and the
prelude reference these names. Removing `TemplateContext` and changing the
`render` signature touches the trait definition, the module doc, and the KEP
index — not a single handler. Locking the model in *after* an engine exists would
be the expensive version.

## Drawbacks

**Building the model allocates.** A `Value::Map` is a `Vec`, a `Seq` is a `Vec`;
rendering a list of N rows builds N `Value::Map`s. The borrowing keeps strings
out of it, but the container vectors are real allocations the compile-time
approach does not pay. Mitigation is a later `Object`/lazy trait (Future
possibilities) for the hot case; the eager model is the correct, simple first
cut, and the allocations are measured, not waved away — a `model/build` bench
lands with `kernleaf`.

**Dynamic dispatch on shape at render time.** The engine `match`es on `Value`
every interpolation, where Askama resolves the field at compile time. This is the
inherent cost of runtime templates, and it is the cost hot reload is worth paying
— stated plainly rather than hidden. The bar (see Prior art) is minijinja/Tera,
which pay the same and are fast enough; the benchmark target is them, not Askama.

**A lossy edge for exotic scalars.** `u64` > `i64::MAX`, `i128`, `NaN`/`Inf`
floats have no dedicated variant and go through `Str`. Rare in a view model, but
real; the alternative (a bignum/decimal variant) is complexity the 99% case does
not want.

## Rationale and alternatives

**Compile-time typed templates (Askama-style).** Fastest — the template is Rust,
the model is your struct, zero dynamic dispatch. Rejected because it is
**incompatible with hot reload**: a template compiled into the binary cannot be
edited without a rebuild, and M5's <10 ms template reload is a committed feature.
Choosing Askama's model would silently delete a milestone.

**Keep `&dyn Any`, add downcast helpers.** Give the engine `as_str()`,
`as_seq()`, etc., that try a fixed set of downcasts. Rejected: the set of types
is open (any user struct), so the helpers can only ever cover the built-ins —
which is exactly a `Value` enum, but discovered by failed downcasts instead of a
`match`. The enum is the same idea, made honest and exhaustive.

**`serde_json::Value` as the model.** It is the dynamic model Tera uses, and it
would work. Rejected on the KEP-0000 §1 line: it pulls `serde`/`serde_json` into
`kernway-core`, the one crate whose whole value is compiling in a second with no
data-model dependency. Our `Value` is seven variants we own; the cost of writing
it is less than the cost of the dependency on the core.

**A lazy `Object` trait instead of an eager tree** (minijinja's `Object`).
Values are computed on demand as the template asks for them, so a model with a
field the template never uses costs nothing. Better for the hot path — and it is
the planned *next* step, not the first. The eager `Value` is simpler to build an
engine against and to test, and the two coexist (an `Object` can yield `Value`s).
Starting eager keeps this KEP about the shape, not about laziness.

**Do nothing.** M4 does not start. The template trait stays a documented lie —
present in the API, impossible to implement. Not an option.

## Prior art

- **minijinja `Value` + `Object`.** The closest analogue and the model to learn
  from: an enum of concrete kinds plus a trait for lazy objects. This KEP takes
  the enum now and notes the object trait as the next step — the same two-layer
  design, staged.
- **Tera / `serde_json::Value`.** Proves a dynamic-tree model renders real apps
  at acceptable speed; also the cautionary tale for the serde dependency this KEP
  avoids in the core.
- **Askama / `sailfish`.** The compile-time bar for raw speed, and the reason the
  trade-off is explicit: they are faster and cannot hot-reload. Kernway picks the
  other side of that trade on purpose.
- **Thymeleaf (`IContext`) / Spring `Model`.** A named-attribute map handed to a
  view — `Value::Map` at the root is the same shape, with the values typed rather
  than `Object`.

## Unresolved questions

- **`#[derive(Model)]`.** Where does it live (a new `kernway-template-macro`, or
  `di-macro`), and does it borrow every field (`to_value(&self) -> Value<'_>`) or
  offer an owned form for values that outlive the source? Leaning borrowed, with
  the derive skipping fields it cannot borrow into a `Cow`.
- **The lazy `Object` trait.** Exact signature, and how it interleaves with the
  eager `Value` (does `Object::get` return `Value` or another `Object`?). Deferred
  to when a benchmark shows the eager build cost mattering.
- **Filters/functions.** `{{ name | upper }}` — do filters operate on `Value`
  (uniform, boxes the result) or specialise? Out of scope here; it is an engine
  concern once the model exists.
- **A non-map root.** Is `render("t", &Value::Str("x"))` an error, or does the
  engine expose the scalar as `.`? Left to the engine; the type permits both.

## Future possibilities

- **`#[derive(Model)]`** so a struct becomes a `Value::Map` with no boilerplate —
  the ergonomic payoff, once the shape is settled here.
- **A lazy `Object` trait** for the hot path: a model that computes fields on
  demand, so an unused field costs nothing. The staged second layer.
- **Borrowed `Seq`/`Map` slices** (`Cow<'a, [Value<'a>]>`) if the build
  allocations show up in a render bench — pass an existing slice through instead
  of collecting a `Vec`.
- **A serde adapter crate** (outside the core) so `Serialize` types drop in for
  apps that want it, without the core taking the dependency.
