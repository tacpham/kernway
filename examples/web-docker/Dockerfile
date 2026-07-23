# Kernway M1 — a web app in a container.
#
# Build from the workspace root, because web-docker depends on sibling crates by
# path:
#   docker build -f examples/web-docker/Dockerfile -t kernway-web-docker .
#   docker run -p 8080:8080 kernway-web-docker
#
# Two stages: a full Rust toolchain to compile, and a distroless runtime that
# carries the binary and nothing else — no shell, no package manager, ~20 MB of
# base rather than ~250 MB for a JRE-style image.

# ---- build ----------------------------------------------------------------
FROM rust:1.83-slim-bookworm AS builder
WORKDIR /app

# Copy the whole workspace. .dockerignore keeps target/ and the git history out,
# so this is source only. A dependency-caching split (copy manifests, build deps,
# then copy source) is a later optimisation — correctness first.
COPY . .

RUN cargo build --release -p web-docker

# ---- runtime --------------------------------------------------------------
# distroless/cc carries the C runtime the binary is dynamically linked against
# (glibc, libgcc). A fully static musl build could use scratch instead — that is
# the M6 decision, and it comes with an allocator question (musl's malloc is slow
# under thread-per-core), so it is deliberately not taken here.
FROM gcr.io/distroless/cc-debian12
WORKDIR /app

COPY --from=builder /app/target/release/web-docker /app/web-docker
# The static files are read from disk at runtime in this slice, so they ship
# beside the binary. Embedding them into the binary (one artifact, no path to get
# wrong) is M6.
COPY --from=builder /app/examples/web-docker/public /app/public

EXPOSE 8080
# $PORT is honoured by the app; 8080 is the default the EXPOSE documents.
ENTRYPOINT ["/app/web-docker"]
