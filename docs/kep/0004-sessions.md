---
kep: 0004
title: Sessions — a signed token backed by a revocable server-side registry
status: Accepted
created: 2026-07-24
decided: 2026-07-24
---

# KEP-0004: Sessions — a signed token backed by a revocable registry

## Summary

A login issues a **signed session token** (an HMAC-signed cookie) that carries the
user's identity — `sid` (session id), `user`, `roles`, `exp` — so a request reads
who you are without a store lookup. The server also keeps a **session registry**:
`sid → record`, one row per login. Every request verifies the token *and* checks
its `sid` is still in the registry. **Deleting a row logs that session out
immediately.** Two rows for the same user mean two active logins (two devices);
deleting all of a user's rows is "log out everywhere". This is the deliberate
hybrid of a JWT (fast, self-describing) and a server-side session (revocable),
chosen because pure JWT's only revocation is a blacklist.

## Motivation

Pure JWT is stateless and fast, and in exchange **cannot be revoked**. Once issued,
a token is valid until it expires; there is no server-side "log this session out
now". The field workaround is a blacklist — a growing server-side list of revoked
tokens checked on every request — which is a session store bolted on backwards, and
the thing teams reach for after being burned:

- A user logs out, but their token keeps working until expiry.
- An account is compromised; you cannot force-terminate its sessions.
- "Sign out all other devices" is impossible without server state.
- A leaked signing key lets an attacker mint tokens with no way to stop them
  short of rotating the key and invalidating *everyone*.

Pure server-side sessions fix revocation but pay a full store fetch for the session
*data* on every request, and the cookie is an opaque id that says nothing.

Expected outcome, concrete: after this ships, `logout(sid)` makes that session's
next request anonymous; `logout_user(user)` does it for every device; a request
still reads identity from the token without fetching session data; and the registry
check is a cheap membership read, not a full fetch. Measurable: a revoked session is
rejected on its very next request, and the per-request authorization cost is one
HMAC verify plus one registry `contains` — no I/O in the in-memory backend.

## Guide-level explanation

At login, the server calls the session manager, which:

1. creates a random `sid`,
2. records `sid → { user, roles, created, meta }` in the registry,
3. signs a token carrying `{ sid, user, roles, exp }` and sets it as a cookie.

```rust
let token = sessions.login("alice", ["ADMIN"], device_meta)?;   // → Set-Cookie
```

On each request, an auth layer turns the cookie back into a `SecurityContext`:

```rust
let ctx = sessions.authenticate(cookie);   // Some(SecurityContext) or anonymous
```

`authenticate` verifies the signature and expiry (from the token alone), then
checks the registry still contains `sid`. If the signature is bad, the token is
expired, or the `sid` was removed — **anonymous**. Otherwise you get a
`SecurityContext` with the user and roles (from the token, or refreshed from the
record — see below).

Logging out is deleting a row:

```rust
sessions.logout(sid);          // this device
sessions.logout_user("alice"); // every device this user is logged in on
```

Because there is a row per login, the registry is also the answer to "where am I
logged in": list the rows whose `user` is you.

**Mental model shift for someone arriving from JWT:** the token is still
self-describing and verified with a key, but it is no longer the *sole* authority.
A token is only live while its `sid` is in the registry — so the registry, not the
expiry, is what "logged in" means. The token buys speed (identity without a fetch);
the registry buys control (revocation).

## Reference-level explanation

### The token

A compact signed string, cookie-sized, `payload.signature`:

- `payload` — the claims `{ sid, user, roles, version, exp }`, encoded (e.g.
  base64url of a small, fixed serialization; not necessarily JWT's JSON, though
  JWT-compatible encoding is an option). `version` is the account version at login,
  compared against the account's current version to force re-login on a change (see
  the account seam below); it is absent when no `AccountStatus` provider is used.
- `signature` — `HMAC-SHA256(key, payload)`, base64url. Verified in constant time.

`exp` bounds the token's life even if the registry check were skipped; keep it
short (minutes to hours) with re-issue, so a stale token self-limits.

### The registry

```rust
struct SessionRecord {
    user: String,
    created: u64,        // unix seconds — for the absolute timeout
    last_seen: u64,      // unix seconds — for the idle timeout, advanced lazily
    meta: SessionMeta,   // device / ip / user-agent, for the "my sessions" list
}

trait SessionStore: Send + Sync {
    fn insert(&self, sid: &str, record: SessionRecord);
    fn get(&self, sid: &str) -> Option<SessionRecord>;   // also the membership check
    fn remove(&self, sid: &str);
    fn remove_user(&self, user: &str);                   // logout everywhere
    fn sessions_of(&self, user: &str) -> Vec<(String, SessionRecord)>;
}
```

The default backend is in-memory: `RwLock<HashMap<String, SessionRecord>>`. The
per-request path is a **read** (`get`/membership), which an `RwLock` serves
concurrently; `insert`/`remove` are writes and rare (login/logout). This is the one
deliberate piece of shared state, and it is read-mostly and bounded — distinct from
"request data on the hot path", which thread-per-core keeps lock-free.

A `SessionStore` is a trait so a multi-instance deployment swaps in Redis or SQL
without touching the manager; the in-memory backend is for a single instance and
for tests.

### The manager

`SessionManager<S: SessionStore>` holds the store, the signing key, and a live
`SessionConfig`:

- `login(user, roles, meta) -> Token` — new `sid`, `insert`, sign.
- `authenticate(cookie) -> SecurityContext` — verify signature + `exp`; if
  `store.get(sid)` is `None`, return anonymous (revoked); enforce the current
  timeouts (below); else build the context.
- `logout(sid)`, `logout_user(user)`, `sessions_of(user)`.

### Timeouts, configured live

```rust
struct SessionConfig {
    token_ttl: Duration,          // the `exp` baked into a new token
    absolute_timeout: Duration,   // max session age from `created`, enforced server-side
    idle_timeout: Option<Duration>, // max gap since `last_seen`; None = no idle limit
    max_sessions: Option<usize>,  // capacity cap; None = bounded only by memory
}
```

The config is held **live** — behind an `Arc<RwLock<SessionConfig>>` (or atomics
for the scalar fields) — and read on every `authenticate`. Changing it takes effect
**immediately, for existing sessions too**, with no redeploy:

- **Absolute timeout** is checked as `record.created + absolute_timeout < now`.
  Because this is re-evaluated server-side against the *current* config, dropping
  the timeout from 30 min to 5 min instantly expires every session older than 5 min
  — the token's baked `exp` is only a fail-safe upper bound. **This live-config
  behaviour is a property of the hybrid**: a pure JWT freezes `exp` at issue and
  cannot be shortened after the fact.
- **Idle timeout** is `record.last_seen + idle_timeout < now`. It needs `last_seen`
  to advance, which is a *write*; to keep the registry read-mostly, `last_seen` is
  updated **lazily** — only when it is more than a threshold (e.g. 60 s) behind, so
  a burst of requests from one session does not write on each.
- A session that fails either check is treated exactly like a revoked one:
  anonymous, and its record is removed on the way out (lazy eviction).

Config reaches the manager the way any app config does; once the M5 config reload
lands, editing the value in the running app is enough — "set it and it takes
effect" needs no new code because the manager already reads the value live rather
than capturing it.

### Capacity and eviction

The in-memory store is a `HashMap<String, SessionRecord>`, so it is bounded by RAM,
not by a fixed cap. A record is roughly 300–400 bytes (a 64-hex `sid`, the user, a
few roles, `meta`, plus map overhead), so:

| Live sessions | Approx memory |
|---|---|
| 100 K | ~35 MB |
| 1 M | ~350 MB |
| ~2.5–3 M | ~1 GB |

Two mechanisms keep the map from growing without bound:

- **TTL eviction** — an expired record (absolute or idle) is removed lazily on the
  `authenticate` that finds it stale, and a periodic sweep reaps sessions that are
  never touched again. So the resident set tracks *active* sessions, not cumulative
  logins.
- **`max_sessions`** — an optional hard cap. At the cap, a new `login` either
  evicts the oldest record or is refused (a deliberate choice, defaulting to refuse
  so an attacker cannot evict a victim's session by flooding logins). This bounds
  memory and is the backstop against a login-flood DoS.

Beyond one instance's RAM, or for sessions that must survive a restart or be shared
across instances, the `SessionStore` trait swaps in Redis or SQL, whose capacity is
the backing store's, not the process's.

### Injected, not wired by hand

The pieces are Kernway DI beans, so a handler or middleware asks for what it needs
and the container supplies it (`#[component]` / `#[inject]`, `AppContext`):

- `SessionManager` — a bean; the auth middleware injects it to `authenticate`, a
  logout handler injects it to `logout`/`logout_user`.
- `SessionStore` — a bean behind the trait; the in-memory one by default, a Redis
  one dropped in by registering a different component, no other code changing.
- `AccountStatus` — the application registers **its own** `#[component]` over its
  user repository; the manager injects it (as `Option`, since it is opt-in).
  "Get the account info right now" is just `accounts.account(user)` on the injected
  bean.
- `SecurityContext` — request-scoped: the auth middleware builds it and puts it in
  request state; a handler injects it to read the current user/roles, and the
  template `Env` reads it for `th:authorize`.

So there is no manual plumbing: implement `AccountStatus` over your database,
register it as a component, and every `authenticate` consults it.

### TTL: one authority, not two clocks

A session's expiry must never be decided in two places that can drift. The rule:
**the registry is an index of *live* sessions, not a second expiry clock.** The
authoritative expiry is computed at `authenticate`, from state read live —

- the session's age against the current `SessionConfig` (server-side,
  hot-reloadable), and
- the account's `expires`, read fresh from the DB via `AccountStatus`.

The registry's own storage TTL — a Redis `EXPIRE`, or the in-memory sweep — is only
a **garbage-collection backstop**. It is set to at least the maximum session
lifetime (and refreshed on activity), so it can never evict a session the
authoritative check would still accept; it only reaps records nobody returns to.
Two consequences:

- Shortening `SessionConfig.absolute_timeout` from 30 min to 5 min takes effect at
  the authenticate check immediately; the Redis key may still carry a 30-min
  `EXPIRE`, but the live check rejects (and lazily removes) the session at 5 min, so
  the registry TTL never gets the chance to disagree.
- Subscription `expires` lives in the DB and is read live; it is **never copied**
  into the token or the registry, so there is nothing to keep in sync — the DB is
  its one clock.

So the DB and the registry do not "match" TTLs: the DB owns subscription expiry
(read live), `SessionConfig` owns session timeout (checked live), and the registry
TTL is a loose upper bound for cleanup — always *more* lenient than the authority,
never stricter. One decision point, at authenticate; every stored TTL is a backstop.
(Session durations should use one vocabulary across the framework — the `Ttl` type
that `kernway-cache` already defines is the natural shared representation, lifted to
a common crate if security should not depend on cache.)

### One outcome: account changes invalidate the session (an optional seam)

Session timeout, subscription expiry, deactivation, and a role change all reduce to
**one thing: the session is no longer valid, so the user is logged out and must
re-authenticate.** They are not four behaviours — they are four reasons for the same
verdict. Making them converge is what keeps the design small.

Two concerns still layer cleanly:

- **The session** — this KEP: the token, `sid`, registry, timeout, revocation.
- **The account** — is this *user* still allowed? `active` (enabled / not banned), a
  subscription `expire`, and a `version` that the application bumps on **any change
  that must end existing sessions** — a role edit, a deactivation, an expiry change,
  a password reset. This state is the application's, and it changes.

KEP-0004 does not own account state; it consults it through an **optional seam** at
`authenticate`, where it already touches the store:

```rust
/// Optional. When set, checked on every authenticate — so a change ends the
/// session on the very next request.
trait AccountStatus: Send + Sync {
    fn account(&self, user: &str) -> Option<Account>;
}
struct Account {
    active: bool,          // false → session invalidated (disabled / banned)
    expires: Option<u64>,  // subscription end (unix s); Some(t) with t < now → invalidated
    version: u64,          // bumped by the app on any change that must force re-login
}
```

The token carries the account `version` and the roles it was minted with. On
`authenticate`, with a provider set, the session is invalidated (its `sid` removed,
result anonymous) if **any** of these hold: `None` (user gone), `!active`,
`expires < now`, or `account.version != token.version`. That last one is the whole
role story: **the app bumps `version` when it changes a user's roles, and the
version mismatch logs every one of that user's sessions out** — so their next
request re-authenticates and the new token is minted with the new roles. `version`
is also "log this user out everywhere" for free: bump it.

Because a role change forces re-login, **the roles carried in the token are never
stale** — any change that would make them wrong has already invalidated the token.
This is why the token can carry roles safely and the fast path (roles from the
token, no per-request role fetch) stays correct.

**When no provider is set** — a simple app with no subscriptions or bans — none of
`active`/`expire`/`version` exist; the session lives out its timeout and roles come
from the token. The whole account layer is opt-in: implement one trait over your
user table, or ignore it.

The one distinction to hold onto: **session timeout** bounds how long a *login*
lasts (re-login just re-issues a token); **account `expire`** is the *subscription's*
end and denies even a fresh login. Orthogonal, both checked.

### The signing key, and what a leak costs

The HMAC key is the token's integrity. A leak lets an attacker forge a token's
claims — but the forged token must still name a `sid` that exists in the registry
to pass, which the attacker cannot create without a real login, and a copied real
`sid` is revoked the moment that session is. On suspected key leak, clearing the
registry (or rotating the key) terminates all sessions. So the registry contains
the blast radius that pure JWT cannot.

### HMAC-SHA256 — ours

SHA-256 is a fixed, published algorithm with NIST test vectors; unlike a CSPRNG,
implementing it is deterministic and fully testable, so it is written in
`kernway-security` and checked against the standard vectors, per
[KEP-0000 §1](0000-principles.md#1-ours--write-it-do-not-import-it). HMAC is the
thin standard wrapper (RFC 2104), with a constant-time compare on verify. (If the
implementation cost is later judged not worth it, a single audited hash crate is
the fallback — but the default intent is ours, tested against the vectors.)

### What this KEP does not specify

- **Authentication itself** — checking a password, an OAuth exchange. That is what
  *produces* the `login(user, roles, …)` call; how, is the app's or an auth
  provider's concern.
- **Account storage** — the user table holding `active`, subscription `expire`, and
  roles is the *application's*, not the session machinery's. KEP-0004 defines only
  the `AccountStatus` seam to read it at `authenticate`; how those values are stored
  and edited (a DB, an admin UI) is out of scope, and the seam is optional.
- **CSRF** — a separate mechanism (KEP is not needed; `kernway-security::csrf`
  exists). Sessions and CSRF are orthogonal and both apply to a form POST.
- **Token encoding wire format** — whether the payload is JWT-compatible JSON or a
  tighter kernway encoding is an implementation choice, left open below.
- **Refresh tokens** — re-issuing near expiry to extend a session without a fresh
  login. The absolute/idle timeouts are specified here; automatic *re-issue* on top
  of them is a follow-on.

## Drawbacks

**It is not stateless.** Every request reads the registry, so a horizontally scaled
deployment needs a shared store (Redis), reintroducing the network hop and the
dependency that pure JWT avoids. For a single static binary the in-memory store is
free; at scale, the store is real infrastructure. A reader who genuinely needs
statelessness and can tolerate no revocation would rightly prefer plain JWT.

**A store read per request once the account seam is used.** Without an
`AccountStatus` provider, `authenticate` is a token verify plus a registry
membership check. With one — the case a subscription app needs — it also reads the
account (`active`/`expires`/`version`) each request, which for a shared/remote store
is another lookup. It is the price of "a change logs them out on the next request",
and an app that does not need that simply does not set the seam. (Note the role
staleness that a naive token-roles design would have is *avoided* here: a role
change bumps `version`, which invalidates the session, so the token's roles are
never wrong — they were minted after the last change.)

**A signing key to protect and rotate.** Pure server-side sessions have only an
opaque id — nothing to forge, no key to leak. This design reintroduces a key as a
critical secret, and rotation (invalidating old tokens without logging everyone out)
needs care (accept two keys during a window).

**More moving parts than either pure approach.** A token codec, an HMAC, a store
trait, a manager, and an auth layer — versus "sign a JWT" or "look up a cookie".
The complexity is the price of wanting both revocation and a store-free identity
read, and a team that needs only one of those is carrying parts it does not use.

## Rationale and alternatives

**Pure JWT (stateless).** Rejected as the default for exactly the motivating pain:
no revocation but a bolted-on blacklist, which is a worse session store. Kept
available in spirit — a deployment can ignore the registry check — but not the
recommended path.

**Pure server-side sessions (opaque id + store).** A strong alternative: fully
revocable, no signing key, fresh roles always. Rejected as the sole design because
every request then fetches session *data*, and the cookie carries no identity, so
even a health check that only needs "is this an admin" pays a store round trip. The
hybrid lets the token answer identity and reserves the store for the revocation
check and the data that must be fresh.

**Blacklist on top of JWT.** The common workaround, and explicitly what this
replaces: a blacklist is a store of *revoked* ids that grows until tokens expire and
must be checked on every request — the same per-request store cost as an allowlist,
but inverted so it can only ever grow and can never answer "where am I logged in".
An allowlist (this design) is the same cost done right: it shrinks on logout and
*is* the session list.

**Do nothing.** Kernway ships no session mechanism, and every app rolls its own —
which, given the JWT-revocation trap, most will get wrong in the same way. A
framework that renders a login form (KEP-0003, kernleaf) but leaves "stay logged in
safely" to the user has not finished the story.

## Prior art

- **Spring Security** offers both stateless JWT (`oauth2ResourceServer`) and
  server-side `HttpSession`, and its guidance for revocable JWT is precisely this:
  keep a server-side store of valid token ids. This KEP makes that the default
  rather than an advanced add-on.
- **Django / Rails** default to server-side sessions (opaque cookie + store) —
  revocable, but a store fetch per request and no self-describing token. This design
  keeps their revocability and adds the token's store-free identity read.
- **OAuth 2.0 reference tokens vs JWT access tokens** name exactly this fork: a
  reference token is an opaque handle resolved at the server (revocable, a lookup);
  a JWT is self-contained (fast, not revocable). Introspection endpoints are the
  registry check by another name. The hybrid here is a reference-token's revocation
  with a JWT's readable claims in one cookie.
- **The JWT-revocation literature** (Auth0, OWASP) converges on "allowlist/denylist
  of jti in a fast store" once revocation is required — i.e. that stateless JWT
  alone is insufficient for session control, which is the premise here.

## Unresolved questions

- **Token wire format** — a tight kernway encoding, or JWT-compatible JSON for
  interop with existing tooling? Leaning kernway-tight for the cookie, with JWT as a
  possible alternate codec.
- ~~Where request-scoped `SecurityContext` lives~~ — **decided**: it is a
  *request-scoped DI bean*, set by the auth middleware and `#[inject]`-able in a
  handler and readable by the template `Env`. The mechanism (a per-request DI scope
  in `di-core`) is its own concern and its own decision —
  [KEP-0005](0005-request-scoped-beans.md). The CSRF token rides the same scope.
- **Sweep cadence** — lazy eviction handles a session that is revisited; a periodic
  sweep reaps ones that are abandoned. How often the sweep runs (and whether it is a
  background task or piggybacks on `login`) is an implementation tuning left open.
- **Key rotation window** — accepting a previous key for a grace period so a rotation
  does not log everyone out at once.

## Future possibilities

- **Redis / SQL `SessionStore`, selected by a Cargo feature.** The backend is a
  compile-time choice: **default is in-memory** (sync — the store has no I/O, so
  blocking is instant and async would be pure overhead); a `redis` feature brings the
  I/O backend, and *that* is where the session path turns **async**, because a
  Redis/SQL `get` must not block a core (KEP-0000 §4). Sync-vs-async is a type-level
  property in Rust, so "switch to async" is the feature flipping the code path, not a
  runtime toggle — the app enables `redis` and the flow is async from there. Because
  Kernway is async-only (unlike Spring, which needs a *sync* `SessionRepository`
  **and** a *reactive* one for its two paradigms), the async side is **one** trait
  (`BoxFuture`-returning, like `Layer`), not two hierarchies. The async backend lands
  with the async-handler work (KEP-0002); until a real Redis backend exists, only the
  default in-memory sync path is built — no async machinery for a backend that is not
  there yet (KEP-0000 §1, YAGNI).
- **"My sessions" UI** — `sessions_of(user)` already returns the per-device rows,
  so a "manage your logins / sign out this device" page is a small addition.
- **Sliding expiry and refresh tokens** — re-issue near expiry, `last_seen` updates
  on the record, idle-timeout by pruning stale records.
- **Absolute per-user session caps** — "max 5 devices" by evicting the oldest row on
  a sixth login, trivial once the registry is per-device.
- **Anomaly signals** — the record's `meta` (ip/device) enables "new device"
  detection and step-up auth later.
