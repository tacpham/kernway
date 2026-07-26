---
kep: 0008
title: A request body that can be buffered, spooled to disk, or multipart
status: Draft
created: 2026-07-27
decided:
---

# KEP-0008: Request body — buffer, spool, or multipart

## Summary

The inbound mirror of [KEP-0002]. Where KEP-0002 let a *response* body name a
file the connection task streams out, this lets a *request* body that is too
large for memory stream **in** to a temporary file, and lets a
`multipart/form-data` body be parsed into its parts — small fields kept in
memory, file parts spooled to disk — without any part being held whole in RAM.

Concretely, three things:

- A size boundary on `Request.body`. Bodies up to `max_inmemory_body` are read
  into memory as today; larger ones stream to a temp file
  (`Request.body_spool`), and a body over the hard `max_upload_size` ceiling is
  refused with `413` before a byte touches disk.
- `UploadFile` — an extractor for a single streamed body (a `PUT /files/x` with
  the bytes as the whole body). It hands the handler a path, not the bytes, and
  a `persist(dest)` that moves the temp file into place off the request path.
- `Multipart` — an extractor for `multipart/form-data` (RFC 7578): iterate the
  parts, each field as a value or each file as an `UploadFile`, parsed as a
  stream so a 2 GB upload with three files stays O(chunk) in memory.

Handlers do not change signature and do no I/O on the request path beyond what
they already await. This KEP is about the request body only; it does not touch
the response body, the router, or the handler signature (settled in [KEP-0006]).

[KEP-0002]: 0002-response-body.md
[KEP-0006]: 0006-async-handlers.md

## Motivation

Today every request body is a `Vec<u8>`. The connection task reads
`content-length` bytes off the socket into `Request.body`, and only then runs
the handler. Three things follow, all from that one fact:

**An upload is resident in memory, whole, per concurrent request.** A 500 MB
file `PUT` is 500 MB of `Request.body`; ten of them is 5 GB. There is no size at
which the server declines — it will try to buffer whatever `content-length`
claims until the allocator or the OOM killer intervenes. This is the exact
inbound twin of the problem KEP-0002 fixed outbound, and it is why "drop files
in a folder and deploy" cannot yet include *receiving* files.

**There is no upload ceiling.** A hostile client sends `content-length:
9999999999` and the server begins buffering. The only backstop is the machine
falling over. A size limit is not hardening to add later — for a body the server
reads on someone else's say-so, the limit is part of being correct at the edge
([KEP-0000 §3]).

**A browser file-upload form does not work at all.** An HTML
`<form enctype="multipart/form-data">` — the way every browser uploads a file —
sends a `multipart/form-data` body: a boundary-delimited stream of parts, each
with its own headers (`Content-Disposition: form-data; name="avatar";
filename="me.png"`). Kernway has no parser for it, so the one upload shape users
actually reach for is the one it cannot serve. `UploadFile` handles a raw
single-body `PUT`, which programmatic clients use — but not the form.

Expected outcome, measurable: a body over the threshold uses O(chunk) memory,
not O(body); a body over the ceiling is refused with `413` before spooling; a
`multipart/form-data` post with N files yields N `UploadFile`s each streamed to
disk, with peak memory bounded by the chunk size and the largest in-memory
field, not the upload size. The existing small-body fast path (`Json`,
`Validated`, `Form`) is byte-for-byte unchanged.

[KEP-0000 §3]: 0000-principles.md#3-solid--correct-at-the-edges-or-not-correct

## Guide-level explanation

Most handlers do not change. A `Json<T>`, a `Validated<T>`, a small form post —
all still read `Request.body` from memory, because a body under the threshold
(1 MiB by default) is still buffered exactly as before. The threshold, the
ceiling, and the spool directory are configured once on the builder or in
`application.properties`:

```rust
KernwayApp::builder()
    .max_inmemory_body(1024 * 1024)      // ≤ 1 MiB stays in memory
    .max_upload_size(4 * 1024 * 1024 * 1024) // > 4 GiB is refused with 413
    .upload_temp_dir("/data/uploads/tmp")    // spool here (same volume as the store → rename)
```

What is new is that a large body no longer sits in memory. A handler that
receives one asks for it by type.

**A single streamed body** — `PUT /songs/42` with the file as the whole body:

```rust
async fn upload(&self, file: UploadFile) -> impl IntoResponse {
    file.persist("/data/songs/42.mp3").await?;   // move it into place, off the shard
    StatusCode::CREATED
}
```

`UploadFile` is the path to a temp file the server already streamed the body
into. The handler does no reading; `persist` renames it into place (a copy+delete
only if the destination is on another filesystem), on the blocking pool. If the
handler never persists it, the temp file is deleted when the request ends.

**A browser form** — `multipart/form-data` with a text field and a file:

```rust
async fn create(&self, mut form: Multipart) -> impl IntoResponse {
    while let Some(part) = form.next().await? {
        match part.name() {
            "title" => { let title = part.text().await?; /* small: in memory */ }
            "cover" => { part.file()?.persist("/data/covers/x.png").await?; }
            _ => {}   // unknown field: skipped, its bytes drained
        }
    }
    StatusCode::CREATED
}
```

Each part is parsed as it arrives. A short text field (`title`) is read into a
`String`; a file field (`cover`) is spooled to disk the same way `UploadFile` is,
and `part.file()` hands back an `UploadFile`. The whole 2 GB never exists in
memory — at most one chunk plus the current small field.

The mental-model shift is one sentence: **a request body is not always bytes in
`Request.body` anymore** — over the threshold it is a file on disk, reached
through `UploadFile`, and a `multipart` body is a stream of those.

## Reference-level explanation

### The buffer/spool boundary

`Request` gains one field (mirrors `SpooledBody`, already in `kernway-core`):

```rust
pub struct SpooledBody { pub path: PathBuf, pub len: u64 }  // Drop deletes the temp file

pub struct Request {
    // ...
    pub body: Vec<u8>,                    // set when the body was buffered (≤ threshold)
    pub body_spool: Option<SpooledBody>,  // set instead when the body was streamed to disk
}
```

Exactly one of `body` / `body_spool` carries the body. `body_bytes()` reads
whichever is set (materialising the spool for the buffered extractors — `Json`,
`Validated` — which is why they still work above the threshold, at the cost of
one read; large uploads use `UploadFile`/`Multipart`, which never materialise).

`SpooledBody`'s `Drop` removes the temp file. `persist` renames it out first, so
a persisted upload is not deleted — the rename already moved it, and the `Drop`
on the now-missing path is a harmless best-effort `remove_file`.

### The connection task decides, before the handler

The threshold decision is in `serve_connection`, not in an extractor, because it
must happen while the bytes are still on the socket — an extractor runs after the
body is already somewhere. In order, after the head is parsed and `content-length`
is known:

1. `content_length > max_upload_size` → write `413`, close. No body is read; the
   ceiling is enforced before the first body byte is pulled off the socket.
2. `content_length ≤ max_inmemory_body` → the current fast path: finish buffering
   into `Request.body`.
3. otherwise → `spool_body`: stream the body to a temp file in `file_chunk`
   reads, each *file write* on the blocking pool (`spawn_blocking`), only the
   socket read on the shard. Memory is O(chunk). This is the inbound mirror of
   KEP-0002's `stream_file`, and it reuses the same chunk size and the same
   "I/O off the shard" rule ([KEP-0000 §4]).

`spool_body` consumes exactly `content_length` body bytes and leaves any
pipelined bytes of the *next* request at the front of the read buffer, so
keep-alive after a spooled upload still works. A read that stalls past the idle
timeout, or a peer that closes mid-body, aborts the upload and the connection —
the temp file's `Drop` cleans up.

[KEP-0000 §4]: 0000-principles.md#4-stable--never-block-never-surprise

### `UploadFile`

```rust
impl UploadFile {
    pub fn path(&self) -> &Path;
    pub fn len(&self) -> u64;
    pub async fn persist(self, dest: impl Into<PathBuf>) -> std::io::Result<()>;
}
```

`from_request` returns the spooled body, or an error if the body was buffered (it
was empty or under the threshold — the caller wants `req.body`/`Json` instead).
As an `Extract` (the `#[controller]` argument trait), a missing spool is a `400`.
`persist` tries `rename` and falls back to copy+remove across filesystems, all on
the blocking pool. Pointing `upload_temp_dir` at the same volume as the final
store keeps `persist` a rename — the difference between instant and copying a
gigabyte.

### `Multipart` (RFC 7578) — the new work

A streaming parser over a `multipart/form-data` body. The boundary comes from the
`Content-Type: multipart/form-data; boundary=…` header; the body is a sequence of

```
--boundary CRLF
Content-Disposition: form-data; name="field"[; filename="f.png"] CRLF
[Content-Type: … CRLF]
CRLF
<part body>
CRLF
--boundary   (next part)  … or  --boundary--  (end)
```

The extractor exposes an async iterator, not a materialised `Vec<Part>`, because
materialising would defeat the point:

```rust
impl Multipart {
    pub async fn next(&mut self) -> Result<Option<Part>, MultipartError>;
}
impl Part {
    pub fn name(&self) -> &str;
    pub fn filename(&self) -> Option<&str>;
    pub fn content_type(&self) -> Option<&str>;
    pub async fn text(&mut self) -> Result<String, MultipartError>;  // small fields
    pub fn file(self) -> Result<UploadFile, MultipartError>;         // file parts, spooled
}
```

Per part, the parser applies the **same** buffer/spool boundary as the whole
body: a part whose accumulated body stays under a per-part memory limit is a text
field held in a `String`; one that grows past it (or that carries a `filename`)
spools to a temp file and becomes an `UploadFile`. So the memory bound is
per-part, and the file case reuses `SpooledBody` — `Multipart` is `UploadFile`
applied N times behind a boundary scanner, not a second spooling mechanism.

The source the parser reads is the request body: from `Request.body` if the whole
multipart body fit in memory, or streamed from `body_spool` / the socket if it
did not. To keep the first cut tractable, **the multipart parser reads from the
already-received body** (`body_bytes()`): the outer threshold decides
memory-vs-disk for the whole body, and `Multipart` splits that into parts,
spooling large parts. Parsing *directly off the socket* — so the outer body is
never even spooled whole — is a later optimisation noted under Future
possibilities; it needs the parser to drive the socket read, which is a larger
change to `serve_connection`.

Limits, because multipart is a classic amplifier ([KEP-0000 §3]) and the limits
are part of the spec, not a later pass:

- `max_upload_size` still caps the whole body (enforced by the connection task
  before parsing).
- A cap on the **number of parts** (default e.g. 1000) — a body of a million
  empty parts is a cheap way to make the parser allocate a million times.
- A cap on the length of a **part header** block, so a part with a
  multi-megabyte `Content-Disposition` cannot be used to force unbounded header
  buffering.
- A missing or malformed boundary, a part with no `name`, or a truncated body
  each fail the extraction with a `400`, not a panic.

### Configuration

The three bounds load from `application.properties` (KEP-0007), so deployment
does not require a recompile, with the builder methods overriding:

```
kernway.upload.max-size        = 4GiB     # → max_upload_size (413 ceiling)
kernway.upload.buffer-limit    = 1MiB     # → max_inmemory_body (memory/disk boundary)
kernway.upload.temp-dir        = /var/tmp # → temp_dir
```

Sizes accept a plain byte count or a `KiB`/`MiB`/`GiB` suffix. Absent keys keep
the defaults (1 MiB / 4 GiB / `std::env::temp_dir()`).

### What this KEP does not specify

- **The response body** — settled in KEP-0002; untouched here.
- **`transfer-encoding: chunked` request bodies.** Bodies must still declare
  `content-length` (the runtime roadmap already tracks chunked as open). Spooling
  a chunked body needs the dechunker to drive the spool loop; deferred.
- **Parsing multipart straight off the socket** without spooling the outer body
  first — a Future possibility, not the first cut.
- **`application/x-www-form-urlencoded`** large bodies. Urlencoded forms are
  small by nature and stay a buffered `Form<T>`; this KEP is about the streamed
  cases.

## Drawbacks

**A request body is no longer always in `Request.body`.** Code that reads
`req.body` directly and assumes it holds the whole body is wrong above the
threshold — it must go through `body_bytes()` or an extractor. That is a real
mental-model change, and the mitigation (buffered extractors call `body_bytes()`,
which materialises the spool) trades a silent correctness trap for one extra file
read when someone uses `Json` on a body that happened to spool. The alternative —
never spool, always buffer — is the memory problem this KEP exists to fix.

**Spooling writes every large upload to disk, twice in the cross-device case.**
An upload that spools and then `persist`s across filesystems is written to the
temp volume and copied to the destination — two writes of the whole file. The
mitigation is operational (`upload_temp_dir` on the destination volume → rename,
one write), but it is a footgun: a default `temp_dir` on a different mount turns
every upload into a copy, silently. The default should be documented loudly, and
perhaps a warning logged when temp and a known store volume differ.

**A streaming multipart parser is materially more code than "buffer and split".**
The tempting version reads the whole body into memory and splits on the boundary
— far simpler, and wrong for exactly the large-upload case that motivates this.
The streaming parser has to handle a boundary that straddles a chunk edge, a part
header split across reads, and CRLF ambiguity at part ends. That complexity is the
cost of the memory bound; a reader who only ever accepts small forms would
reasonably find it overkill, and for them the buffered `Form<T>` still exists.

**Disk as a DoS surface.** Moving uploads off memory and onto disk trades one
exhaustion target for another: `max_upload_size` bounds a single body, but N
concurrent uploads of `max_upload_size − 1` can still fill the temp volume. A
total-concurrent-upload budget is not in this KEP, and its absence is a real gap
for a hostile environment — noted under Unresolved.

## Rationale and alternatives

**Never spool; raise the buffer limit.** Keep every body in memory, just allow a
bigger `Vec`. Rejected: it does not bound memory, it moves the cliff. The whole
point is that upload size and memory use must decouple, which only disk (or
socket-direct streaming) achieves.

**A `Body`-style enum on the request, symmetric with KEP-0002.** Model
`Request.body` as `Buffered(Vec<u8>) | Spooled(SpooledBody)` instead of two
fields with an invariant. Considered, and reasonable — it makes "exactly one is
set" unrepresentable-when-wrong. Rejected for now only because `Request.body:
Vec<u8>` is load-bearing in a great many call sites and handlers, and the
two-field form kept every one of them compiling while adding the spool path; the
enum is a follow-up refactor with the same churn argument KEP-0002 made for the
response side, worth doing but not blocking. This is the weakest of the
rejections and a future KEP may well flip it.

**A third-party multipart crate (`multer`, `multipart`).** Pull in an existing
parser rather than write one. Rejected on the KEP-0001 line: the parser is small,
security-sensitive, and on the hot path, and adopting one means adopting its body
abstraction (usually hyper's `Stream`), which Kernway does not have. Writing it
against `SpooledBody` keeps the upload path a single mechanism and the dependency
surface unchanged — the same reasoning that kept compression and the HTTP codec
in-house.

**Make handlers pull the body themselves (`req.body_reader()`).** Hand the
handler an `AsyncRead` and let it decide. Rejected: it pushes framing, limits,
and cleanup into every handler, which is exactly the error-prone surface
extractors exist to remove — and it cannot enforce the ceiling, because by the
time a handler runs the bytes are already arriving.

**Do nothing.** The server keeps buffering every upload in memory, has no
ceiling, and cannot accept a browser file form. Acceptable only if Kernway never
receives a file — which contradicts the same "deploy a real app" goal that
motivated serving them.

## Prior art

- **Spring `MultipartResolver` / `MultipartFile`.** Spring resolves a multipart
  request into `MultipartFile` objects, spilling to disk above a threshold
  (`spring.servlet.multipart.max-file-size`, `max-request-size`, `file-size-
  threshold`) — the exact three-bound model here, and the naming this KEP's
  config keys deliberately echo so a Spring user reads them without surprise.
  `MultipartFile.transferTo(dest)` is `UploadFile::persist`.
- **axum `Multipart` / `bytes` limits.** axum wraps `multer` and streams fields
  as an async iterator (`while let Some(field) = multipart.next_field().await?`),
  which `Multipart::next` mirrors. axum leaves the disk-spool policy to the
  application; Kernway builds spooling in, because the memory bound is the point,
  not an add-on.
- **Rails `ActionDispatch::Http::UploadedFile` + Rack multipart.** Rack's
  multipart parser spills parts over `Rack::Utils.multipart_part_limit` to
  `Tempfile`, and `UploadedFile` wraps the tempfile with `#tempfile` and a
  cleanup on request end — the `SpooledBody`-`Drop` model, arrived at
  independently, which is some evidence it is the right default.
- **Go `Request.ParseMultipartForm(maxMemory)` / `FormFile`.** Go's stdlib takes
  a single `maxMemory` and spills the remainder to temp files, returning
  `multipart.File` (which may be an in-memory reader or an `*os.File`). Same
  boundary idea; Go exposes the "might be memory, might be a file" seam directly,
  where Kernway hides it behind `UploadFile` and keeps the file case concrete for
  a later `persist`-by-rename.
- **nginx `client_max_body_size` / `client_body_temp_path`.** The ceiling-and-
  temp-dir pair, at the reverse proxy. `max_upload_size` and `upload_temp_dir`
  are the same two knobs inside the app, for deployments without a proxy in front.

## Unresolved questions

- **A concurrent-upload disk budget.** `max_upload_size` bounds one body; nothing
  bounds the sum across in-flight uploads, so the temp volume is a shared
  exhaustion target. A total budget (or a temp-volume free-space check before
  spooling) belongs somewhere — this KEP, or a later hardening one? Leaning
  toward a follow-up, but the gap is real and should be named in the docs now.
- **Parsing multipart directly off the socket.** The first cut spools the whole
  body (if over threshold) then parses; a body of many large files is written to
  disk once as one blob and again per persisted part. Socket-direct parsing
  avoids the first write but needs the parser to drive the read loop in
  `serve_connection`. Worth it, or is the extra write acceptable given `persist`
  is usually a rename? Deferred.
- **`If a part has its own `Content-Encoding``** (a gzipped part) — decode
  transparently, like the response path, or hand the raw bytes to the handler?
  Almost never used in `form-data`; leaning raw, but unspecified.
- **Per-part memory limit default.** The whole-body `buffer-limit` has a
  measured-ish default (1 MiB); the per-part text/file boundary needs its own,
  and it should be smaller (a text field is bytes, not megabytes). A guess until
  there is a bench.

## Future possibilities

- **A request `Body` enum** symmetric with KEP-0002, making the buffer/spool
  invariant unrepresentable-when-wrong — the refactor the two-field form defers.
- **Socket-direct multipart**: the parser drives the read, so a multi-file form
  never spools the outer body whole — only each file part lands on disk, once.
- **`transfer-encoding: chunked` request bodies**, once the dechunker exists —
  the spool loop reads from the dechunker instead of a fixed `content-length`.
- **Content validation hooks** on a spooled upload (magic-byte type sniffing, a
  virus-scan handoff) before `persist`, since the file is already on disk and
  named.
- **Resumable uploads** (`tus`, or `Content-Range` on `PUT`) layered on
  `UploadFile` + a keyed temp path — the spool file is the natural building block.
