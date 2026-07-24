# kernleaf — the template engine: the Thymeleaf Standard Dialect, in Rust

## Purpose

Render HTML from a [`Value`](../../kep/0003-template-model.md) model. Templates are
HTML with `th:*` attributes, so a `.html` file is valid on its own and opens in a
browser showing its placeholder content — the engine *overrides* that content at
render time. This is Thymeleaf's defining feature, **natural templates**: a
designer previews the page without running the server.

A template is parsed **once** into a cached DOM (`add`) and rendered by walking
that DOM — parsing never touches the request path. It is **HTML-safe by default**:
`th:text` escapes, and only the explicit `th:utext` emits raw HTML.

**Not** in scope (yet): parameterised fragments and the hot-reload loader (M5).
This charter covers slices A–G — the full Thymeleaf Standard Dialect core: the
attribute engine, the Standard Expression language, `@{}` URLs / `#{}` messages,
the `#`-utility objects, context-aware escaping, security (`th:authorize` +
auto-CSRF), and fragments (`th:fragment`/`th:insert`/`th:replace`). This completes
the kernleaf part of milestone **M4**.

## Status

As of 2026-07-24. M4 slice A.

| Area | State | Notes |
|---|---|---|
| Parse HTML + `th:*` → cached DOM (`add`) | ✅ | lenient parser: elements, void tags, attrs, comments, doctype |
| Natural templates (no `th:` → verbatim) | ✅ | placeholder content passes through unchanged |
| `th:text` (escaped) / `th:utext` (raw) | ✅ | escaping is the default; raw is the one explicit opt-out |
| `th:if` / `th:unless` | ✅ | truthiness from [`Value::is_truthy`] |
| `th:each="x : ${xs}"`, nested, correct precedence | ✅ | `th:each` wraps `th:if` per item, as Thymeleaf orders them |
| `th:<attr>` (`th:href`, `th:value`, `th:class`, …) | ✅ | sets that attribute from an expression, escaped |
| `xmlns:th` declaration stripped from output | ✅ | as Thymeleaf does |
| **Standard Expression** — vars, literals, arithmetic, comparison, boolean, ternary/elvis, `\|…\|` | ✅ B | full grammar in `expr.rs` |
| **`@{}` link URLs** — path vars, query params, URL-encoding | ✅ C | `@{/u/{id}(id=${x}, q=${y})}`; encoding is the URL escape context |
| **`#{}` i18n messages** — bundle lookup, `{0}` args | ✅ C | `.message(key, value)`; missing key → `??key??` |
| **Utility objects** — `#strings`/`#numbers`/`#lists`/`#bools` methods | ✅ D | separate `Util` branch — baseline `${var}` unaffected (measured) |
| **Context-aware escaping** — `th:inline` js/css + `[[…]]`/`[(…)]` | ✅ E | HTML default, JS/CSS per `th:inline`; inline scan at parse time |
| **`th:authorize`** — `hasRole`/`hasAnyRole`/`isAuthenticated`/`permitAll`/`denyAll` | ✅ F | consults `kernway-core`'s `Authorization`; fail-closed |
| **Auto-CSRF** — inject `_csrf` into state-changing forms | ✅ F | only for `method != get` when a token is supplied |
| **Fragments** — `th:fragment` + `th:insert`/`th:replace`, cross/same/whole-template | ✅ G | resolves against loaded templates; depth-capped against cycles |
| Benchmarked vs minijinja | ✅ | **~1.7× faster** on render (see Speed) |
| Parameterised fragments (`th:fragment="card(a,b)"`) | ❌ | later — needs a scope that binds computed args |
| Hot-reload loader | ❌ | M5 |
| Utility objects (`#strings`, `#numbers`, …) | ❌ | slice D — Thymeleaf's answer to "filters" |
| `th:inline` JS/CSS escape contexts | ❌ | slice E |
| `th:authorize` + auto-CSRF, fragments | ❌ | slice F, with `kernway-security` and [`kernway-htmx`](kernway-htmx.md) |
| Loader + hot reload | ❌ | M5 — `add` is what the watcher calls on change |

**Today**: `Kernleaf::new()`, `.add(name, source)?` to parse, then
`.render(name, &model)` (the [`TemplateEngine`] trait), or `.render_with(name,
&model, &RenderContext)` to supply `th:authorize` facts and a CSRF token. 69 unit
tests + a doctest.

## Standards / safety

| Concern | Rule | Tested |
|---|---|---|
| XSS via `th:text` | HTML-escaped by default | ✅ `th_text_escapes_html_the_xss_gate` |
| Raw HTML is explicit | only `th:utext` emits unescaped, and its name says so | ✅ `th_utext_is_raw…` |
| Attribute injection | `th:<attr>` values escaped (`"` `'` `<` `>` `&`) | ✅ `attribute_values_are_escaped` |
| Malformed template | reported at `add`, not silently at render | ✅ error paths in `add` |
| JS breakout in `<script>` | `th:inline="javascript"` `\uXXXX`-escapes `< > & /`, so a value cannot close the script or the string | ✅ `javascript_inline_escapes…` |
| CSS injection | `th:inline="css"` backslash-hex-escapes non-alphanumerics | ✅ `css_inline_escapes…` |
| Unauthorized content leak | `th:authorize` drops the element + subtree, fail-closed (no context → denied) | ✅ `th_authorize_is_fail_closed…` |
| CSRF (forgotten token) | a state-changing `<form>` auto-injects the `_csrf` field — the developer cannot forget it | ✅ `auto_csrf_injects…` |

Escaping-by-default is the load-bearing rule
([KEP-0000 §3](../../kep/0000-principles.md#3-solid--correct-at-the-edges-or-not-correct)):
the escaping is **context-aware** — HTML body/attribute, URL (`@{}`), JS and CSS
(`th:inline`) each get their own rule, and the rule is chosen at parse time so the
common HTML path stays a straight escape (never a per-character state machine).
`th:text`/`[[…]]` are the safe paths; `th:utext`/`[(…)]` are the explicit raw
exceptions, and their names say so.

## Architecture

```text
source HTML with th:* attributes
   │  Parser::parse_nodes()   → lenient HTML parse
   ▼
DOM: Vec<Dom>   Dom = Text | Comment | Declaration | Element{th_if, th_each, th_text, … , children}
   │  (th:* attributes pulled out of the raw attrs at parse time — this IS the IR)
   │  cached in Kernleaf.templates, keyed by name (`add`)
   │
   │  render(name, &model)                          ← the request path starts here
   ▼  render_nodes → render_element (th:each) → render_instance (th:if/unless) → open/attrs/body/close
   ▼
HTML string
```

Everything above `render` happens once at `add` (or on a hot-reload change);
everything at/below happens per request. Directive precedence follows Thymeleaf:
`th:each` (outer, repeats) wraps `th:if` (inner, filters each item). Path lookup
checks the loop scope first (innermost binding wins), then the model root.

## Public surface

```rust
pub struct Kernleaf { /* name → cached DOM */ }
impl Kernleaf {
    pub fn new() -> Self;
    pub fn add(&mut self, name: impl Into<String>, source: &str) -> Result<(), TemplateError>;
    pub fn message(&mut self, key: impl Into<String>, value: impl Into<String>); // #{key}
    pub fn is_compiled(&self, name: &str) -> bool;
    pub fn render_with(&self, name, model, ctx: &RenderContext) -> Result<String, _>; // th:authorize + CSRF
}
impl TemplateEngine for Kernleaf { /* render(&self, name, &Value) -> Result<String, _> */ }

// The security context for a render.
pub struct RenderContext<'a> { /* … */ }
impl<'a> RenderContext<'a> {
    pub fn new() -> Self;
    pub fn authorize(self, authz: &'a dyn Authorization) -> Self; // for th:authorize
    pub fn csrf(self, token: &'a str) -> Self;                    // injected into forms
}
```

**Stability**: the trait impl is the contract (`kernway-core`'s `TemplateEngine`).
The dialect is additive — new `th:*` attributes and expression forms extend it
without breaking existing templates.

## Integration

**Depends on**: `kernway-core` only — it renders the `Value` model and implements
`TemplateEngine`, both from [KEP-0003](../../kep/0003-template-model.md). The HTML
parser, expression evaluator, and escaping are hand-written, not a wrapped crate
([KEP-0000 §1](../../kep/0000-principles.md#1-ours--write-it-do-not-import-it)).

**Depended on by**: nothing yet. It will be an opt-in meta-crate feature
(`features = ["kernleaf"]`, the pattern `htmx` set), wired once the loader (M5)
and the `web` tier land. A handler returns `Html(engine.render(...)?)`.

**Must never depend on**: `kernway-server`, the filesystem, or an async runtime.
Parse and render are pure and synchronous; the loader that *reads* templates from
disk (M5) is separate, kept out so the engine stays unit-testable without I/O.

## Speed

`cargo bench -p kernleaf`, against **minijinja 2** (the dynamic-template incumbent
— the fair bar; Askama compiles into the binary and cannot hot-reload). Same
50-row output, escaped both sides, asserted equal before timing.

| Path | kernleaf | minijinja | kernleaf is |
|---|---|---|---|
| `render/user_list_50` (per request) | **3.01 µs** | 5.17 µs | **1.72× faster** |
| `parse/user_list` (once, off-path) | 0.80 µs | 1.02 µs | 1.28× faster |

Numbers from [BENCHMARKS.md](../BENCHMARKS.md). kernleaf renders faster by walking
a parsed DOM directly over a minimal `Value`, where minijinja runs a bytecode VM —
and it is faster *while* doing the whole Standard Dialect core — attribute
processing, a full expression evaluator, `@{}`/`#{}`, `#`-utility objects,
context-aware escaping, `th:authorize`, and fragments. The benchmark is rerun each
slice, not quoted as a permanent ratio.

The lead held across the build, and where it moved is recorded, not hidden:
- **Slices C, D, E, G cost nothing measurable** (render held ~3.0 µs). `@{}`/`#{}`/
  `#util` are separate branches; the `[[…]]` inline scan runs at parse time so
  plain text stays a fast `Text` node; fragment threading is two extra reference
  params. A template that does not use a feature does not pay for it — the
  pay-for-what-you-use discipline, verified. (Slice E was the second danger flagged
  earlier — an escaper taxing the HTML path — measured and avoided.)
- **Slice F is the one slice that added a small cost** — render 3.0 → ~3.1 µs
  (~2-3%). `th:authorize` is a *per-element* gate that no branching removes, though
  it is a cheap `Option` match, not a string compare or allocation. This is exactly
  the lead-shrinkage predicted when generality rises — the honest ~1.7× that
  remains, not a claim that every feature is free.

The request path itself is a DOM walk with no parsing, no disk, and allocation only
for the output `String` and the transient loop scope.

## Generic — the extension points

The model is generic already: any `Value` renders, any `ToValue` type becomes one.
Extensibility lands where Thymeleaf puts it — utility objects and expression
methods (slice D), not speculative engine knobs now.

## Direction

| Slice | Goal | Blocked by |
|---|---|---|
| A | attribute engine: `th:text/utext/if/unless/each/<attr>`, `${}`/literals, natural templates, escaping, cache | — (done) |
| B | Standard Expression: operators, comparison, boolean, ternary/elvis, literal substitution `\|…\|` | — (done) |
| C | `@{...}` link URLs (URL-encoding context), `#{...}` i18n messages | — (done) |
| D | utility objects (`#strings`, `#numbers`, `#lists`, `#bools`) — Thymeleaf's "filters" | — (done) |
| E | `th:inline` JS/CSS escape contexts, `[[…]]`/`[(…)]` inline expressions | — (done) |
| F | `th:authorize` (via `kernway-core`'s `Authorization`) + auto-CSRF | `kernway-security` — (done) |
| G | fragment addressing (`th:insert`/`th:replace`) for layouts + htmx | — (done) |
| later | parameterised fragments (`card(a,b)`) — a scope that binds computed args | a scope redesign |
| M5 | a loader + watcher that calls `add` on file change (<10 ms) | a file watcher |

## Open questions

- **Iteration status** (`th:each="x, stat : ${xs}"` → `stat.index/count/first/last`):
  slice A takes only the loop variable; the status object is wanted soon.
- **Expression edge**: is a bare (unquoted, non-`${}`) attribute value a string
  literal (current, lenient) or an error? Standard Thymeleaf leans on `${}`/`''`;
  strictness can come with slice B's real expression parser.
- **Fragment inclusion** (`th:insert`/`th:replace`, `~{}`): the layout story, and
  where fragment addressing for htmx (slice F) plugs in.
- **Parser strictness**: the HTML parse is deliberately lenient (mismatched close
  tags do not abort). Is a strict mode worth it for catching template typos?

## Related KEPs

| KEP | Bearing |
|---|---|
| [0003](../../kep/0003-template-model.md) | The `Value` model this engine renders, and why it is dynamic (hot reload) |
| [0000 §1](../../kep/0000-principles.md#1-ours--write-it-do-not-import-it) | Why the HTML parser and evaluator are ours, not a wrapped crate |
| [0000 §2](../../kep/0000-principles.md#2-fast--measured-or-it-is-not-a-claim) | Why the engine is benchmarked against minijinja every slice |
| [0000 §3](../../kep/0000-principles.md#3-solid--correct-at-the-edges-or-not-correct) | Why `th:text` escaping is the default and a test |
| [0000 §4](../../kep/0000-principles.md#4-stable--never-block-never-surprise) | Why parsing is off the request path, and raw output is explicit not a toggle |
