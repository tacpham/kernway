#!/usr/bin/env bash
# Deterministic quality gate for the core crates.
#
# Checks the three things the project promises of core code, each with the tool
# that can actually prove it:
#
#   solid    cargo test + clippy (pedantic) + miri (UB in hand-written unsafe)
#   generic  crate dependency rules from docs/internal/ARCHITECTURE.md
#   fast     criterion benches, compared against the committed baseline
#
# Advisory by design: it reports, it never blocks. Exit code is always 0 unless
# the script itself is misused, so a Stop hook cannot wedge a session.
#
# Usage:
#   scripts/check-core.sh              # only if core changed since HEAD
#   scripts/check-core.sh --all        # run regardless of what changed
#   scripts/check-core.sh --json       # emit a Claude Code hook JSON object
#   scripts/check-core.sh --with-miri  # include miri (slow: minutes)
#   scripts/check-core.sh --with-bench # include benches (slow)

set -uo pipefail
cd "$(dirname "$0")/.." || exit 0

# Crates whose API and speed are load-bearing for everything else.
CORE_CRATES=(kernway-core di-core rt-core rt-net kernway-http kernway-orm-core kernway-cache-core)

ONLY_IF_CHANGED=1
AS_JSON=0
WITH_MIRI=0
WITH_BENCH=0
for arg in "$@"; do
  case "$arg" in
    --all)        ONLY_IF_CHANGED=0 ;;
    --json)       AS_JSON=1 ;;
    --with-miri)  WITH_MIRI=1 ;;
    --with-bench) WITH_BENCH=1 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

# --- Which core crates changed? ------------------------------------------
# Compares the working tree against HEAD, including untracked files, so work
# in progress is covered rather than only what has been committed.
changed_files() {
  { git diff --name-only HEAD 2>/dev/null
    git ls-files --others --exclude-standard 2>/dev/null
  } | sort -u
}

TOUCHED=()
for crate in "${CORE_CRATES[@]}"; do
  if changed_files | grep -q "^crates/${crate}/"; then
    TOUCHED+=("$crate")
  fi
done

if [[ $ONLY_IF_CHANGED -eq 1 && ${#TOUCHED[@]} -eq 0 ]]; then
  exit 0   # nothing to say
fi
[[ ${#TOUCHED[@]} -eq 0 ]] && TOUCHED=("${CORE_CRATES[@]}")

PKG_ARGS=()
for crate in "${TOUCHED[@]}"; do PKG_ARGS+=(-p "$crate"); done

FINDINGS=()
note() { FINDINGS+=("$1"); }

# --- solid: tests --------------------------------------------------------
if ! OUT=$(cargo test "${PKG_ARGS[@]}" 2>&1); then
  note "FAIL tests — $(grep -cE '^test .* FAILED' <<<"$OUT") failing in: ${TOUCHED[*]}"
fi

# --- solid: clippy, stricter than the workspace default ------------------
# Core is a published API surface, so it is held to pedantic. Two carve-outs:
# module_name_repetitions fights the crate's own naming, and missing_errors_doc
# is noise until the v1.0 documentation pass.
CLIPPY_LINTS=(-W clippy::pedantic
              -A clippy::module_name_repetitions
              -A clippy::missing_errors_doc
              -A clippy::missing_panics_doc
              -A clippy::must_use_candidate)
if ! OUT=$(cargo clippy "${PKG_ARGS[@]}" --all-targets -- "${CLIPPY_LINTS[@]}" 2>&1); then
  note "FAIL clippy could not run — see: cargo clippy ${PKG_ARGS[*]}"
else
  COUNT=$(grep -cE '^warning: ' <<<"$OUT")
  if [[ $COUNT -gt 0 ]]; then
    # Name the top lints, not just a count — "110 warnings" is ignorable noise,
    # "83 of them are one lint" is a decision.
    TOP=$(grep -oE 'clippy::[a-z_]+' <<<"$OUT" | sort | uniq -c | sort -rn | head -3 \
          | awk '{printf "%s×%s ", $2, $1}')
    note "clippy::pedantic — $COUNT warning(s); top: ${TOP:-n/a}"
  fi
fi

# --- generic: the crate-independence rules -------------------------------
# From docs/internal/ARCHITECTURE.md. These are the edges that, once added, are
# very hard to remove — so they are checked mechanically rather than by review.
forbidden_dep() { # crate, dependency, why
  if grep -qE "^\s*$2\s*=" "crates/$1/Cargo.toml" 2>/dev/null; then
    note "DEP  $1 must not depend on $2 — $3"
  fi
}
forbidden_dep di-core     kernway-core "DI would leak HTTP types across the boundary"
forbidden_dep rt-core     kernway-core "a runtime must not know about HTTP"
forbidden_dep rt-net      kernway-core "a TCP layer must not know about HTTP"
forbidden_dep kernway-http rt-core     "the codec must stay runtime-agnostic"
forbidden_dep kernway-http rt-net      "the codec must stay runtime-agnostic"
forbidden_dep kernway-orm-core   kernway-core "data access must not import web types"
forbidden_dep kernway-cache-core kernway-core "the cache spec must not import web types"

# kernway-core is spec-only: traits and plain data, no third-party machinery.
if grep -qE '^\s*(serde|serde_json|rusqlite|mio|libc)\s*=' crates/kernway-core/Cargo.toml 2>/dev/null; then
  note "DEP  kernway-core is spec-only — an implementation dependency crept in"
fi

# --- solid: unsafe must be justified -------------------------------------
# rt-core/rt-net are the only crates allowed unsafe. Every `unsafe` block needs
# a SAFETY note; an unexplained one is exactly what miri will not catch for you.
for crate in rt-core rt-net; do
  [[ " ${TOUCHED[*]} " == *" $crate "* ]] || continue
  BLOCKS=$(grep -rn "unsafe " "crates/$crate/src" --include=*.rs | grep -vcE "unsafe_op_in_unsafe_fn|unsafe_code" || true)
  NOTES=$(grep -rc "SAFETY:" "crates/$crate/src"/*.rs "crates/$crate/src"/**/*.rs 2>/dev/null | awk -F: '{s+=$2} END {print s+0}')
  if [[ $BLOCKS -gt 0 && $NOTES -eq 0 ]]; then
    note "UNSAFE $crate has unsafe with no SAFETY: comments"
  fi
done

# --- solid: miri, the only thing that finds UB in the waker vtable -------
if [[ $WITH_MIRI -eq 1 ]]; then
  if cargo +nightly miri --version >/dev/null 2>&1; then
    if ! OUT=$(cargo +nightly miri test -p rt-core 2>&1); then
      note "FAIL miri reported undefined behaviour in rt-core — this is never acceptable"
    fi
  else
    note "SKIP miri not installed (rustup toolchain install nightly --component miri)"
  fi
fi

# --- fast: benches against the committed baseline ------------------------
if [[ $WITH_BENCH -eq 1 ]]; then
  if [[ -d crates/rt-core/benches ]]; then
    if ! OUT=$(cargo bench -p rt-core 2>&1); then
      note "FAIL benches did not run"
    else
      REGRESSED=$(grep -c "Performance has regressed" <<<"$OUT")
      [[ $REGRESSED -gt 0 ]] && note "PERF $REGRESSED benchmark(s) regressed vs the saved baseline"
    fi
  else
    note "SKIP no benches yet — 'fast' is unverified without them"
  fi
fi

# --- report --------------------------------------------------------------
if [[ ${#FINDINGS[@]} -eq 0 ]]; then
  SUMMARY="core checks clean (${TOUCHED[*]})"
else
  SUMMARY="core checks on ${TOUCHED[*]}:"$'\n'"$(printf '  • %s\n' "${FINDINGS[@]}")"
fi

if [[ $AS_JSON -eq 1 ]]; then
  # Advisory only: no `decision`, no `continue:false`. The session is never
  # blocked by this script — a wrong gate that stops work is worse than a
  # missed warning.
  python3 -c 'import json,sys; print(json.dumps({"systemMessage": sys.stdin.read().rstrip()}))' <<<"$SUMMARY"
else
  echo "$SUMMARY"
fi
exit 0
