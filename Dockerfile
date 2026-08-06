# syntax=docker/dockerfile:1
FROM lukemathwalker/cargo-chef:latest-rust-1.89 AS chef
WORKDIR /app

FROM chef AS planner
COPY Cargo.* ./
COPY crates crates
RUN rm -rf crates/audio crates/client crates/desktop crates/protocol crates/ui crates/video
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
# Cook dependencies — this is the cached Docker layer
RUN cargo chef cook --release --recipe-path recipe.json
# Build the binary
COPY Cargo.* ./
COPY crates crates
RUN rm -rf crates/audio crates/client crates/desktop crates/protocol crates/ui crates/video
RUN cargo build --release --bin server && \
  strip target/release/server

# ── Runtime ──────────────────────────────────────────────────────
FROM debian:trixie-slim

RUN useradd --uid 10000 --create-home runner
WORKDIR /app

COPY --from=builder /app/target/release/server .
RUN chown -R runner:runner /app

EXPOSE 5000/udp

USER 10000

ENTRYPOINT ["./server"]
