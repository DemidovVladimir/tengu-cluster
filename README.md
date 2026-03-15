# Tengu Cluster

Model-agnostic AI agent fleet runtime in Rust. Single binary, zero dependencies.

## What It Does

- **Single-agent chat** — talk to any AI model from your terminal with persistent history
- **Multi-agent fleet** — run specialized agents (QA, backend, integration) as a coordinated team
- **Any model, one key** — use [OpenRouter](https://openrouter.ai) to access Claude, GPT, Gemini, Llama, Mistral, DeepSeek and hundreds more behind one API key
- **Custom skills** — define tools as markdown files, agents execute them during conversation
- **Telegram channel** — chat with your agent from your phone, with inline keyboard approval for dangerous tools
- **Persistent memory** — cross-session vector memory with automatic recall, per-workspace isolation, metadata tagging, and orchestrator topic overviews (disk or Qdrant)
- **Token budget gates** — per-flow token limits with 80% warning threshold and hard cutoff

## Quickstart

### Docker (recommended)

```bash
git clone https://github.com/user/tengu-cluster.git && cd tengu-cluster
make setup          # creates .env and config.toml from templates
nano .env           # set OPENROUTER_API_KEY (or other API keys)
make up             # start tengu
```

### Native

```bash
cargo build
mkdir -p ~/.tengu
cp config.example.toml ~/.tengu/config.toml
cargo run -- secret init                              # prompts for master password
cargo run -- secret set OPENROUTER_API_KEY sk-or-...  # prompts for master password
cargo run -- chat
```

### One-liner (cloud/VPS)

```bash
curl -fsSL https://raw.githubusercontent.com/user/tengu-cluster/main/deploy/install.sh | bash
```

The default config uses `nvidia/nemotron-3-super-120b-a12b:free` via OpenRouter (free tier). Change the model with one line:

```toml
[agents.main]
default = true
engine = "openrouter"
model = "google/gemini-2.5-flash"    # or anthropic/claude-sonnet-4, openai/gpt-4o, etc.
```

See the [Quickstart Guide](docs/QUICKSTART.md) for the full walkthrough.

## Supported Backends

| Engine | API Key | Model Format |
|--------|---------|-------------|
| **OpenRouter** (recommended) | `OPENROUTER_API_KEY` | `provider/model` (e.g. `nvidia/nemotron-3-super-120b-a12b:free`) |
| **Anthropic** | `ANTHROPIC_API_KEY` | `claude-sonnet-4-20250514` |
| **OpenAI** | `OPENAI_API_KEY` | `gpt-4o` |
| **Ollama** | (none, local) | `llama3.2` |
| **Hugging Face** | `HF_TOKEN` | `org/model:variant` |
| **Claude Code** | (subprocess) | `claude-sonnet-4-5-20250929` |

## Commands

```bash
cargo run -- chat             # Interactive chat (default)
cargo run -- telegram         # Telegram bot
cargo run -- orchestrate      # Multi-agent fleet
cargo run -- status           # Show config summary
cargo run -- doctor           # Check backend connectivity
cargo run -- secret init      # Create encrypted secrets vault
cargo run -- secret set K V   # Store a secret
cargo run -- secret list      # List stored secret keys
cargo run -- secret remove K  # Remove a secret
cargo run -- prune            # Remove all cached/ephemeral state
cargo run -- prune --sandbox desci  # Also clean workspace state
```

### Chat Commands

| Command | Description |
|---------|-------------|
| `/help` | List commands |
| `/cost` | Token usage |
| `/context` | Context window info |
| `/engine` | Current engine details |
| `/eco` / `/standard` / `/precise` | Switch lens mode |
| `/reset` | Clear conversation |
| `/purge` | Clear conversation + wipe persistent memory + clean workspace artifacts |
| `/reload` | Re-read env vars + re-scan skills |
| `/skills` | List discovered skills |
| `/enable N` / `/disable N` | Enable/disable a skill |
| `/team <goal>` | Explicitly plan & execute goal across multiple agents (Telegram) |
| `/project <name>` | Create new project subfolder in workspace (Telegram) |
| `/agents` | List available agents and roles (Telegram) |
| `/wallet` | Show Privy wallet address and balance (Telegram) |
| `/wallet status` | Show wallet ID, address, balance, and policy (Telegram) |

## Built-In Workspace Primitives

When an agent has `workspace` configured, four built-in primitives are available automatically:

| Primitive | Risk | Approval | Description |
|-----------|------|----------|-------------|
| `read_file` | Low | No | Read file contents (text and PDF) |
| `list_directory` | Low | No | List files and directories |
| `write_file` | Medium | Yes | Write content to file |
| `run_command` | High | Yes | Execute shell command in workspace |

These are the stable foundation — all skills and external tools interact with the workspace through these primitives. When memory is enabled, the memory subsystem registers its own `remember` tool automatically. The `remember` tool accepts optional `metadata` key-value tags (e.g., `kind`, `topic`) for structured recall.

Tools marked "Yes" for approval require user confirmation before execution — via dialog in TUI mode, or inline keyboard buttons in Telegram mode. Approval dialogs are generated generically from tool metadata (risk level, description), not hardcoded per tool name.

## Multi-Agent Fleet

Run specialized agents as a coordinated team. Roles are fully dynamic — any string works:

```toml
[orchestrator]
enabled = true

[agents.qa]
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"
role = "qa"
capabilities = ["workspace.read", "workspace.list", "workspace.shell"]
skill_packages = ["search", "test_runner"]

[agents.qa.identity]
name = "QA Agent"
instructions = "You review code, run tests, and verify correctness."

[agents.backend]
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"
role = "backend_engineer"
capabilities = ["workspace.read", "workspace.list", "workspace.write", "workspace.shell"]

[agents.backend.identity]
name = "Backend Engineer"
instructions = "You write server code, design APIs, and manage databases."
```

```bash
cargo run -- orchestrate

# Or use a sandbox config for domain-specific teams:
cargo run -- orchestrate --sandbox webstudio
cargo run -- telegram --sandbox webstudio
cargo run -- telegram --sandbox desci
```

Tasks flow through: **Pending -> InProgress -> Completed** (with automatic retry on failure). Use `capabilities` for hard runtime permissions and `skill_packages` for workflow-specific skill context.

In Telegram multi-agent mode, plain messages are orchestrated across the team automatically, while `@role: message` forces a specific agent. `/team <goal>` remains available as an explicit planning command. Independent tasks run in parallel batches; dependent tasks receive prior step output embedded inline in their prompt (no file-path indirection). After each multi-agent run, the orchestrator auto-summarizes results into a topic overview stored in memory. Before planning new goals, prior topic overviews are recalled via RAG and injected into the planner context. Use `/project <name>` to create isolated project subfolders within the workspace without restarting.

See the [Fleet Orchestration Guide](docs/FLEET.md), [Sandboxes Guide](docs/SANDBOXES.md) for full setup.

## Custom Skills

Define tools as `skills/*.md` files. Two formats are supported:

**Classic skills** — shell command tools with parameters:

```markdown
# test_runner

Run the project test suite.

## Parameters
- `filter` (string, optional): test name filter

## Execution
```bash
cargo test {{filter}} 2>&1
```
```

**API skills** — YAML frontmatter wrapping API docs, auto-generates a curl-based tool:

```markdown
---
name: my-api
description: My external API
homepage: https://api.example.com
---

# API Documentation

Full API reference here — injected into the agent's system prompt.
```

Agents call these tools during conversation. Load them per agent with `skill_packages = ["tool1", "tool2"]` and grant execution with matching `capabilities`.

See the [Skills Guide](docs/SKILLS.md) for the full format and examples.

## On-Chain Signing

DeSci minting uses `EVM_PRIVATE_KEY` for direct on-chain signing via alloy — the full minting pipeline (reservation, metadata upload, terms signing, mint transaction) runs natively inside `src/adapters/desci_tools.rs`. Other workflows can use **Privy agentic wallets** for policy-based guardrails.

## Deployment

Deploy anywhere with Docker Compose. GPU acceleration is supported via Ollama.

```bash
make up              # API backends only (OpenRouter, Anthropic, etc.)
make up-gpu          # + Ollama with NVIDIA GPU (CUDA)
make up-full         # + Ollama GPU + Qdrant vector memory
make up-cpu          # + Ollama (CPU only)
make down            # stop everything
make logs            # tail logs
make doctor          # run diagnostics
```

**GPU support:**
- **NVIDIA CUDA** — `docker compose --profile ollama-gpu up -d` passes GPU to Ollama via `nvidia-container-toolkit`
- **Apple Metal** — install Ollama natively (`brew install ollama`), set `OLLAMA_HOST=http://host.docker.internal:11434`

**Cloud provisioning** — use `deploy/cloud-init.yml` with Hetzner, AWS, DigitalOcean, or any cloud-init provider:

```bash
hcloud server create --name tengu --type cx22 --image ubuntu-24.04 \
  --user-data-from-file deploy/cloud-init.yml --ssh-key my-key
```

See the [Deployment Guide](docs/DEPLOYMENT.md) for full setup, GPU configuration, cloud provisioning, and production checklist.

## Token Budget Gates

Each conversation flow has a configurable token limit (`max_tokens_per_flow`, default: 100,000). The system enforces two thresholds:

- **80% warning**: a notice is sent to the user showing current usage and remaining budget
- **100% hard limit**: further requests are blocked with a message to use `/reset`

```toml
[agents.main.limits]
max_tokens_per_flow = 100_000
```

## Documentation

| Guide | What It Covers |
|-------|---------------|
| [Quickstart](docs/QUICKSTART.md) | Installation, first config, first chat, next steps |
| [Deployment](docs/DEPLOYMENT.md) | Docker, Docker Compose, GPU (CUDA/Metal), cloud provisioning, production checklist |
| [Configuration Reference](docs/CONFIGURATION.md) | Every config field, env var, default value, and validation rule |
| [Skills Guide](docs/SKILLS.md) | Skill file format, parameters, execution, policy, per-agent filtering |
| [Fleet Orchestration](docs/FLEET.md) | Multi-agent setup, roles, task lifecycle, parallel execution |
| [Sandboxes](docs/SANDBOXES.md) | Domain-specific multi-agent teams, per-agent tool restrictions |
| [Wallet & Signing](docs/WALLET.md) | Privy agentic wallets, policy setup, on-chain transaction signing |
| [DeSci Guide](docs/GUIDE_DESCI.md) | End-to-end IP-NFT minting with aura-orchestrator and Privy wallets |
| [Architecture](ARCHITECTURE.md) | Hexagonal architecture rules and project structure |

## Feature Flags

| Flag | Default | Purpose |
|------|---------|---------|
| `openrouter` | on | OpenRouter unified API |
| `anthropic` | on | Anthropic/Claude |
| `openai` | on | OpenAI |
| `ollama` | on | Ollama local models |
| `claude-code` | on | Claude Code subprocess |
| `huggingface` | off | Hugging Face Inference Providers |
| `telegram` | on | Telegram bot channel |
| `qdrant` | off | Qdrant vector store for RAG memory |

```bash
# Build with Qdrant vector store
cargo build --features qdrant

# Build with all features
cargo build --all-features
```

## Architecture

This project uses **hexagonal architecture** as a **mandatory** requirement. See [ARCHITECTURE.md](ARCHITECTURE.md) for the full rules.

- `src/domain/` — pure business rules, no I/O
- `src/application/` — use-case orchestration via ports
- `src/adapters/` — infrastructure implementations

## Development

```bash
cargo fmt --all
cargo test --workspace
```

Architecture guardrails are enforced by `tests/hex_architecture_enforcement.rs`.
