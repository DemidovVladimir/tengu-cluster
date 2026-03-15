# Deployment Guide

Run Tengu Cluster locally, in Docker, or on any cloud provider.

## Table of Contents

- [Quick Reference](#quick-reference)
- [Local (Native)](#local-native)
- [Docker](#docker)
- [Docker Compose](#docker-compose)
- [GPU Acceleration](#gpu-acceleration)
  - [NVIDIA CUDA](#nvidia-cuda)
  - [Apple Metal](#apple-metal)
- [Cloud Deployment](#cloud-deployment)
  - [One-Liner Install](#one-liner-install)
  - [Hetzner Cloud](#hetzner-cloud)
  - [Any VPS (Cloud-Init)](#any-vps-cloud-init)
- [Makefile Reference](#makefile-reference)
- [Profiles](#profiles)
- [Volumes and Data](#volumes-and-data)
- [Production Checklist](#production-checklist)

---

## Quick Reference

```bash
# Local native build
cargo build && cargo run -- chat

# Docker (API backends only)
make setup && make up

# Docker + local Ollama with GPU
make setup && make up-gpu

# Full stack (Ollama GPU + Qdrant vector memory)
make setup && make up-full

# Cloud VPS (one-liner)
curl -fsSL https://raw.githubusercontent.com/user/tengu-cluster/main/deploy/install.sh | bash
```

---

## Local (Native)

Build and run directly on your machine. Best for development and macOS with Metal GPU.

```bash
# Build
cargo build

# Configure
mkdir -p ~/.tengu
cp config.example.toml ~/.tengu/config.toml

# Set API key
cargo run -- secret init
cargo run -- secret set OPENROUTER_API_KEY sk-or-...

# Run
cargo run -- chat
```

For Qdrant vector memory support:

```bash
cargo build --features qdrant
```

See the [Quickstart Guide](QUICKSTART.md) for the full walkthrough.

---

## Docker

Build and run the tengu binary in a container. The image is ~100MB (debian:bookworm-slim base).

```bash
# Build
docker build -t tengu-cluster .

# Run (mount config, pass env)
docker run -d \
  --name tengu \
  -v ~/.tengu/config.toml:/opt/tengu/config.toml:ro \
  -v tengu-data:/opt/tengu/data \
  --env-file .env \
  -p 7070:7070 \
  tengu-cluster telegram
```

### Build Arguments

| Arg | Default | Description |
|-----|---------|-------------|
| `FEATURES` | `ollama,anthropic,openai,openrouter,claude-code,telegram` | Cargo feature flags to compile in |

```bash
# Build with Qdrant support
docker build --build-arg FEATURES="ollama,anthropic,openai,openrouter,claude-code,telegram,qdrant" -t tengu-cluster .
```

### Health Check

The image includes a built-in health check that runs `tengu doctor` every 60 seconds.

---

## Docker Compose

The recommended way to run Tengu with supporting services (Ollama, Qdrant).

### Setup

```bash
# Create config files from templates
make setup
# This creates .env and config.toml if they don't exist

# Edit API keys
nano .env

# Edit agent configuration
nano config.toml
```

### Start

```bash
# API backends only (OpenRouter, Anthropic, OpenAI)
docker compose up -d

# With Ollama for local inference (CPU)
docker compose --profile ollama up -d

# With Ollama + NVIDIA GPU
docker compose --profile ollama-gpu up -d

# With Qdrant vector memory
docker compose --profile qdrant up -d

# Full stack: Ollama GPU + Qdrant
docker compose --profile full up -d

# Full stack: Ollama CPU + Qdrant
docker compose --profile full-cpu up -d
```

Or use the Makefile shortcuts: `make up`, `make up-gpu`, `make up-full`, etc.

### Override Config

The compose file reads these environment variables from `.env`:

| Variable | Default | Description |
|----------|---------|-------------|
| `TENGU_FEATURES` | `ollama,anthropic,openai,openrouter,claude-code,telegram` | Build features |
| `TENGU_PORT` | `7070` | Host port for Tengu hub |
| `OLLAMA_PORT` | `11434` | Host port for Ollama |
| `QDRANT_HTTP_PORT` | `6333` | Host port for Qdrant HTTP |
| `QDRANT_GRPC_PORT` | `6334` | Host port for Qdrant gRPC |
| `NVIDIA_VISIBLE_DEVICES` | `all` | Which NVIDIA GPUs to expose |
| `RUST_LOG` | `info` | Log level |

### Interact

```bash
# Tail logs
docker compose logs -f tengu

# Run diagnostics
docker compose exec tengu tengu doctor

# Interactive chat inside the container
docker compose exec -it tengu tengu chat

# Check service status
docker compose ps
```

---

## GPU Acceleration

Tengu itself calls external APIs or local Ollama for inference — it doesn't run models directly. GPU acceleration works by giving Ollama access to your GPU for faster local model inference.

### NVIDIA CUDA

**Requirements:** NVIDIA GPU + [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/install-guide.html)

```bash
# Install NVIDIA Container Toolkit (Ubuntu/Debian)
curl -fsSL https://nvidia.github.io/libnvidia-container/gpgkey | \
  sudo gpg --dearmor -o /usr/share/keyrings/nvidia-container-toolkit-keyring.gpg
curl -s -L "https://nvidia.github.io/libnvidia-container/stable/deb/nvidia-container-toolkit.list" | \
  sed 's#deb https://#deb [signed-by=/usr/share/keyrings/nvidia-container-toolkit-keyring.gpg] https://#g' | \
  sudo tee /etc/apt/sources.list.d/nvidia-container-toolkit.list > /dev/null
sudo apt-get update && sudo apt-get install -y nvidia-container-toolkit
sudo nvidia-ctk runtime configure --runtime=docker
sudo systemctl restart docker

# Start with GPU
make up-gpu
# or
docker compose --profile ollama-gpu up -d
```

Set `TENGU_GPU_HINT=cuda` in `.env` to inform Tengu that a GPU is available (affects runtime profile auto-detection).

To restrict which GPUs are visible, set `NVIDIA_VISIBLE_DEVICES=0` (or `0,1` for multi-GPU) in `.env`.

### Apple Metal

Docker cannot passthrough Apple's Metal GPU. For GPU-accelerated local inference on macOS:

1. Install Ollama natively:

```bash
brew install ollama
ollama serve
```

2. Pull a model:

```bash
ollama pull llama3.2
```

3. Connect Tengu to host Ollama. In `.env`:

```bash
OLLAMA_HOST=http://host.docker.internal:11434
TENGU_GPU_HINT=metal
```

Or if running Tengu natively (not Docker):

```bash
OLLAMA_HOST=http://localhost:11434
TENGU_GPU_HINT=metal
```

4. Configure an agent to use Ollama in `config.toml`:

```toml
[agents.local]
default = true
engine = "ollama"
model = "llama3.2"
```

Metal acceleration is automatic on Apple Silicon — Ollama uses it by default.

---

## Cloud Deployment

### One-Liner Install

Provision any Linux server with a single command:

```bash
curl -fsSL https://raw.githubusercontent.com/user/tengu-cluster/main/deploy/install.sh | bash
```

The installer:
- Detects OS, architecture, and GPU (NVIDIA CUDA / Apple Metal)
- Installs Docker and Docker Compose if missing
- Installs NVIDIA Container Toolkit if a GPU is detected
- Clones the repository
- Creates `.env` and `config.toml` from templates
- Auto-selects the best Docker Compose profile based on hardware
- Builds and starts the services

**Environment variables to customize the installer:**

| Variable | Default | Description |
|----------|---------|-------------|
| `TENGU_DIR` | `./tengu-cluster` | Installation directory |
| `TENGU_PROFILE` | auto-detected | Docker Compose profile (`ollama`, `ollama-gpu`, `qdrant`, `full`, `full-cpu`) |
| `TENGU_BRANCH` | `main` | Git branch to clone |
| `SKIP_DOCKER` | `false` | Skip Docker installation |
| `SKIP_START` | `false` | Skip starting services after install |

```bash
# Install without starting
SKIP_START=true bash deploy/install.sh

# Force GPU profile
TENGU_PROFILE=ollama-gpu bash deploy/install.sh

# Install to custom directory
TENGU_DIR=/opt/tengu bash deploy/install.sh
```

### Hetzner Cloud

Using the [Hetzner Cloud CLI](https://github.com/hetznercloud/cli):

```bash
# Standard VPS (API backends only)
hcloud server create \
  --name tengu \
  --type cx22 \
  --image ubuntu-24.04 \
  --user-data-from-file deploy/cloud-init.yml \
  --ssh-key my-key

# GPU server (with NVIDIA GPU for local inference)
hcloud server create \
  --name tengu-gpu \
  --type gx11 \
  --image ubuntu-24.04 \
  --user-data-from-file deploy/cloud-init.yml \
  --ssh-key my-key
```

**Recommended Hetzner server types:**

| Type | vCPUs | RAM | GPU | Use Case |
|------|-------|-----|-----|----------|
| `cx22` | 2 | 4 GB | — | Single agent, API backends |
| `cx32` | 4 | 8 GB | — | Multi-agent fleet, API backends |
| `cx42` | 8 | 16 GB | — | Large fleet, Qdrant memory |
| `gx11` | 8 | 30 GB | A100 40GB | Local inference with Ollama |

After provisioning, SSH in and configure:

```bash
ssh root@<ip>
cd /opt/tengu-cluster
nano .env                    # set API keys
nano config.toml             # customize agents
make up                      # start (API only)
# or
make up-gpu                  # start with Ollama GPU
```

The cloud-init script automatically:
- Installs Docker
- Detects and configures NVIDIA GPUs
- Clones the repo to `/opt/tengu-cluster`
- Pre-builds the Docker image
- Creates a systemd service for auto-start on reboot
- Configures UFW firewall (ports 22 + 7070)

### Any VPS (Cloud-Init)

The `deploy/cloud-init.yml` file works with any cloud provider that supports [cloud-init](https://cloud-init.io/):

- **DigitalOcean:** paste into "User Data" when creating a Droplet
- **AWS EC2:** pass as `--user-data` in launch configuration
- **GCP:** use `--metadata-from-file user-data=deploy/cloud-init.yml`
- **Vultr:** paste into "Cloud-Init User-Data" field
- **Linode:** paste into "User Data" in advanced options

---

## Makefile Reference

| Command | Description |
|---------|-------------|
| `make help` | Show all available commands |
| `make setup` | Create `.env` and `config.toml` from templates |
| `make build` | Build the Docker image |
| `make up` | Start Tengu (API backends only) |
| `make up-gpu` | Start Tengu + Ollama with NVIDIA GPU |
| `make up-cpu` | Start Tengu + Ollama (CPU only) |
| `make up-full` | Start Tengu + Ollama GPU + Qdrant |
| `make up-full-cpu` | Start Tengu + Ollama CPU + Qdrant |
| `make up-qdrant` | Start Tengu + Qdrant (no Ollama) |
| `make down` | Stop all services |
| `make logs` | Tail Tengu logs |
| `make status` | Show running services |
| `make doctor` | Run Tengu diagnostics |
| `make clean` | Stop all and remove volumes (destructive) |
| `make native` | Build locally with cargo (debug) |
| `make native-release` | Build locally with cargo (release) |
| `make native-qdrant` | Build locally with qdrant feature |

---

## Profiles

Docker Compose profiles control which services start alongside Tengu:

| Profile | Services | Use Case |
|---------|----------|----------|
| (none) | Tengu only | API backends (OpenRouter, Anthropic, OpenAI) |
| `ollama` | Tengu + Ollama (CPU) | Local inference without GPU |
| `ollama-gpu` | Tengu + Ollama (NVIDIA GPU) | Local inference with CUDA |
| `qdrant` | Tengu + Qdrant | Vector memory with ANN search |
| `full` | Tengu + Ollama GPU + Qdrant | Everything with GPU |
| `full-cpu` | Tengu + Ollama CPU + Qdrant | Everything without GPU |

---

## Volumes and Data

Docker Compose creates named volumes for persistent data:

| Volume | Container Path | Purpose |
|--------|---------------|---------|
| `tengu-data` | `/opt/tengu/data` | Conversation history, logs, memory store |
| `ollama-data` | `/root/.ollama` | Downloaded Ollama models |
| `qdrant-data` | `/qdrant/storage` | Qdrant vector collections |

To back up:

```bash
docker run --rm -v tengu-data:/data -v $(pwd):/backup alpine tar czf /backup/tengu-backup.tar.gz -C /data .
```

To restore:

```bash
docker run --rm -v tengu-data:/data -v $(pwd):/backup alpine tar xzf /backup/tengu-backup.tar.gz -C /data
```

---

## Production Checklist

Before running in production:

- [ ] Set API keys in `.env` or secrets vault
- [ ] Set `TENGU_MASTER_PASSWORD` for non-interactive vault decryption
- [ ] Change `hub.bind` to `0.0.0.0` if external access is needed
- [ ] Set `hub.auth_mode = "token"` and configure `hub.auth_token`
- [ ] Set `allowed_users` in `[telegram]` section (never leave empty in production)
- [ ] Review `capabilities` per agent — restrict `workspace.write` and `workspace.shell` as needed
- [ ] Set `RUST_LOG=warn` for production (reduce log volume)
- [ ] Configure firewall: allow only ports 22 (SSH) and 7070 (Tengu hub)
- [ ] Set up volume backups for `tengu-data`
- [ ] Monitor with `make doctor` or health check endpoint
- [ ] Set `max_tokens_per_flow` and `max_cost_per_flow` limits per agent
