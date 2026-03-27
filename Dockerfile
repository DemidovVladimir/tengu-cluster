# ---- Builder Stage ----
FROM rust:1.86-bookworm AS builder

WORKDIR /usr/src/tengu

# Copy manifests first for layer caching
COPY Cargo.toml Cargo.lock ./

# Create stub main.rs for dependency caching
RUN mkdir -p src && echo "fn main() {}" > src/main.rs

# Pre-build dependencies (cached unless Cargo.toml changes)
ARG FEATURES="openrouter,telegram"
RUN cargo build --release --features "${FEATURES}" 2>/dev/null || true

# Copy real source + skills
COPY src src
COPY skills skills

# Touch source to invalidate the stub build
RUN touch src/main.rs

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

COPY --from=builder /usr/src/tengu/target/release/tengu /usr/local/bin/tengu

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
