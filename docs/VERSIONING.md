# Kernway — Rust versioning & MSRV policy

How Kernway tracks the Rust language/toolchain. Unlike Spring/Java: with Rust we
**update by value, not by obligation**.

---

## 1. Why Rust differs from Java/Spring

| | Java / Spring | Rust / Kernway |
|---|---|---|
| Backward compatibility | at the bytecode level; idioms shift constantly | **Stability guarantee since 1.0** — code that compiles keeps compiling on stable, forever |
| Language upgrades | a JDK bump can change behavior | **Editions** (2015/18/21/24) — *opt-in*; crates of different editions interoperate |
| Upgrade pressure | must follow new JVM capabilities (e.g. virtual threads → Spring 6.1) | no "JVM" underneath; std is stable |

→ Spring chases new versions because the **platform forces it**. Rust has no such
pressure, so **"if it's good, you don't need to update"** is largely true.

---

## 2. Policy

1. **Update when there is VALUE.** Adopt a new language/std feature only when it:
   - removes `unsafe`/a hack, **or**
   - cuts code (e.g. we adopted `u64::div_ceil` instead of a hand-rolled ceiling division), **or**
   - gives free performance, **or**
   - improves safety.
   Never update just because a new release exists.
2. **Stable only. Never nightly.**
3. **Fixed MSRV, enforced by CI.** See §3.
4. **Editions: migrate when the benefit justifies it.** `cargo fix --edition` automates most of it. Kernway is currently on **edition 2021**; move to 2024 when the ergonomic/safety wins are worth it, not urgently.
5. **A specific advantage:** Kernway's *zero async runtime, pure std* stance means it **sidesteps both the async-in-traits axis and the churn of the tokio ecosystem** — the biggest source of updates for other Rust frameworks. This is a design decision with long-term stability value.

---

## 3. MSRV (Minimum Supported Rust Version)

- **Current MSRV: `1.78`** — declared in `[workspace.package] rust-version` in `Cargo.toml`.
- Chosen as 1.78 because: the newest stdlib feature in use is `u64::div_ceil` (stable since 1.73); 1.78 gives a safe margin and matches the CI toolchain.
- **A conservative MSRV is a framework strength** — don't chase the newest compiler, because that *forces users* to upgrade and hurts adoption.

### When to raise the MSRV
Only when a *worthwhile* feature (§2) requires a higher version. When you raise it:
1. Update `rust-version` in `Cargo.toml`.
2. Update the badge in `README.md`.
3. Update the pinned version in the CI `msrv` job.
4. Note the reason (which feature required it) in the PR.

### CI job that checks the MSRV (snippet)
> ⚠️ `.github/workflows/ci.yml` is not currently in the repo. When CI is restored,
> add the job below to **enforce the MSRV floor independently** of the dev
> toolchain. Keep the version here = the declared MSRV; the other jobs
> (fmt/clippy/test) may run a newer stable.

```yaml
  msrv:
    name: MSRV (1.78)
    runs-on: ubuntu-latest
    container: rust:1.78-slim-bookworm     # MUST match rust-version in Cargo.toml
    steps:
      - uses: actions/checkout@v4
      - run: cargo check --workspace --all-targets
```

Check locally (if you have rustup):
```bash
rustup toolchain install 1.78
cargo +1.78 check --workspace --all-targets
```

---

## 4. One-line summary
Rust gives us the stability that Spring emulates with machinery; Kernway leans
into that — **keep a conservative MSRV, adopt new features only when they truly pay off.**
