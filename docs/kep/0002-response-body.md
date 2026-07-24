---
kep: 0002
title: A response body that can be bytes, a file, or a stream
status: Accepted
created: 2026-07-24
decided: 2026-07-24
---

# KEP-0002: Response body — bytes, file, or stream

## Summary

`Response.body` changes from `Vec<u8>` to a `Body` enum:

```rust
pub enum Body {
    Empty,
    Bytes(Vec<u8>),
    File { path: PathBuf, len: u64, range: Option<(u64, u64)> },
}
```

A handler still hands back bytes in the common case (`Body::Bytes`, and the
existing `IntoResponse` impls keep working). But a response can now *name* a
file instead of carrying its contents, so the connection task — which is async
— reads it, in bounded chunks, off the blocking pool. That is what makes it
possible to serve a large file without loading it into memory, to answer `HEAD`
with a `Content-Length` and no body, and to answer a byte `Range`.

This KEP is about the body only. The async *handler* — a handler that can
`await` — is a separate, larger decision and is explicitly **not** part of it.

## Motivation

Three things are impossible today, all for the same reason: the body is a
`Vec<u8>`, and the encoder derives `Content-Length` from `body.len()` and always
appends the body.

**Serving a large file loads it entirely into memory.** `kernway-static::load_static`
does `std::fs::read` — a 200 MB download is 200 MB of resident memory, per
concurrent request. Ten of them is 2 GB. This is the single reason the static
server cannot be pointed at anything but small assets, and it is the M2b blocker.

**`HEAD` cannot be answered correctly.** `HEAD` must return the exact headers a
`GET` would, including `Content-Length`, with no body. The encoder computes
`Content-Length` from `body.len()`, so an empty body yields `content-length: 0`,
which is wrong. Today the static layer simply declines `HEAD` and it 404s.

**`Range` cannot be answered.** A video seek, a download resume, a PDF viewer
fetching one page — all send `Range: bytes=...` and expect `206 Partial Content`
with `Content-Range`. There is no way to say "the body is bytes 1000–1999 of a
2 MB file, and `Content-Length` is 1000".

Expected outcome, measurable: serving an N-byte file uses O(chunk) memory, not
O(N); `HEAD /file` returns the file's length with an empty body; `Range` returns
`206` with the right slice. The existing 41 ns static-route path and every
current handler are unaffected.

## Guide-level explanation

Most handlers do not change at all. `Response::new(OK).body(b"hi".to_vec())`
still works, `Json(x)`, `Html(x)`, `&str`, `String` all still work — they
produce `Body::Bytes` under the hood, and the `.body()` builder still takes
`impl Into<Vec<u8>>`.

What is new is that a response can name a file rather than carry it:

```rust
Response::new(StatusCode::OK).file("public/video.mp4")   // the server reads it
```

The handler does no I/O. It hands back a `Body::File` describing *what* to send;
the connection task, which is already async, opens and streams it on the
blocking pool so no core is stalled ([KEP-0000 §4]). This is the same principle
the M1 static read already follows, made a first-class response shape.

For the static layer specifically, three things start working:

- **Large files** stream in chunks instead of being read whole.
- **`HEAD`** returns the headers and length with no body.
- **`Range: bytes=0-99`** returns `206` and those hundred bytes.

None of this is a new API a user calls for static files — the server does it.
The user-visible surface is one builder method, `.file(path)`, for a handler
that wants to send a file it names.

[KEP-0000 §4]: 0000-principles.md#4-stable--never-block-never-surprise

## Reference-level explanation

### The type

```rust
pub enum Body {
    /// No body. HEAD responses, 204, 304.
    Empty,
    /// Bytes already in memory — the common case, what IntoResponse produces.
    Bytes(Vec<u8>),
    /// A file the connection task reads. `len` is the full file size (for
    /// Content-Length and Range math); `range`, when set, is the half-open byte
    /// interval to send for a 206.
    File { path: PathBuf, len: u64, range: Option<(u64, u64)> },
}
```

`Body` lives in `kernway-core`. It is deliberately small and closed — a `Stream`
variant for generated (non-file) bodies is a later addition, noted under Future
possibilities, and left out now so this KEP stays about files.

### Response

`Response.body: Vec<u8>` becomes `Response.body: Body`. `Response::new` sets
`Body::Empty`. The `.body(impl Into<Vec<u8>>)` builder wraps in `Body::Bytes`,
so every current call site is source-compatible. A new `.file(path, len)` (or a
higher-level `.file(path)` that stats first) sets `Body::File`.

### The encoder splits head from body

`encode_response_with` today returns one `Vec<u8>` of head-plus-body. It splits:

- `encode_head(response, connection, content_length) -> Vec<u8>` — the status
  line and headers, with `Content-Length` passed *in* rather than read from the
  body. This is the change that makes `HEAD` and `File` expressible.
- The body is written separately by the connection task:
  - `Body::Empty` → nothing.
  - `Body::Bytes(b)` → `content_length = b.len()`, write `b` (unchanged behaviour;
    still coalesced into one buffer with the head for a small response, so the
    single-`write` property the current encoder documents is preserved).
  - `Body::File { len, range, .. }` → `content_length` is the range length or
    `len`; the task streams the file in bounded chunks from the blocking pool.

### The connection task

`serve_connection` gains the body-writing branch. For a file it opens once, seeks
to the range start if any, and loops `spawn_blocking(read chunk) → write chunk`
until done. Chunk size is bounded (say 64 KiB) so memory is O(chunk) regardless
of file size. A read error mid-stream closes the connection — the head, with its
`Content-Length`, is already committed, so there is no way to signal the error in
band; closing is the honest option and matches what every server does here.

### HEAD

The static layer handles `HEAD` like `GET` up to the point of the body: it
resolves, checks, and stats the file (it already stats for the ETag), producing a
response whose body is `Body::Empty` but whose `Content-Length` is the file
length. The `Content-Length` therefore has to travel independently of the body —
which is exactly what passing it into `encode_head` provides. No file contents
are read for a `HEAD`.

### Range

`Range: bytes=start-end` on a `GET`:

- Parse a **single** range (multi-range `multipart/byteranges` is out of scope —
  see Unresolved). A syntactically bad range → ignore it, serve `200` full. An
  unsatisfiable range (start ≥ len) → `416 Range Not Satisfiable` with
  `Content-Range: bytes */len`.
- A satisfiable range → `206`, `Content-Range: bytes start-end/len`,
  `Body::File { range: Some((start, end+1)), .. }`, `Content-Length = end-start+1`.
- **Cap the ranges per request** at one here (the multi-range cap is moot until
  multi-range exists), and reject overlapping/pathological sets when it does —
  range handling is a known DoS amplifier, so the limit is part of the spec, not
  a later hardening pass ([KEP-0000 §3]).

[KEP-0000 §3]: 0000-principles.md#3-solid--correct-at-the-edges-or-not-correct

### What this KEP does not specify

- **The async handler.** Handlers stay `Fn(&Request, &AppContext) -> Response`.
  A handler that returns `Body::File` still does no I/O, so nothing here needs
  the handler to `await`. Making handlers async is a separate KEP with a much
  larger blast radius (every handler, middleware, extractor, and test), and
  bundling it here would hold hostage a change the static server needs now.
- **`sendfile`/zero-copy.** Chunks pass through userspace for now. A
  `sendfile`/`splice` fast path is a later optimisation behind the same `Body::File`
  surface — the enum is what makes it possible, not this implementation of it.
- **A `Stream` variant** for generated bodies (SSE, chunked proxying). Noted for
  later; not needed for files.

## Drawbacks

**It is a breaking change to a `kernway-core` type.** `Response.body` is public,
and its type changes. Every construction and every read of `.body` is touched.
The mitigation — `.body()` still takes bytes, `IntoResponse` still produces
`Body::Bytes` — keeps *handler* code compiling, but code that pattern-matches or
indexes `response.body` as a `Vec<u8>` (several tests do) breaks and must be
updated. That is real churn, and it is the strongest argument for doing it once,
now, rather than twice.

**The encoder is no longer a single call returning a single buffer.** Splitting
head from body costs some of the tidiness the current one-buffer design has, and
the "one `write` for a small response" property now has to be preserved
deliberately (coalesce head+bytes) rather than falling out for free. Get that
wrong and small responses regress on latency via Nagle — so the property needs a
test, not a comment.

**A file streamed in chunks through userspace is slower than `sendfile`.** This
KEP does not deliver zero-copy, so for large-file throughput Kernway will, for
now, be behind an nginx that `sendfile`s. The enum leaves room to fix it, but the
first implementation is honestly a chunked copy, and the charter should not claim
otherwise.

**Mid-stream errors cannot be reported.** Once the head with its `Content-Length`
is sent, a read failure has nowhere to go but a dropped connection. That is
inherent to `Content-Length` framing and not specific to this design — but it is
a real failure mode the streaming path introduces that the read-it-all path did
not have (there, a read error happened *before* any head was sent, so it could
still 404).

## Rationale and alternatives

**Keep `Vec<u8>`, add a separate `StreamResponse` type.** Leave `Response`
alone; handlers that stream return a different type entirely. Rejected: it
splits the response model in two, so middleware, `IntoResponse`, and the encoder
all have to handle both, and "is this response streamable" leaks into every layer.
One `Body` enum keeps a single response type end to end.

**A trait object body: `Box<dyn Read>` or a `Stream`.** Maximally general — any
source, not just files. Rejected for now on two counts: it erases the file case,
which is the one that wants `sendfile` later (you cannot `sendfile` a
`dyn Read`), and it forces an allocation and dynamic dispatch on every response
including the tiny ones. A closed enum keeps `Bytes` a plain vector and keeps
`File` concrete enough to optimise. The trait-object escape hatch can be added as
a `Stream` variant if a real need appears.

**Bundle the async handler in.** The original roadmap note (the old "KEP-0005")
lumped async handlers, `Body`, hot reload, and link-time extensions together.
Rejected as one KEP: they have wildly different blast radii and only `Body` is on
the critical path for M2b. Splitting them means the static server gets what it
needs without waiting on a framework-wide handler-signature change.

**Do nothing.** The static server stays limited to small files, `HEAD` 404s, and
`Range` is unsupported. Acceptable only if Kernway never serves a download, a
video, or a large asset — which contradicts "drop files in a folder and deploy".

## Prior art

- **axum / hyper `Body`.** hyper's body is a `Stream` of frames; axum wraps it.
  Maximally general and the right choice for a library that does not know its
  workloads. Kernway knows one workload it cares about — files — and picks a
  closed enum so it can special-case them, which hyper's trait-object body cannot.
- **Go `http.ServeContent` / `io.Reader` + `http.ServeFile`.** Go passes an
  `io.ReadSeeker` and the stdlib handles `Range`, `Content-Length`, and
  conditional requests from it. `Body::File` is the same idea with the seeker
  implied by the path, and the `Range`/conditional logic likewise centralised in
  the server rather than each handler.
- **nginx `sendfile`.** The bar for large-file serving, and the thing
  `Body::File` is shaped to allow later. nginx does zero-copy kernel-to-socket;
  this KEP's first cut does not, and says so.
- **Rails `send_file` / Rack `Rack::Files`.** `send_file` names a path and the
  framework streams it with `Range` support — a direct analogue of `.file(path)`,
  and evidence that "handler names a file, framework serves it" is a well-worn,
  ergonomic shape.

## Unresolved questions

- **Multi-range requests** (`Range: bytes=0-49,100-149`) need a
  `multipart/byteranges` body. Worth it, or is single-range enough? Deferred; the
  spec permits a server to answer a multi-range request with a single `200`.
- **Where does `HEAD` routing live?** For static it is natural. For a dynamic
  route, does the framework synthesise `HEAD` from the `GET` handler (run it,
  drop the body) or require an explicit one? Leaning toward auto-synthesis, but
  it interacts with side-effecting GETs (which are already a mistake).
- **Chunk size.** 64 KiB is a guess. It should be measured — the right value
  trades syscall count against memory, and belongs in `BENCHMARKS.md` once there
  is a large-file bench.
- **Conditional + Range interaction** (`If-Range`): serve the range only if the
  validator still matches, else the full file. Not in the first cut.

## Future possibilities

- **`sendfile`/`splice` zero-copy** behind `Body::File`, per platform — the
  payoff the enum exists to enable, and where thread-per-core helps (the fd and
  the socket are on the same core, no cross-core coordination).
- **A `Stream` variant** for generated bodies: SSE (`kernway-sse` today buffers),
  chunked proxying, server-push.
- **`If-Range`** and multi-range, once single-range and conditional GET have
  settled.
- **Precompressed variants** (`.br`/`.gz` chosen by `Accept-Encoding`) compose
  naturally: pick the file, then `Body::File` it.
