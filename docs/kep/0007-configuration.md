---
kep: 0007
title: Configuration — layered properties, profiles, and typed access
status: Accepted
created: 2026-07-25
decided: 2026-07-25
---

# KEP-0007: Configuration

## Summary

An app needs settings that change per environment without recompiling: the bind
address, timeouts, a Redis URL, log levels. Kernway gets a **layered configuration**
loaded once at startup — a base `application.properties`, an optional
`application-{profile}.properties` for the active profile, and environment
variables, each overriding the last — read through a typed [`Config`]. The format is
Spring's `.properties` (dotted keys, `key = value`), parsed ourselves (no `toml`/
`serde`/`yaml` dependency for a not-hot path that a few lines handle). The first use
is `logging.level.<module>` driving KEP-0006-era `kernway-log`, so a deployment turns
up one module's logs from a file, not a recompile. The typed `#[configuration]`
binding onto a DI bean is specified here but built next.

[`Config`]: ../../crates/kernway-config/src/lib.rs

## Motivation

Settings are currently literals in code (`"0.0.0.0:8080"`, a signing key, the log
default). Changing one for staging vs prod means editing and rebuilding — the thing
twelve-factor config exists to avoid. Spring solved this with `application.properties`
+ profiles + env overrides, and Spring developers reach for it by reflex. Kernway
should have the same shape: a file for the defaults, a profile file for the
environment, env vars for the secrets and the per-deploy overrides — and one typed
`Config` to read them.

Concretely it unblocks `logging.level.kernway_redis=debug` in a file (KEP-0006 left
log levels to the `KW_LOG` env only), and everything after it — server settings,
session TTLs, the Redis address — stops being a literal.

## Guide-level explanation

`Config::load()` reads, lowest precedence first:

1. **`application.properties`** — the committed defaults.
2. **`application-{profile}.properties`** — for the active profile (`KW_PROFILE`,
   e.g. `prod`), if present. Overrides the base.
3. **Environment variables** prefixed `KW_`, with `__` → `.` — the per-deploy
   overrides and secrets. `KW_SERVER__PORT=9090` sets `server.port`. Highest
   precedence.

```properties
# application.properties
server.port = 8080
logging.level = info
logging.level.kernway_redis = debug
session.token-ttl-secs = 3600
```

```rust
let config = Config::load();                       // the standard layered load
let port: u16 = config.get_or("server.port", 8080);
let ttl = config.get::<u64>("session.token-ttl-secs");
// A subtree — every logging.level.* entry, for the log filter.
for (module, level) in config.with_prefix("logging.level.") { … }
```

`get::<T>` parses through `FromStr`, so `u16`/`bool`/`Duration`-via-secs/`String`
all work; `get_or` supplies a default. `with_prefix` yields the keys under a prefix
(the `logging.level.*` case), and `sub(prefix)` returns a `Config` reparented at it.

`#[configuration]` (built next) binds a section to a struct and registers it as a DI
bean, the typed twin of the loose keys:

```rust
#[configuration(prefix = "server")]
struct ServerConfig { port: u16, host: String }
// injected like any bean: fn handler(cfg: &ServerConfig) { … }
```

## Reference-level explanation

### `Config` — a flat map of dotted keys

Parsing `.properties` yields `HashMap<String, String>` keyed by the dotted path
(`logging.level.kernway_redis`). A flat map, not a tree: lookups are the common case
and dotted-key access is O(1); `with_prefix`/`sub` scan, which is fine for a
startup-time, handful-of-keys structure. Values stay strings until `get::<T>` parses
them, so the config layer has no opinion on types.

### The properties parser

`key = value` per line, `#`/`!` line comments, blank lines ignored, first `=` splits
(values may contain `=`), key and value trimmed. No multi-line, no interpolation in
the first cut — a dozen lines, and every branch is ours. Duplicate keys: last wins
(so a later source overriding an earlier one is just re-insertion).

### Precedence and profiles

`load()` inserts sources in order — base file, then profile file, then env — each
overwriting, so later wins. The profile is `KW_PROFILE` (or the
`kernway.profiles.active` property in the base file). `ConfigBuilder` exposes the
layers (`file`, `profile_file`, `env`, `set`) for a custom order or for tests, so
`load()` is just the conventional sequence.

### Environment binding

Only `KW_`-prefixed vars are read (so config never slurps unrelated env), the prefix
stripped, lowercased, and `__` → `.`. `__` (not `_`) maps to the separator because
keys themselves contain `_` (`kernway_redis`) — a single `_` would be ambiguous, the
long-standing pain of Spring's relaxed binding. `KW_LOGGING__LEVEL__KERNWAY_REDIS=trace`
→ `logging.level.kernway_redis=trace`, unambiguous.

### The logging bridge

`kernway-log` stays config-agnostic (it takes a `Filter`); the app builds the filter
from config — `logging.level` is the default, each `logging.level.<module>` an
override. Kept as a small helper at the app/meta layer, not a dependency edge from
`kernway-log` up to `kernway-config`.

### What this KEP does not build yet

- **`#[configuration]` derive + DI binding.** Specified above; the macro and the
  bean registration are the next slice. The loose `Config` API lands first.
- **YAML** — *built, behind the `yaml` feature.* `application.yml` /
  `application-{profile}.yml` are read and **flattened into the same dotted-key map**
  (a nested map → `a.b.c`, a sequence → indexed `a.0`/`a.1`), so `get`, `with_prefix`,
  `#[configuration]`, and the logging bridge are unchanged. YAML is the one format not
  hand-rolled — its spec (anchors, aliases, implicit typing) is the responsible-
  dependency case (`yaml-rust2`, a maintained pure-Rust parser; `serde_yaml` is
  archived). Off by default: the properties baseline stays dependency-free. Real
  lists still need a richer `Config` type (below); indexed keys are the interim.
- **`${...}` interpolation and `@ConfigurationProperties` relaxed binding.** Later.

## Drawbacks

- **Properties, not YAML.** Spring devs may expect YAML; nested structures are flatter
  in properties (`a.b.c=`). Acceptable — the keys here are flat, and properties is a
  Spring format too, parsed with no dependency.
- **A flat map loses structure.** No typed section until `#[configuration]` lands;
  `with_prefix` is a scan, not a tree walk. Fine at startup scale.
- **Env `__` convention is a thing to learn.** Unusual next to `RUST_LOG`, but the
  only unambiguous mapping given keys contain `_`.

## Rationale and alternatives

**TOML via the `toml` crate.** Idiomatic Rust, typed, nested. Rejected as the base
format because config parsing is not a hot path (parsed once), a dependency is not
warranted for a format a dozen lines parse, and `.properties` matches the Spring UX
this framework courts. TOML support could be added later as an alternate source.

**`serde` + a struct per config.** Strong typing up front, but couples the whole
config surface to one big deserialize and needs a format crate. The flat `Config` +
later `#[configuration]` gives typing where wanted without a monolith.

**Env-only (twelve-factor purist).** No files, everything an env var. Rejected: a
committed `application.properties` of defaults is where a reader learns what is
configurable, and profiles express environments cleanly. Env stays the top override
layer, which is the twelve-factor part that matters.

## Prior art

- **Spring Boot** — `application.properties`/`.yml`, `application-{profile}`,
  `spring.profiles.active`, env override, `@ConfigurationProperties`,
  `@Value("${...}")`. This KEP is the properties + profiles + env core of it, with
  `#[configuration]` as the `@ConfigurationProperties` twin to follow.
- **12-factor config** — config in the environment. Kernway keeps env as the top
  layer, over committed file defaults.
- **`config` / `figment` crates** — layered config for Rust. Same layering idea;
  Kernway writes the small properties core itself per KEP-0000 §1.

## Future possibilities

- `#[configuration(prefix = "…")]` binding a section to a DI bean.
- `${other.key}` and `${ENV_VAR}` interpolation in values.
- A YAML or TOML source alongside properties.
- `--server.port=9090` command-line args as the highest layer.
- Config change watch + hot reload (pairs with the M5 hot-reload work).
