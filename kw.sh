#!/usr/bin/env bash
# kw.sh — Kernway build tool (macOS / Linux)
# No Rust install needed. Just Docker.
#
# The shell twin of kw.ps1 (Windows), with two deliberate differences that make
# "test in Docker" actually fast and clean on a Mac:
#
#   1. target/ lives in a NAMED VOLUME, not the host bind-mount. A macOS bind
#      mount is slow for the thousands of small writes a build makes, and mixing
#      Linux artifacts into the same target/ the host's native `cargo` uses
#      corrupts both. A separate volume keeps the container build incremental
#      *and* isolated from the host — the difference between a clean 2-minute
#      re-test and a 15-minute from-scratch one.
#   2. The cargo registry AND git db are cached, so dependencies download once.
#
# Usage:
#   ./kw.sh test            — run the whole suite in Linux
#   ./kw.sh check           — fast type-check
#   ./kw.sh clippy          — lint (warnings are errors)
#   ./kw.sh fmt             — format
#   ./kw.sh bench [name]    — run a bench (all, or one by --bench name)
#   ./kw.sh run <example>   — run an example, port 8080 published
#   ./kw.sh shell           — bash inside the container
#   ./kw.sh clean-cache     — drop the cached volumes (registry + target)
# No `set -u`: macOS ships bash 3.2, where expanding an empty array under
# `nounset` (`"${arr[@]}"`) is itself an "unbound variable" error. -e and
# pipefail still catch real failures.
set -eo pipefail

# Latest stable 1.x. Must be >= 1.85: the dep tree (zeroize 1.9, via ring/rustls)
# needs edition2024, which older Cargo rejects. The `1` tag tracks the newest 1.x
# so this stays current; pin a specific one with KW_IMAGE if you need repeatability.
# (examples/web-docker/Dockerfile still pins 1.83 — bump it too before it builds
# the jwks deps.)
IMAGE="${KW_IMAGE:-rust:1-bookworm}"

# Persisted between runs. `clean-cache` removes them.
REGISTRY_VOL="kernway-cargo-registry"
TARGET_VOL="kernway-target"

# All cores by default; override with KW_CPUS for a quieter or fairer run.
CPUS="${KW_CPUS:-}"

# Shared `docker run` wrapper. --init reaps zombies and forwards Ctrl-C.
kw_docker() {
    local cpus_arg=()
    [[ -n "$CPUS" ]] && cpus_arg=(--cpus "$CPUS")
    # Corporate-proxy CA: opt-in ONLY, via KW_CA_CERT=/path/to/ca.pem. We do not
    # auto-grab $SSL_CERT_FILE — on macOS that is routinely set to a Python/certifi
    # bundle whose path Docker Desktop has not shared, and mounting it fails the
    # whole run (exit 125). The rust image ships its own CA store; the default
    # path needs no cert at all.
    local ca_arg=()
    if [[ -n "${KW_CA_CERT:-}" && -f "${KW_CA_CERT:-}" ]]; then
        ca_arg=(-v "${KW_CA_CERT}:/tmp/corp-ca.pem:ro"
                -e SSL_CERT_FILE=/tmp/corp-ca.pem
                -e CARGO_HTTP_CAINFO=/tmp/corp-ca.pem)
    fi
    docker run --rm --init "${cpus_arg[@]}" "${ca_arg[@]}" \
        -v "${PWD}:/workspace" \
        -v "${REGISTRY_VOL}:/usr/local/cargo/registry" \
        -v "${TARGET_VOL}:/workspace/target" \
        -e CARGO_TERM_COLOR=always \
        -w /workspace \
        "$@"
}

# cargo-in-container.
kw_cargo() {
    kw_docker "$IMAGE" cargo "$@"
}

# clippy/fmt need components the base image ships only as a shim; add them first.
# Only used by the lint/format commands, so test/check/build stay lean.
kw_cargo_tooled() {
    kw_docker "$IMAGE" bash -c 'rustup component add clippy rustfmt >/dev/null 2>&1 || true; exec cargo "$@"' _ "$@"
}

cmd="${1:-help}"
arg="${2:-}"

case "$cmd" in
    build)   echo ">> cargo build --workspace";                     kw_cargo build --workspace ;;
    release) echo ">> cargo build --workspace --release";           kw_cargo build --workspace --release ;;
    test)    echo ">> cargo test --workspace --all-features";       kw_cargo test --workspace --all-features ;;
    check)   echo ">> cargo check --workspace --all-features";      kw_cargo check --workspace --all-features ;;
    clippy)  echo ">> cargo clippy --workspace --all-targets";      kw_cargo_tooled clippy --workspace --all-targets -- -D warnings ;;
    fmt)     echo ">> cargo fmt --all";                             kw_cargo_tooled fmt --all ;;

    bench)
        if [[ -n "$arg" ]]; then
            echo ">> cargo bench --bench $arg"
            kw_cargo bench --bench "$arg"
        else
            echo ">> cargo bench --workspace"
            kw_cargo bench --workspace
        fi
        ;;

    run)
        [[ -z "$arg" ]] && { echo "Usage: ./kw.sh run <example>"; exit 1; }
        echo ">> cargo run -p $arg  (port 8080 published)"
        kw_docker -p 8080:8080 "$IMAGE" cargo run -p "$arg"
        ;;

    shell)
        echo ">> bash (Ctrl-D to exit)"
        kw_docker -it "$IMAGE" bash
        ;;

    clean-cache)
        echo ">> removing cached volumes: $REGISTRY_VOL, $TARGET_VOL"
        docker volume rm "$REGISTRY_VOL" "$TARGET_VOL" 2>/dev/null || true
        ;;

    *)
        cat <<'EOF'

  Kernway Build Tool — Docker Linux, no local Rust needed (macOS / Linux)

    ./kw.sh test           Run the whole suite (--all-features)
    ./kw.sh check          Fast type-check
    ./kw.sh clippy         Lint (warnings are errors)
    ./kw.sh fmt            Format
    ./kw.sh bench [name]   Run all benches, or one by --bench name
    ./kw.sh build          Build workspace
    ./kw.sh release        Build release
    ./kw.sh run <example>  Run an example (port 8080 published)
    ./kw.sh shell          Open a bash container
    ./kw.sh clean-cache    Drop cached volumes (registry + target)

  Env: KW_IMAGE (rust image), KW_CPUS (limit cores), SSL_CERT_FILE (corp CA)

EOF
        ;;
esac
