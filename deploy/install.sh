#!/usr/bin/env bash
# Tengu Cluster — One-liner installer
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/user/tengu-cluster/main/deploy/install.sh | bash
#
# Options (env vars):
#   TENGU_DIR        Install directory       (default: ./tengu-cluster)
#   TENGU_PROFILE    Compose profile         (default: none — API only)
#                    Options: ollama, ollama-gpu, qdrant, full, full-cpu
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

# ── Detect GPU ──────────────────────────────────────────────────
detect_gpu() {
    GPU_TYPE="none"

    if [ "$OS" = "macos" ] && [ "$ARCH" = "arm64" ]; then
        GPU_TYPE="metal"
        ok "Apple Silicon detected — Metal GPU acceleration available"
        ok "For local inference, install Ollama natively: brew install ollama"
        return
    fi

    if command -v nvidia-smi &>/dev/null; then
        GPU_TYPE="cuda"
        GPU_INFO="$(nvidia-smi --query-gpu=name,memory.total --format=csv,noheader 2>/dev/null || echo 'unknown')"
        ok "NVIDIA GPU detected: $GPU_INFO"
        return
    fi

    if [ -d /dev/dri ] && ls /dev/dri/renderD* &>/dev/null 2>&1; then
        warn "GPU device found at /dev/dri but no NVIDIA driver detected"
        warn "AMD/Intel GPUs are not yet supported for Ollama acceleration"
    fi

    info "No GPU detected — will use CPU for local inference"
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

# ── Install NVIDIA Container Toolkit ────────────────────────────
install_nvidia_toolkit() {
    if [ "$GPU_TYPE" != "cuda" ]; then
        return
    fi

    if dpkg -l nvidia-container-toolkit &>/dev/null 2>&1; then
        ok "NVIDIA Container Toolkit already installed"
        return
    fi

    info "Installing NVIDIA Container Toolkit..."
    if [ "$OS" = "linux" ]; then
        distribution=$(. /etc/os-release; echo "$ID$VERSION_ID") 2>/dev/null || distribution="ubuntu22.04"
        curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey | \
            sudo gpg --dearmor -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg
        curl -s -L "https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list" | \
            sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' | \
            sudo tee /etc/apt/sources.list.d/nvidia-container-toolkit.list > /dev/null
        sudo apt-get update -qq
        sudo apt-get install -y -qq nvidia-container-toolkit
        sudo nvidia-ctk runtime configure --runtime=docker
        sudo systemctl restart docker
        ok "NVIDIA Container Toolkit installed and configured"
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
            https://github.com/user/tengu-cluster.git "$TENGU_DIR" 2>/dev/null || {
            # If the repo URL fails (placeholder), just create the directory
            warn "Could not clone repo — using local copy"
            if [ ! -d "$TENGU_DIR" ]; then
                err "Directory $TENGU_DIR does not exist and clone failed"
                exit 1
            fi
        }
    fi
    cd "$TENGU_DIR"
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
    fi

    # Auto-select profile based on GPU if not specified
    if [ -z "$TENGU_PROFILE" ]; then
        case "$GPU_TYPE" in
            cuda)
                TENGU_PROFILE="ollama-gpu"
                info "Auto-selected profile: ollama-gpu (NVIDIA GPU detected)"
                ;;
            metal)
                info "Apple Metal detected — use native Ollama for GPU inference"
                info "Run: brew install ollama && ollama serve"
                info "Then set OLLAMA_HOST=http://host.docker.internal:11434 in .env"
                ;;
        esac
    fi

    # Set GPU hint in .env
    case "$GPU_TYPE" in
        cuda)
            if ! grep -q "^TENGU_GPU_HINT=" .env 2>/dev/null; then
                echo "TENGU_GPU_HINT=cuda" >> .env
            fi
            ;;
        metal)
            if ! grep -q "^TENGU_GPU_HINT=" .env 2>/dev/null; then
                echo "TENGU_GPU_HINT=metal" >> .env
            fi
            ;;
    esac

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
    if [ -n "$TENGU_PROFILE" ]; then
        docker compose --profile "$TENGU_PROFILE" build
        docker compose --profile "$TENGU_PROFILE" up -d
    else
        docker compose build
        docker compose up -d
    fi

    ok "Tengu is running!"
    echo ""
    docker compose ps
}

# ── Summary ─────────────────────────────────────────────────────
summary() {
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    ok "Tengu Cluster installed successfully"
    echo ""
    echo "  Directory:  $(pwd)"
    echo "  Profile:    ${TENGU_PROFILE:-default (API only)}"
    echo "  GPU:        $GPU_TYPE"
    echo ""
    echo "  Commands:"
    echo "    make up           Start (API backends)"
    echo "    make up-gpu       Start with Ollama + NVIDIA GPU"
    echo "    make up-full      Start full stack (Ollama GPU + Qdrant)"
    echo "    make logs         View logs"
    echo "    make doctor       Run diagnostics"
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
    detect_gpu
    install_docker
    install_nvidia_toolkit
    setup_repo
    configure
    start_services
    summary
}

main "$@"
