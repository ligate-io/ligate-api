# Multi-stage Dockerfile for `ligate-api`.
#
# Stage 1: build the workspace's `ligate-api` binary in a Rust
# toolchain image with the Sov-SDK build deps available.
# Stage 2: slim runtime with the binary + ca-certificates only.
#
# Targets `linux/amd64` + `linux/arm64` (Railway provisions both).

FROM rust:1.93-bookworm AS builder

# Build deps: librocksdb (transitively via the Sovereign SDK), clang
# for clang-sys, pkg-config + libssl for reqwest's rustls native fall-
# backs (we use rustls directly, but some build scripts still poke
# for libssl headers).
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        clang libclang-dev libssl-dev pkg-config protobuf-compiler && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache deps separately from sources. Copy manifests first; cargo's
# incremental build skips dep recompilation on source-only edits.
COPY Cargo.toml Cargo.lock* rust-toolchain.toml constants.toml ./
COPY crates/api/Cargo.toml ./crates/api/
COPY crates/drip/Cargo.toml ./crates/drip/
COPY crates/indexer/Cargo.toml ./crates/indexer/
COPY crates/types/Cargo.toml ./crates/types/
RUN mkdir -p crates/api/src crates/drip/src crates/indexer/src crates/types/src && \
    echo "fn main() {}" > crates/api/src/main.rs && \
    echo "" > crates/drip/src/lib.rs && \
    echo "" > crates/indexer/src/lib.rs && \
    echo "" > crates/types/src/lib.rs && \
    SKIP_GUEST_BUILD=1 RISC0_SKIP_BUILD_KERNELS=1 \
    CONSTANTS_MANIFEST_PATH=/build/constants.toml \
    cargo build --release --bin ligate-api && \
    rm -rf crates/*/src

COPY crates ./crates
COPY migrations ./migrations
RUN SKIP_GUEST_BUILD=1 RISC0_SKIP_BUILD_KERNELS=1 \
    CONSTANTS_MANIFEST_PATH=/build/constants.toml \
    cargo build --release --bin ligate-api && \
    strip target/release/ligate-api

# Stage 2: minimal runtime — glibc + ca-certificates + the binary.
FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*

# Unprivileged user; matches the most common host-side UID mapping.
ARG UID=1000
RUN useradd --system --uid ${UID} --shell /usr/sbin/nologin --create-home api
USER api
WORKDIR /home/api

COPY --from=builder --chown=api:api /build/target/release/ligate-api /usr/local/bin/ligate-api
COPY --from=builder --chown=api:api /build/migrations /home/api/migrations

# HTTP server port. Override at runtime with `API_BIND=0.0.0.0:PORT`.
# Railway sets `PORT` automatically; we read `API_BIND` so the start
# command can be `API_BIND=0.0.0.0:${PORT}`.
EXPOSE 8080

# `DRIP_SIGNER_KEY` and `DATABASE_URL` MUST be injected at runtime
# via env / Railway secrets, never baked into the image.
ENTRYPOINT ["/usr/local/bin/ligate-api"]
