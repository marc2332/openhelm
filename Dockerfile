# ── Stage 1: Build ────────────────────────────────────────────────────────────
FROM rust:1.88-bookworm AS builder

WORKDIR /build

# Copy manifests first for better layer caching
COPY Cargo.toml Cargo.lock ./
COPY opencontrol/Cargo.toml opencontrol/Cargo.toml
COPY opencontrol-sdk/Cargo.toml opencontrol-sdk/Cargo.toml
COPY opencontrol-github/Cargo.toml opencontrol-github/Cargo.toml
COPY opencontrol-http/Cargo.toml opencontrol-http/Cargo.toml

# Create dummy source files so cargo can resolve the dependency graph and
# cache the (slow) dependency build in its own layer.
RUN mkdir -p opencontrol/src opencontrol-sdk/src opencontrol-github/src opencontrol-http/src \
    && echo "fn main() {}" > opencontrol/src/main.rs \
    && echo "" > opencontrol-sdk/src/lib.rs \
    && echo "" > opencontrol-github/src/lib.rs \
    && echo "" > opencontrol-http/src/lib.rs \
    && cargo build --release 2>/dev/null || true \
    && rm -rf opencontrol/src opencontrol-sdk/src opencontrol-github/src opencontrol-http/src

# Copy actual source code
COPY opencontrol/ opencontrol/
COPY opencontrol-sdk/ opencontrol-sdk/
COPY opencontrol-github/ opencontrol-github/
COPY opencontrol-http/ opencontrol-http/

# Build the real binary (touch sources so cargo recompiles all workspace crates)
RUN touch opencontrol/src/main.rs \
    opencontrol-sdk/src/lib.rs \
    opencontrol-github/src/lib.rs \
    opencontrol-http/src/lib.rs \
    && cargo build --release

# ── Stage 2: Runtime ──────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/opencontrol /usr/local/bin/opencontrol

# Pre-create the data directory for audit logs
RUN mkdir -p /root/.local/share/opencontrol

# Config file is expected at /root/opencontrol.toml -- mount it at runtime:
#   docker run -v ./opencontrol.toml:/root/opencontrol.toml:ro ...

ENTRYPOINT ["opencontrol"]
CMD ["start"]
