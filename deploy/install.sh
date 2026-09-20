#!/usr/bin/env bash
# Tengu Cluster — One-liner installer
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/DemidovVladimir/tengu-cluster/main/deploy/install.sh | bash
#
# Options (env vars):
#   TENGU_DIR        Install directory       (default: ./tengu-cluster)
#   TENGU_NETWORK    tor (default) | open    tor = tengu runs behind the Arti + lyrebird-rs
#                    proxy (docker-compose.tor.yml); open = plain internet, and
#                    config.toml gets `[egress] network = "open"`
#   LYREBIRD_RS_SRC  lyrebird-rs checkout    (default: <TENGU_DIR>/../lyrebird-rs, cloned from
#                    LYREBIRD_RS_REPO when missing; only needed for TENGU_NETWORK=tor)
#   LYREBIRD_RS_REPO Git URL of lyrebird-rs  (default: https://github.com/DemidovVladimir/lyrebird-rs.git)
#   TENGU_PROFILE    Compose profile         (default: none — OpenRouter + Telegram only)
#                    Options: postgres-memory (Postgres + pgvector agentic memory;
#                    sets TENGU_FEATURES=openrouter,telegram,postgres_memory so the
#                    image is built with the feature)
#   TENGU_BRANCH     Git branch to clone     (default: main)
#   SKIP_DOCKER      Skip Docker install     (default: false)
#   SKIP_START       Skip starting services  (default: false)

set -euo pipefail

# ── Colors ──────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

info()  { echo -e "${CYAN}[tengu]${NC} $*"; }
ok()    { echo -e "${GREEN}[tengu]${NC} $*"; }
warn()  { echo -e "${YELLOW}[tengu]${NC} $*"; }
err()   { echo -e "${RED}[tengu]${NC} $*" >&2; }

# ── Defaults ────────────────────────────────────────────────────
TENGU_DIR="${TENGU_DIR:-./tengu-cluster}"
TENGU_BRANCH="${TENGU_BRANCH:-main}"
TENGU_PROFILE="${TENGU_PROFILE:-}"
TENGU_NETWORK="${TENGU_NETWORK:-tor}"
LYREBIRD_RS_REPO="${LYREBIRD_RS_REPO:-https://github.com/DemidovVladimir/lyrebird-rs.git}"
SKIP_DOCKER="${SKIP_DOCKER:-false}"
SKIP_START="${SKIP_START:-false}"

# ── Detect OS ───────────────────────────────────────────────────
detect_os() {
    case "$(uname -s)" in
        Linux*)   OS=linux ;;
        Darwin*)  OS=macos ;;
        *)        err "Unsupported OS: $(uname -s)"; exit 1 ;;
    esac

    ARCH="$(uname -m)"
    case "$ARCH" in
        x86_64|amd64) ARCH=amd64 ;;
        aarch64|arm64) ARCH=arm64 ;;
        *) err "Unsupported architecture: $ARCH"; exit 1 ;;
    esac

    info "Detected: $OS/$ARCH"
}

# ── Install Docker ──────────────────────────────────────────────
install_docker() {
    if command -v docker &>/dev/null; then
        ok "Docker already installed: $(docker --version)"
        return
    fi

    if [ "$SKIP_DOCKER" = "true" ]; then
        warn "Docker not found but SKIP_DOCKER=true — skipping"
        return
    fi

    info "Installing Docker..."

    if [ "$OS" = "macos" ]; then
        if command -v brew &>/dev/null; then
            brew install --cask docker
            ok "Docker Desktop installed — please start it from Applications"
            warn "Waiting for Docker to start..."
            while ! docker info &>/dev/null 2>&1; do
                sleep 2
            done
        else
            err "Please install Docker Desktop from https://docker.com/products/docker-desktop"
            exit 1
        fi
    elif [ "$OS" = "linux" ]; then
        curl -fsSL https://get.docker.com | sh
        sudo systemctl enable --now docker
        sudo usermod -aG docker "$USER" 2>/dev/null || true
        ok "Docker installed. You may need to log out and back in for group changes."
    fi
}

# ── Clone / Update Repo ────────────────────────────────────────
setup_repo() {
    if [ -d "$TENGU_DIR/.git" ]; then
        info "Updating existing installation..."
        git -C "$TENGU_DIR" fetch origin
        git -C "$TENGU_DIR" checkout "$TENGU_BRANCH"
        git -C "$TENGU_DIR" pull origin "$TENGU_BRANCH"
    else
        info "Cloning tengu-cluster..."
        git clone --branch "$TENGU_BRANCH" --depth 1 \
            https://github.com/DemidovVladimir/tengu-cluster.git "$TENGU_DIR" 2>/dev/null || {
            warn "Could not clone repo — using local copy"
            if [ ! -d "$TENGU_DIR" ]; then
                err "Directory $TENGU_DIR does not exist and clone failed"
                exit 1
            fi
        }
    fi
    cd "$TENGU_DIR"
}

# ── lyrebird-rs (Tor pluggable transports, built into deploy/tor) ──
setup_lyrebird() {
    if [ "$TENGU_NETWORK" != "tor" ]; then
        return
    fi
    LYREBIRD_RS_SRC="${LYREBIRD_RS_SRC:-$(cd .. && pwd)/lyrebird-rs}"
    case "$LYREBIRD_RS_SRC" in
        http://*|https://*|git@*) ok "lyrebird-rs from $LYREBIRD_RS_SRC" ;;
        *)
            if [ ! -f "$LYREBIRD_RS_SRC/Cargo.toml" ]; then
                info "Cloning lyrebird-rs into $LYREBIRD_RS_SRC..."
                git clone --depth 1 "$LYREBIRD_RS_REPO" "$LYREBIRD_RS_SRC"
            fi
            ok "lyrebird-rs at $LYREBIRD_RS_SRC"
            ;;
    esac
    export LYREBIRD_RS_SRC
}

# ── Configure ───────────────────────────────────────────────────
configure() {
    if [ ! -f .env ]; then
        cp .env.example .env
        info "Created .env from template"
    fi

    if [ ! -f config.toml ]; then
        cp config.example.toml config.toml
        info "Created config.toml from template"
        if [ "$TENGU_NETWORK" = "open" ]; then
            printf '\n[egress]\nnetwork = "open"\n' >> config.toml
            info "config.toml: [egress] network = \"open\" (TENGU_NETWORK=open)"
        fi
    fi

    ok "Configuration ready"
    echo ""
    warn "Edit these files before starting:"
    echo "  .env         — API keys (OPENROUTER_API_KEY, TELEGRAM_BOT_TOKEN, etc.)"
    echo "  config.toml  — Agent config, models, memory settings"
    echo ""
}

# ── Start ───────────────────────────────────────────────────────
start_services() {
    if [ "$SKIP_START" = "true" ]; then
        info "Skipping auto-start (SKIP_START=true)"
        return
    fi

    info "Building tengu image..."
    if [ "$TENGU_PROFILE" = "postgres-memory" ]; then
        export TENGU_FEATURES="${TENGU_FEATURES:-openrouter,telegram,postgres_memory}"
    fi
    if [ -n "$TENGU_PROFILE" ]; then
        make up-memory NETWORK="$TENGU_NETWORK"
    else
        make up NETWORK="$TENGU_NETWORK"
    fi

    ok "Tengu is running!"
    echo ""
    make status NETWORK="$TENGU_NETWORK"
}

# ── Summary ─────────────────────────────────────────────────────
summary() {
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    ok "Tengu Cluster installed successfully"
    echo ""
    echo "  Directory:  $(pwd)"
    echo "  Network:    $TENGU_NETWORK (make targets take NETWORK=tor|open)"
    echo "  Profile:    ${TENGU_PROFILE:-default (OpenRouter + Telegram)}"
    echo ""
    echo "  Commands:"
    echo "    make up           Start tengu (over Tor; NETWORK=open for direct)"
    echo "    make up-memory    Start tengu + Postgres/pgvector agentic memory"
    echo "    make logs         View logs"
    echo "    make doctor       Run diagnostics (+ live Tor-exit check)"
    echo "    make down         Stop all"
    echo ""
    echo "  Manual:"
    echo "    docker compose exec tengu tengu chat"
    echo "    docker compose exec tengu tengu telegram"
    echo "    docker compose exec tengu tengu doctor"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
}

# ── Main ────────────────────────────────────────────────────────
main() {
    echo ""
    echo "  ╔══════════════════════════════════╗"
    echo "  ║   Tengu Cluster — Installer      ║"
    echo "  ╚══════════════════════════════════╝"
    echo ""

    detect_os
    install_docker
    setup_repo
    setup_lyrebird
    configure
    start_services
    summary
}

main "$@"
