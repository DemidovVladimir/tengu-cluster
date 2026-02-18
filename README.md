# Tengu Cluster

Fast, low-cost Rust agent hub for business workflows across chat channels.

## What It Is

Tengu Cluster is a single Rust application that routes messages to AI models and keeps strict control over:
- token usage
- storage/retrieval behavior
- operational cost

Current working baseline:
- CLI chat runtime
- Ollama backend
- in-memory knowledge retrieval with budget-capped query API (`query_with_budget`)
- official-first dependency policy for providers/channels
- Candle as planned local acceleration path (CUDA/Metal when available, CPU fallback)

## Why Use It

- Lower running cost through explicit token budgeting
- Predictable behavior for long-running business chats
- Rust-first architecture for speed and deploy simplicity
- Clear path to durable flows, compaction, and retrieval guard rails

## How To Use

### 1. Build

```bash
cargo build
```

### 2. Configure

Copy and edit config:

```bash
mkdir -p ~/.tengu
cp config.example.toml ~/.tengu/config.toml
```

Export environment variables as needed:

```bash
cp .env.example .env
# then export variables from .env using your shell tool of choice
# (or export directly, e.g. `export OLLAMA_HOST=http://localhost:11434`)
```

Environment variable details:
- `.env.example`

### 3. Run

```bash
cargo run -- chat
```

Useful commands inside chat:
- `/help`
- `/cost`
- `/context`
- `/eco`, `/standard`, `/precise`
- `/reset`

## Development Checks

Run local quality checks:

```bash
scripts/check_rust_file_descriptions.sh
cargo fmt --all
cargo test --workspace
```

Enable the pre-commit hook:

```bash
git config core.hooksPath .githooks
```

## Key Docs

- `PRD.md`
- `ARCHITECTURE.md`
- `ROADMAP_MVP.md`
- `STORAGE_RETRIEVAL_GAP_ANALYSIS.md`
- `DEPENDENCY_POLICY.md`
- `RUST_PATTERNS_PLAYBOOK.md`
