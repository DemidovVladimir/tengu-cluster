# ---- Builder Stage ----
FROM rust:1.86-bookworm AS builder

WORKDIR /usr/src/tengu

# Copy manifests first for layer caching
COPY Cargo.toml Cargo.lock ./
COPY crates/tengu-core/Cargo.toml crates/tengu-core/Cargo.toml
COPY crates/tengu-backends/Cargo.toml crates/tengu-backends/Cargo.toml
COPY crates/tengu-channels/Cargo.toml crates/tengu-channels/Cargo.toml
COPY crates/tengu-optimizer/Cargo.toml crates/tengu-optimizer/Cargo.toml

# Create stub lib.rs for dependency caching
RUN mkdir -p src crates/tengu-core/src crates/tengu-backends/src crates/tengu-channels/src crates/tengu-optimizer/src \
    && echo "fn main() {}" > src/main.rs \
    && echo "pub fn stub() {}" > crates/tengu-core/src/lib.rs \
    && echo "pub fn stub() {}" > crates/tengu-backends/src/lib.rs \
    && echo "pub fn stub() {}" > crates/tengu-channels/src/lib.rs \
    && echo "pub fn stub() {}" > crates/tengu-optimizer/src/lib.rs

# Pre-build dependencies (cached unless Cargo.toml changes)
ARG FEATURES="ollama,anthropic,openai,openrouter,claude-code,telegram"
RUN cargo build --release --features "${FEATURES}" 2>/dev/null || true

# Copy real source
COPY src src
COPY crates crates

# Touch source files to invalidate the stub build
RUN touch src/main.rs crates/tengu-core/src/lib.rs crates/tengu-backends/src/lib.rs \
    crates/tengu-channels/src/lib.rs crates/tengu-optimizer/src/lib.rs

# Build the real binary
RUN cargo build --release --features "${FEATURES}"

# ---- Runtime Stage ----
FROM debian:bookworm-slim AS runtime

LABEL org.opencontainers.image.title="Tengu Cluster" \
      org.opencontainers.image.description="Multi-agent AI orchestrator" \
      org.opencontainers.image.source="https://github.com/user/tengu-cluster"

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /usr/src/tengu/target/release/tengu-cluster /usr/local/bin/tengu

# Copy skills directory (API skill definitions)
COPY skills /opt/tengu/skills

# Default working directory
WORKDIR /opt/tengu

# Persistent data directory
RUN mkdir -p /opt/tengu/data

# Default config path (mount your own config at runtime)
ENV TENGU_CONFIG=/opt/tengu/config.toml \
    TENGU_HOME=/opt/tengu/data

EXPOSE 7070

HEALTHCHECK --interval=60s --timeout=10s --retries=3 --start-period=10s \
    CMD ["tengu", "doctor"]

ENTRYPOINT ["tengu"]
CMD ["telegram"]
