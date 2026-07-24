# kernway-security — CSRF, security headers, authorization context

## Purpose

The web-security primitives a server-rendered Kernway app needs, each pure and
testable: **security response headers**, **CSRF** token issue/verify, and an
**authorization context** (`SecurityContext`) that `th:authorize` in `kernleaf`
and route guards consult. It is the crate `kernleaf` slice F depends on.

**Not** in scope: authentication itself (verifying a password, validating a JWT,
an OAuth flow) — that produces a `SecurityContext`, and how it does so is the
application's or an auth-provider crate's concern. This crate is what you check
*against*, not how you log in. Also not here: session storage (the CSRF design is
deliberately stateless), rate limiting, and CORS (a separate middleware).

## Status

As of 2026-07-24.

| Area | State | Notes |
|---|---|---|
| Security headers — strict baseline + builders | ✅ | nosniff, frame-options, CSP, referrer, HSTS, permissions-policy |
| `SecurityHeadersLayer` middleware | ✅ | adds headers to every response via the `Layer` trait |
| CSRF token — generate (OS RNG), 64-hex | ✅ | `getrandom`, the one justified dependency |
| CSRF verify — constant-time, double-submit | ✅ | `verify`, `verify_request` (cookie vs header/form) |
| CSRF cookie — `HttpOnly; SameSite=Lax; Secure` | ✅ | `set_cookie(secure)` |
| `SecurityContext` — principal + roles | ✅ | `is_authenticated`, `has_role`, `has_any_role` |
| `Authorization` trait (in `kernway-core`) | ✅ | `SecurityContext` implements it; `kernleaf`'s `th:authorize` consults it |
| Auto-CSRF form injection, `th:authorize` | ✅ | wired in `kernleaf` slice F — form auto-injects `_csrf`, `th:authorize` gates elements |
| Auth providers (password, JWT, session) | ❌ | out of scope — they *produce* a `SecurityContext` |

**Today**: 12 unit tests + a doctest. Everything a template and a guard need to
enforce CSRF, ship secure headers, and check roles.

## Standards / threat model

This crate is where a security claim has to be a test, not a sentence
([KEP-0000 §3](../../kep/0000-principles.md#3-solid--correct-at-the-edges-or-not-correct)) —
and, this repo being public, an un-mitigated row here would be a published attack
recipe, so there are none.

| Threat | Answer | Tested |
|---|---|---|
| CSRF (forged state-changing request) | double-submit token: cookie value must match the `_csrf` form field or `X-CSRF-Token` header | ✅ `verify_request…` |
| Token guessing | 32 bytes of OS randomness (`getrandom`), hex-encoded | ✅ `a_token_is_64_hex…` |
| Token timing oracle | constant-time compare — no early return on first differing byte | ✅ `verify_is_exact` (correctness; the compare is branch-free) |
| Cookie theft via XSS | CSRF cookie is `HttpOnly` — JS cannot read it; the form field is server-rendered | ✅ `the_cookie_is_httponly…` |
| Cross-site cookie send | `SameSite=Lax` | ✅ (same) |
| MIME sniffing | `X-Content-Type-Options: nosniff` | ✅ `strict_headers…` |
| Clickjacking | `X-Frame-Options: DENY` (+ CSP `frame-ancestors` available) | ✅ (same) |
| Mixed-content / downgrade | `Strict-Transport-Security` (over HTTPS) | ✅ (same) |
| Injection into the page | `Content-Security-Policy: default-src 'self'` by default | ✅ (same) |

RFC references: the headers follow the Fetch/CSP Level 3, RFC 6797 (HSTS), and the
OWASP CSRF and Secure-Headers cheat sheets.

## Why the double-submit pattern

CSRF defences need either server state (a synchronizer token in a session) or a
second channel the attacker cannot forge. Kernway has no session store yet and
prefers statelessness (thread-per-core, no shared session map on the hot path), so
**double-submit** is the fit: the server generates a token, sets it in a cookie
*and* renders it into the form, and verifies the two match on submit. An attacker
on another origin can neither read the victim's cookie (it is `HttpOnly`) nor set
one for our domain, so it cannot produce a matching pair. It works with an
`HttpOnly` cookie precisely because the form field is server-rendered (slice F),
not read from the cookie by JavaScript.

## Public surface

```rust
// Headers
pub struct SecurityHeaders { /* … */ }
impl SecurityHeaders {
    pub fn strict() -> Self;  pub fn new() -> Self;
    pub fn content_security_policy(self, v) -> Self;  // + frame_options, referrer_policy, hsts, content_type_options
    pub fn headers(&self) -> Vec<(&'static str, String)>;
    pub fn apply(&self, resp: &mut Response);
}
pub struct SecurityHeadersLayer(pub SecurityHeaders);   // impl Layer

// CSRF
pub mod csrf {
    pub const COOKIE: &str;  pub const FIELD: &str;  pub const HEADER: &str;
    pub struct CsrfToken(/* … */);
    impl CsrfToken { pub fn generate() -> Self; pub fn from_value(v) -> Self;
                     pub fn as_str(&self) -> &str; pub fn set_cookie(&self, secure: bool) -> String; }
    pub fn verify(submitted: &str, expected: &str) -> bool;      // constant-time
    pub fn token_from_cookie(cookie_header: &str) -> Option<&str>;
    pub fn form_field(body: &str, name: &str) -> Option<String>;
    pub fn verify_request(req: &Request) -> bool;
}

// Authorization
pub struct SecurityContext { /* authenticated, principal, roles */ }
impl SecurityContext {
    pub fn anonymous() -> Self;
    pub fn authenticated(principal, roles) -> Self;
    pub fn is_authenticated(&self) -> bool;  pub fn principal(&self) -> Option<&str>;
    pub fn has_role(&self, role: &str) -> bool;  pub fn has_any_role(&self, roles: &[&str]) -> bool;
}
```

**Stability**: the shapes are stable. `SecurityHeaders` may gain builders and
`csrf`/`SecurityContext` may gain helpers — additive.

## Integration

**Depends on**: `kernway-core` (for `Request`/`Response`/`Layer`) and `getrandom`.
The `getrandom` edge is a **deliberate exception** to
[KEP-0000 §1](../../kep/0000-principles.md#1-ours--write-it-do-not-import-it): a
CSRF token must be unpredictable, and a hand-rolled CSPRNG is precisely the
"solid" failure §1's fourth test guards against. `getrandom` is the OS RNG — small,
audited, the responsible primitive, exactly as `mio` is for I/O readiness.

**Depended on by**: `kernleaf` (slice F: `th:authorize` reads `SecurityContext`,
auto-CSRF renders the `_csrf` field from a `CsrfToken`), and any route guard. It
will be an opt-in meta-crate feature.

**Must never depend on**: `kernway-server` or a specific auth provider. It defines
what is checked; producing a `SecurityContext` (login) lives elsewhere.

## Speed

Not the point here — correctness is — but nothing is heavy. Headers build a small
`Vec`; a CSRF verify is one length check and a branch-free XOR loop; a role check
is a `HashSet` lookup. Token generation reads 32 bytes from the OS once per form
render, never on a verify. No benchmark yet; if one lands it measures the verify,
which must stay constant-time regardless of speed.

## Generic — the extension points

`SecurityHeaders` is a config, not a policy — an app composes the exact header set
it wants. `SecurityContext` carries opaque role strings, so an app's role scheme
(flat roles, `ROLE_`-prefixed, scopes) is its own choice. Authentication is the
deliberate extension seam: anything that can build a `SecurityContext` plugs in.

## Direction

| Phase | Goal | Blocked by |
|---|---|---|
| now | headers, CSRF, `SecurityContext` | — (done) |
| kernleaf F | `th:authorize` consults the context; forms auto-inject `_csrf` | — (this crate) |
| later | a route-guard layer (`require_role`) that 403s before the handler | the context in request state |
| later | CSRF middleware that auto-sets the cookie and rejects bad POSTs | request-scoped state wiring |
| later | a CORS layer, a rate-limit layer | a real need |

## Open questions

- **Where does `SecurityContext` live per request?** It needs to reach both a
  handler and the template `Env`. A request-extension/`Rc` slot (cheap under
  thread-per-core) is the likely home — to be settled when the guard layer lands.
- **CSRF exemptions**: safe methods (GET/HEAD/OPTIONS) never need a check, and an
  API using bearer tokens may opt out. The middleware needs a clear, safe-by-
  default policy for which requests it enforces.
- **CSP nonce/hash** support for inline scripts, once `th:inline` output wants a
  nonce threaded through.

## Related KEPs

| KEP | Bearing |
|---|---|
| [0000 §1](../../kep/0000-principles.md#1-ours--write-it-do-not-import-it) | Why `getrandom` is the one justified dependency (don't hand-roll a CSPRNG) |
| [0000 §3](../../kep/0000-principles.md#3-solid--correct-at-the-edges-or-not-correct) | Why every threat-table row is a test, and none is an un-mitigated ❌ |
| [0000 §4](../../kep/0000-principles.md#4-stable--never-block-never-surprise) | Why the request-scoped context is a plain value (thread-per-core) |
