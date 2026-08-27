# syntax=docker/dockerfile:1
# Multi-stage musl static build, mirroring rootle's layout.
# rust:alpine's host triple is the platform's musl native target
# (x86_64 on amd64, aarch64 on arm), so plain `cargo build` already
# produces a static musl binary — the release matrix relies on this
# for native arm builds with zero cross config.

FROM rust:alpine AS builder
RUN apk add --no-cache musl-dev \
    && rustup component add clippy rustfmt
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples

FROM builder AS test
RUN cargo fmt --check \
    && cargo clippy --locked --all-targets -- -D warnings \
    && cargo test --locked

# Stripped static release binary.
FROM builder AS release
RUN cargo build --release --locked \
    && strip target/release/rootle-gitlab \
    && ldd target/release/rootle-gitlab 2>&1 | grep -q "Not a valid dynamic program\|not a dynamic executable" \
    && echo "static: ok"
