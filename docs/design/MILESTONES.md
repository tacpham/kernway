# Kernway — goal-driven milestones

## How this differs from ROADMAP.md

`ROADMAP.md` is organised by **version**: what ships in v0.3, v0.4, v0.5. It
answers "when does feature X arrive?"

This document is organised by **one goal**, sliced. It answers "what is the
smallest thing we can run end to end, and what does running it teach us?"

The two are complementary and must not contradict each other. Where they do,
this file is the one being executed and `ROADMAP.md` needs correcting.

## The goal — fixed, does not move

```
A Kernway web application deploys as a Docker image and runs.
  · dev:        edit and see it immediately
  · production: fast and stable
```

Concretely, the end state a user experiences:

```bash
kernway new my-site && cd my-site
kernway dev                          # edit a template → visible instantly
kernway build                        # one static binary, assets embedded
docker build -t my-site . && docker run -p 8080:8080 my-site
```

Everything below exists to reach that line, and nothing is built that does not
serve it.

## Method — walking skeleton

Build the thinnest possible end-to-end slice **first**, even when it does almost
nothing. Then thicken it. Never build a layer in isolation and hope it fits.

The reason is not tidiness. It is that a running slice **tells you what is
missing**, and it tells you in the right order — whereas a plan written up front
tells you what you already believed. The first slice below is deliberately
trivial, and it will still surface more gaps than any amount of design.

Each milestone therefore has a **gate**: an observable, runnable check. Not "the
code is written" — something that either happens or does not.

| Rule | Why |
|---|---|
| Every milestone ends in something runnable | A milestone that cannot be demonstrated cannot be verified |
| The gate is observable, not a checklist | "Done" is an opinion; `curl` returning 304 is not |
| Gaps discovered get their own charter, then get built | See `modules/_TEMPLATE.md` |
| The goal never moves | Scope changes are milestones added, not the target redefined |

---

## M1 — Walking skeleton: it runs in Docker ✅ (2026-07-24)

**Goal**: one example application, one Docker image, and it answers a request and
shuts down cleanly.

Done, and it pulled in a little more than the original plan: static file serving
was small enough to include, so `examples/web-docker` serves a real `index.html`
from `public/` rather than a hardcoded string. That is M2 work brought forward
because the slice was cheap — the walking skeleton finding its own next step.

**What it forced us to build** — and did:

- `crates/kernway-static` — path resolution + MIME, zero dependencies, 21 tests
- `.static_files(root)` on the builder; the read runs on the blocking pool via
  `spawn_blocking`, so it never stalls a shard
- `examples/web-docker/` — static site + JSON route + health checks
- A multi-stage `Dockerfile` on a distroless runtime
- `PORT` from the environment; bind `0.0.0.0`
- `/health` (liveness) and `/ready` (readiness) as distinct endpoints

**Gate — passed, from a clean build:**

```
docker build -f examples/web-docker/Dockerfile -t kernway-web-docker .
docker run -d -p 8080:8080 kernway-web-docker

GET /                      200  text/html   1222 B   (public/index.html)
GET /style.css             200  text/css             (+ x-content-type-options: nosniff)
GET /api/ping              200  {"message":"pong"}   (router wins over static)
GET /health                200
GET /ready                 200
GET /../../etc/passwd      404  (raw, curl --path-as-is — server rejects, not curl)
GET /.env                  404
docker stop                exited in 0.18s
image size                 34.9 MB
```

**What it revealed** — the open questions, answered:

- **Does graceful shutdown work with a real Docker `SIGTERM`?** Yes — 0.18s to
  drain and exit, not the 10s-then-`SIGKILL` of an unhandled signal. This moved
  the charter's "graceful shutdown" row from 🚧 to ✅.
- **Image weight?** 34.9 MB on distroless/cc — measured, no longer a guess.
- **Is the meta-crate usable as one dependency?** *Still not tested* — the
  example depends on the crates by path, because `kernway` does not re-export
  `KernwayApp` yet. That is the honest finding: the front door is still ajar, and
  making it a single `kernway = { features = [...] }` line is the next task, now
  scoped as [M1a](#m1a--close-the-front-door).

**Deferred out of M1**, now explicit: cold-start time and idle RSS were not
measured (they belong to M6's real load test), and HEAD returns 404 for static
paths (GET-only slice; HEAD/Range are M2).

---

## M1a — Close the front door

**Goal**: `examples/web-docker` depends on `kernway` alone, not on six crates by
path. The meta-crate re-exports `KernwayApp`, `Response`, and the prelude; the
example proves it.

This is the meta-crate charter's Phase 1, promoted to a milestone because M1
showed the single-dependency story is still untested — and an untested front door
fails for the first user, not for us.

**Gate**: `web-docker/Cargo.toml` lists `kernway` and `serde_json` only, the
image still passes the M1 gate, and `grep -c 'path =' examples/web-docker/Cargo.toml`
is 0.

---

## M2 — Static files

**Goal**: drop `index.html`, CSS, and JS into a folder; the image serves them.

**Forces us to build**:

- KEP-0005 decisions landed: async handlers, `Body` enum
- `Body::File` — the handler names a file, the connection task reads it, so
  nothing blocks a core
- Router mount class (prefix match) — `{param}` matches one segment and cannot
  express `/assets/**`
- `kernway-static`: MIME by extension, ETag, `If-None-Match` → 304,
  `Last-Modified`, path traversal defence, dotfile denial, no directory listing

**Gate**:

```bash
curl -I localhost:8080/                      # 200, text/html
curl -I localhost:8080/assets/app.css        # 200, text/css, ETag present
curl -I -H 'If-None-Match: "<etag>"' ...     # 304, empty body
curl -i 'localhost:8080/../../etc/passwd'    # 404 — and the encoded variants too
curl -i localhost:8080/.env                  # 404
```

The traversal cases are part of the gate, not a later hardening pass. A static
server that ships without them ships a vulnerability.

---

## M3 — htmx, in the baseline

**Goal**: static HTML with `hx-get` calls an endpoint; the endpoint returns a
fragment; it swaps.

**Forces us to build**:

- `kernway-htmx` — its own crate, non-optional dependency
- `Htmx` extractor: `is_request()`, `target()`, `trigger()`, `prompt()`, …
- `HtmxResponse` builder: `trigger()`, `push_url()`, `retarget()`,
  `reswap(Swap::…)` — enums, so a typo is a compile error
- **Automatic `Vary: HX-Request`** whenever the response depends on it

**Gate**:

```bash
curl -i -H 'HX-Request: true' localhost:8080/users
#   → fragment, and `vary: hx-request` present
curl -i localhost:8080/users
#   → full page, same URL
```

`Vary` is in the gate because without it a cache serves a fragment to a browser
expecting a page — the classic htmx bug, and invisible until it happens to a
user.

---

## M4 — Templates and security (`features = ["web"]`)

**Goal**: render a page from data; accept a form back safely.

**Forces us to build**:

- Model representation — the current `TemplateContext` returns `&dyn Any` and
  **cannot be implemented against**; this must be decided first
- `kernleaf`: parse → IR → render, IR compiled at startup and cached, never read
  from disk on the request path
- Context-aware escaping: HTML body, attribute, URL, and JS need different rules
- `kernway-security`: CSRF token issue and verify, security headers
- Fragment addressing, so htmx gets a fragment from the same template

**Gate**:

```bash
# XSS attempt is escaped, not executed
curl 'localhost:8080/search?q=<script>alert(1)</script>'   # &lt;script&gt;
# javascript: URL in an attribute is neutralised
# POST without a CSRF token is rejected
curl -X POST localhost:8080/users -d 'name=x'              # 403
```

Security cases are the gate. A template engine that renders correctly but escapes
incorrectly has not passed.

---

## M5 — Developer experience: hot reload

**Goal**: edit and see it, without thinking about the server.

**Tiered**, because most edits need no rebuild at all:

| Edit | Mechanism | Latency | Restart |
|---|---|---|---|
| Template `.kwl` | watcher → recompile IR | < 10ms | no |
| Static asset | watcher → invalidate cache + ETag | < 10ms | no |
| Config | watcher → reload the reloadable parts | ms | no |
| Rust code | rebuild + socket handover | 1–3s | yes, zero-downtime |

The last row replaces the `.so` plugin idea. A supervisor holds the listening
socket (or both processes bind with `SO_REUSEPORT`), the new child starts
accepting, the old one drains and exits. No `dlopen`, no ABI risk, and the hard
part — graceful drain — already exists.

**Gate**: edit a `.kwl`, refresh the browser, see the change, with no restart in
the log. Edit a `.rs`, and no request is dropped across the handover.

---

## M6 — Production build

**Goal**: `kernway build` produces the artifact the goal statement promises.

**Forces us to build**:

- Asset embedding for release (`include_dir!`) — dev reads from disk, release
  compiles them in, so deployment is genuinely one file
- Release profile: LTO, `codegen-units`, `strip`
- The allocator decision, **measured, not assumed**: musl's malloc is slow under
  multi-threaded allocation, which is the worst case for thread-per-core. If
  `FROM scratch` is wanted, mimalloc or jemalloc probably has to come with it.
- Compile-time measurement for each feature configuration

**Gate**:

```bash
kernway build
ls -la target/release/my-site       # one binary
docker build . && docker images     # size recorded
# cold start and idle RSS measured; benchmark against the same app on Axum
```

Every number in `README.md` and `ARCHITECTURE.md` comparing Kernway to Spring or
tokio becomes checkable here. Several of them are currently inherited from the
literature rather than measured — see
[KEP-0000 §2](../kep/0000-principles.md#2-fast--measured-or-it-is-not-a-claim).

---

## Cross-cutting: correct the public claims

Not a milestone — a debt to clear, and the earlier the better.

`README.md` currently states:

```
- **v1.0** ✅ Stable API, full feature set, flagship example
```

The workspace version is `0.1.0`. `ROADMAP.md` places kernleaf at v0.6. Static
files, htmx, templates, and async handlers do not exist. The most public document
in the repository claims a stable v1.0 for a project at 0.1.

This is worth fixing before anyone reads it, independently of everything above.
The Quick Start has the same problem in a smaller way: it implies a single
`kernway` dependency, which does not yet work.

## Order, and what blocks what

```
M1  skeleton in Docker ──┬── M2  static files ──┬── M3  htmx
                         │                      │
                         │                      └── M4  templates + security
                         │                                    │
                         └────────────────────────────────────┴── M5  hot reload
                                                                      │
                                                                      └── M6  prod build
```

M1 blocks everything, and it is the smallest. That is the point of a walking
skeleton: the cheapest slice is the one that removes the most uncertainty.

M3 and M4 are independent of each other — htmx needs no template engine, and a
template engine needs no htmx. They meet only at fragment rendering.

## Per-milestone checklist

Before a milestone is called done:

- [ ] The gate passes, demonstrably, from a clean checkout
- [ ] Each new crate has a charter (`modules/_TEMPLATE.md`)
- [ ] Decisions that are expensive to reverse have a KEP
- [ ] Public documentation matches what exists — no aspirational claims
- [ ] Security cases in the gate are tests, not manual `curl` runs
- [ ] Numbers stated anywhere are measured; unmeasured ones are labelled
      hypotheses
