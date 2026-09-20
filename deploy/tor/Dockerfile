# Tor egress proxy for tengu: Arti (the Tor Project's Rust client) with
# lyrebird-rs as its managed pluggable transport (obfs4 + snowflake bridges).
#
#   tengu ─► arti :9050 (SOCKS5 + HTTP CONNECT) ─► lyrebird-rs ─► bridge ─► Tor
#
# lyrebird-rs is a separate project (../lyrebird-rs next to this repo, or
# LYREBIRD_RS_SRC — a directory or a git URL) passed in as the named build
# context `lyrebird-rs`; see deploy/tor/compose.yml. Run via `make tor`.
ARG ARTI_VERSION=2.6.0

# ---- Arti ---------------------------------------------------------------
FROM rust:1.91-bookworm AS arti
ARG ARTI_VERSION
RUN apt-get update \
 && apt-get install -y --no-install-recommends libssl-dev libsqlite3-dev pkg-config \
 && rm -rf /var/lib/apt/lists/*
# --locked: exactly the dependency set the Tor Project released.
# http-connect: HTTP CONNECT on the SOCKS port for clients without SOCKS
# support (the Claude Code CLI, teloxide).
RUN cargo install arti --version "${ARTI_VERSION}" --locked --features http-connect \
 && strip /usr/local/cargo/bin/arti

# ---- lyrebird-rs (drop-in `lyrebird` binary) ----------------------------
FROM rust:1.91-bookworm AS lyrebird
WORKDIR /src
COPY --from=lyrebird-rs Cargo.toml Cargo.lock ./
COPY --from=lyrebird-rs src ./src
RUN cargo build --release --locked --bin lyrebird \
 && strip target/release/lyrebird

# ---- runtime ------------------------------------------------------------
FROM debian:bookworm-slim
# ca-certificates: snowflake reaches its broker over domain-fronted HTTPS.
# curl: healthcheck asks check.torproject.org through the SOCKS port.
RUN apt-get update \
 && apt-get install -y --no-install-recommends libssl3 libsqlite3-0 ca-certificates curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home-dir /var/lib/arti --create-home --shell /usr/sbin/nologin arti \
 && chmod 700 /var/lib/arti
COPY --from=arti /usr/local/cargo/bin/arti /usr/local/bin/arti
COPY --from=lyrebird /src/target/release/lyrebird /usr/local/bin/lyrebird
COPY arti.toml /etc/arti/arti.toml
USER arti
VOLUME ["/var/lib/arti"]
EXPOSE 9050
# Healthy only once traffic really exits via Tor (bootstrap through a bridge
# takes ~30 s for obfs4, minutes for snowflake).
HEALTHCHECK --interval=15s --timeout=25s --start-period=60s --retries=40 \
    CMD curl -fsS --max-time 20 --socks5-hostname 127.0.0.1:9050 \
        https://check.torproject.org/api/ip | grep -q '"IsTor":true'
CMD ["arti", "proxy", "-c", "/etc/arti/arti.toml"]
