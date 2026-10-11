# Keep in sync with rust-toolchain.toml. The chef image ships rustup,
# so copying rust-toolchain.toml below makes a mismatch self-correct
# (rustup fetches the pinned version) instead of silently building
# with whatever the tag says.
ARG RUST_VERSION=1.95
FROM lukemathwalker/cargo-chef:latest-rust-${RUST_VERSION} AS chef
WORKDIR /app

# ── Plan: dependency recipe from the manifests only ─────────────────────
# The whole workspace is copied so Cargo.lock resolves exactly as it
# does on a developer machine (that is what lets --locked hold below);
# only the server's dependency graph is ever *compiled*, so ffmpeg,
# pipewire and slint never enter this image.
FROM chef AS planner
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates crates
COPY third_party third_party
RUN cargo chef prepare --recipe-path recipe.json

# ── Build ───────────────────────────────────────────────────────────────
FROM chef AS builder
COPY rust-toolchain.toml ./
# Cargo resolves every [patch] path in the workspace, so the vendored
# crates must be present even though the server doesn't build them.
COPY third_party third_party
COPY --from=planner /app/recipe.json recipe.json
# Dependencies: the cached layer. `deploy` = release + stripped symbols
# (see the workspace profiles), the same profile the desktop ships with.
RUN cargo chef cook --profile deploy --package server --bin server --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY crates crates
# --locked: Cargo.lock is authoritative; a manifest/lock drift fails the
# build here rather than resolving something new inside the container.
RUN cargo build --profile deploy --locked --package server --bin server

# ── Runtime ─────────────────────────────────────────────────────────────
# Distroless: glibc + CA certs, no shell, no package manager. The server
# is pure Rust (rustls, no OpenSSL) so `cc` is all it needs. The builder
# is Debian bookworm (glibc 2.36) and so is this base — no glibc drift.
# uid 10000 matches setup.sh's chown of ./data/baucord.
FROM gcr.io/distroless/cc-debian12
# /app is owned by the runtime uid (no shell here, so ownership comes
# from --chown; WORKDIR would create it root-owned, hence it follows
# the COPY). With `logging.file: ""` in the container config nothing
# is written there anyway; this keeps the default config working too.
COPY --from=builder --chown=10000:10000 /app/target/deploy/server /app/server
WORKDIR /app
USER 10000
EXPOSE 51000/udp
# Docker sends SIGTERM to PID 1; the server closes every session with a
# reason so clients reconnect in ~2 s instead of waiting out the idle
# timeout. The close frames leave first (up to 0.5 s), then the database
# queue is written, for at most 6 s: Docker's default 10 s
# `stop_grace_period` covers it.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/app/server"]
