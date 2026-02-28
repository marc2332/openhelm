# ── Stage 1: Chef base ────────────────────────────────────────────────────────
FROM rust:1.93.1-bookworm AS chef
RUN cargo install cargo-chef
WORKDIR /build

# ── Stage 2: Plan dependencies ───────────────────────────────────────────────
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ── Stage 3: Build ───────────────────────────────────────────────────────────
FROM chef AS builder

# Build dependencies (cached until Cargo.toml/Cargo.lock change)
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Build the real binary
COPY . .
RUN cargo build --release

# ── Stage 4: Runtime ─────────────────────────────────────────────────────────
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/openhelm /usr/local/bin/openhelm

# Pre-create the data directory for audit logs
RUN mkdir -p /root/.local/share/openhelm

# Config file is expected at /root/openhelm.toml -- mount it at runtime:
#   docker run -v ./openhelm.toml:/root/openhelm.toml:ro ...

ENTRYPOINT ["openhelm"]
CMD ["start"]
