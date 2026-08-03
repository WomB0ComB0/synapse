# syntax=docker/dockerfile:1

# ---- Build stage -----------------------------------------------------------
FROM rust:1.94.1-bookworm AS builder
WORKDIR /app

# Pre-cache dependencies: copy manifests and build a dummy bin+lib first so that
# `cargo build` only recompiles crates when Cargo.toml/Cargo.lock change.
COPY Cargo.toml ./
COPY Cargo.lock ./
RUN mkdir -p src \
    && echo 'fn main() {}' > src/main.rs \
    && echo '' > src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

# Build the real sources.
COPY . .
RUN cargo build --release --locked --bin synapse

# ---- Runtime stage ---------------------------------------------------------
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -r -u 10001 synapse
WORKDIR /app
COPY --from=builder /app/target/release/synapse /usr/local/bin/synapse

ENV BIND_ADDR=0.0.0.0:8080
EXPOSE 8080
USER synapse
ENTRYPOINT ["/usr/local/bin/synapse"]
