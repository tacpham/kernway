# kernway-static — request path → safe file path, and MIME

## Purpose

Turn a request path into a filesystem path that is guaranteed to sit under a
configured root, and name a file's MIME type. Pure logic: there is no I/O in this
crate.

The split is the whole design. Path safety — where a directory traversal is
stopped — is decided *before* any file is opened, so it is testable without a
filesystem and every attack string is a unit test. The file read is async I/O and
belongs to `kernway-server`, where it runs on the blocking pool so it never
stalls a core.

**Not** in scope: reading files, streaming, HTTP semantics (ETag, Range,
conditional requests), or serving — all of that is the caller's. This crate
answers one question, "what safe path does this URL mean, if any," and one lookup,
"what MIME type is this."

## Status

As of 2026-07-24. Landed in the M1 slice.

| Area | State | Notes |
|---|---|---|
| Path resolution under a root | ✅ | `resolve()` — lexical containment |
| Percent-decoding before the check | ✅ | so `%2e%2e` cannot smuggle `..` |
| Traversal / dotfile / illegal-byte rejection | ✅ | 21 unit tests |
| Index file for directory requests | ✅ | `/` and `/dir/` → `index.html` |
| MIME by extension | ✅ | hand-written table, ~18 types |
| Symlink re-check at open time | ❌ | needs I/O — belongs with the read in `kernway-server`. M2. |
| Precompressed variants (`.br`/`.gz`) | ❌ | M2 |

**Today**: `StaticFiles::new(root).resolve(url_path)` returns a safe `PathBuf` or
a typed `Rejected`. `mime_for(path)` names the type.

**Not yet**: anything requiring the filesystem, because by design nothing here
touches it.

## Standards

| Spec | Scope | Compliance |
|---|---|---|
| RFC 3986 §2.1 | Percent-decoding of the path | full for the path; query string is not this crate's concern |
| OWASP Path Traversal | Reject `..`, encoded `..`, NUL, backslash | full — lexical, pre-I/O |
| IANA media types | `Content-Type` by extension | partial — a curated subset, not the full registry |

The traversal defence has a test per attack shape (`..`, `%2e%2e`, mid-path `..`,
backslash, NUL, bad encoding), which is the rule from
[KEP-0000 §3](../../kep/0000-principles.md#3-solid--correct-at-the-edges-or-not-correct):
a security claim is a test, not a sentence.

## Architecture

```text
url_path ("/%2e%2e/style.css")
   │
   ▼  percent_decode      → "/../style.css"     (or Err BadEncoding)
   ▼  reject NUL/ctrl/\\   → IllegalByte on hit
   ▼  split('/'), per segment:
   │     ".." → Err Traversal
   │     ".xxx" → Err Dotfile
   │     "." / "" → skip
   │     else → push onto root
   ▼  trailing '/' or no segment → push index
   ▼
Ok(root/style.css…)  or  Err(Rejected)
```

Containment is **lexical**: because no `..` segment survives the loop, the joined
path cannot climb above the root, and this holds without consulting the disk.
The one thing lexical checks cannot see is a symlink *inside* the root pointing
*outside* it — that is the single ❌ in Status, and it is closed at open time in
the caller (M2).

## Public surface

```rust
pub struct StaticFiles { /* root, index */ }
impl StaticFiles {
    pub fn new(root: impl Into<PathBuf>) -> Self;
    pub fn index(self, name: impl Into<String>) -> Self;
    pub fn resolve(&self, url_path: &str) -> Result<PathBuf, Rejected>;
    pub fn root(&self) -> &Path;
}

pub enum Rejected { Traversal, Dotfile, IllegalByte, BadEncoding }

pub fn mime_for(path: &Path) -> &'static str;
```

**Stability**: the shape is stable; `Rejected` may gain variants (a new refusal
reason is additive). `mime_for` may gain entries. Neither is a breaking change.

## Integration

**Depends on**: nothing — std only. This is the point, per
[KEP-0000 §1](../../kep/0000-principles.md#1-ours--write-it-do-not-import-it):
a curated MIME table and hand-rolled percent-decoding cost less than a dependency
on the request hot path.

**Depended on by**: `kernway-server` (wires the read), and it is a baseline crate
of the meta-crate — a fresh `kernway` serves static files with no feature flag.

**Must never depend on**: `kernway-server`, `kernway-core`, or anything with I/O.
The moment this crate opens a file it stops being unit-testable without a
filesystem, which is the property that makes the traversal defence trustworthy.

## Speed

| Path | Runs | Cost | Bench |
|---|---|---|---|
| `resolve`, no `%` | every static request | one `split`, a few comparisons, no alloc beyond the result `PathBuf` | ❌ to write |
| `resolve`, percent-encoded | requests with `%` | one extra pass to decode | ❌ to write |
| `mime_for` | every static response | extension match, no alloc | ❌ to write |

**Allocation policy**: `resolve` allocates exactly one `PathBuf` (the result);
the no-`%` path avoids the decode buffer entirely by returning early. `mime_for`
allocates nothing — it lowercases into a small stack string only when an
extension is present. No benchmark yet; the numbers belong in
[BENCHMARKS.md](../BENCHMARKS.md) once written.

## Generic — the extension points

None, and that is correct. This crate is a leaf: a single safe algorithm and a
lookup table. A caller that wants different behaviour (a different root strategy,
files from an embedded bundle or S3) implements its own resolver rather than
configuring this one — the `FileSource` abstraction for that lives in the
`kernway-server` charter's roadmap, not here.

## Security

The reason this crate exists as a separate, I/O-free unit is that path safety is
too important to be tangled with file reading. See the
[kernway-server Security table](kernway-server.md#security) for the live-tested
row; the defence itself is here, and its tests are the proof.

| Threat | Answer | Tested |
|---|---|---|
| `../` traversal | segment `..` → `Rejected::Traversal` | ✅ |
| Encoded traversal `%2e%2e%2f` | decode before the check | ✅ |
| Dotfiles `.env`, `.git/` | any segment starting `.` → `Rejected::Dotfile` | ✅ |
| NUL / control / backslash | `Rejected::IllegalByte` | ✅ |
| Malformed `%` encoding | `Rejected::BadEncoding` | ✅ |
| Non-UTF-8 after decode | `Rejected::IllegalByte` (not lossy replacement) | ✅ |
| Symlink escaping the root | ❌ not here — canonicalize-at-open, `kernway-server` M2 | ❌ |

## Direction

| Phase | Goal | Blocked by |
|---|---|---|
| M1 | resolve + MIME, wired into the server read | — (done) |
| M2 | symlink re-check (with the caller's open), HEAD/Range support in the server, precompressed variant selection | async `Body::File` |
| later | a `FileSource` trait so the same resolver serves embedded or remote files | a real need |

**Deliberately out of scope**: reading, caching, ETag computation, HTTP framing.
Those are `kernway-server`'s, so that this crate stays a pure, exhaustively
testable safety boundary.

## Open questions

- Should `resolve` also reject a path longer than some bound, as a cheap
  belt-and-braces against pathological inputs? Currently unbounded.
- Windows: backslash is rejected outright today. Correct for safety, but it means
  a genuinely backslash-named file on Windows is unreachable. Acceptable?

## Related KEPs

| KEP | Bearing |
|---|---|
| [0000 §1](../../kep/0000-principles.md#1-ours--write-it-do-not-import-it) | Why no `mime_guess` / `percent-encoding` dependency |
| [0000 §3](../../kep/0000-principles.md#3-solid--correct-at-the-edges-or-not-correct) | Why every traversal shape is a test |
| [0000 §4](../../kep/0000-principles.md#4-stable--never-block-never-surprise) | Why the read is elsewhere — it must not block a core |
