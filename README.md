# Tengu Cluster

**A composable multi-agent system built to run 24/7** — model-agnostic, pluggable, and designed for autonomous orchestration with permanent memory.

Single Rust binary. Zero external dependencies. Hexagonal architecture.

## Vision

Tengu Cluster is a **multi-agent runtime** where:

- **Agents** coordinate through an orchestrator, communicate results inline, and maintain permanent memory across sessions
- **Skills** are fully plug-and-play — drop a `SKILL.md` file into `skills/` and it's immediately available, no code changes required
- **Tools** are general-purpose primitives (read, write, list, execute) reusable across any skill — sandbox policy controls which agents can use which tools
- **Channels** (TUI, Telegram, and any future channel) are isolated behind port traits — adding a new channel never touches business logic

The system follows **hexagonal architecture** (mandatory), **DRY**, and **KISS** principles, with idiomatic Rust throughout. See [Architecture](tengu/Architecture.md) for the full rules.

## What It Does

- **Multi-agent orchestration** — specialized agents (any role) coordinate as a team, with LLM-based task planning, dependency resolution, and parallel batch execution via `JoinSet`
- **24/7 autonomous operation** — orchestrator decomposes goals, routes to agents, passes results inline between dependent tasks, and auto-summarizes outcomes into permanent memory
- **Plug-and-play skills** — define new capabilities as markdown files, no code changes — agents discover and execute them automatically
- **Permanent memory** — cross-session vector memory (disk or Qdrant) with metadata tagging, per-workspace isolation, filtered recall, and orchestrator topic overviews for continuity
- **Any model, one key** — use [OpenRouter](https://openrouter.ai) to access Claude, GPT, Gemini, Llama, Mistral, DeepSeek and hundreds more behind one API key
- **Channel-ready** — TUI and Telegram today, architected for Slack, Discord, API, or any channel via port traits
- **Composable and understandable** — clear separation of agents, tools, skills, and capabilities makes the system easy to extend and reason about
- **Token budget gates** — per-flow token limits with 80% warning threshold and hard cutoff

## Concepts: Agents, Tools, Skills, Capabilities

Understanding these four concepts is key to working with Tengu:

| Concept | What It Is | How It's Added | Example |
|---------|-----------|----------------|---------|
| **Agent** | An LLM-backed actor with a role, identity, and set of permissions | Config (`agents.<id>` in TOML) | `qa`, `backend_engineer`, `onchain_minter` |
| **Tool** | A general-purpose primitive that performs an action | Code (4 built-in workspace primitives) or subsystem (memory) | `read_file`, `write_file`, `run_command`, `remember` |
| **Skill** | Domain knowledge + instructions packaged as a markdown file | Plug-and-play (`skills/name/SKILL.md`) — no code changes | `beach-science`, `privy-agentic-wallets` |
| **Capability** | A permission that controls which tools an agent can use | Config (`capabilities` + `allowed_tools` per agent) | `workspace.read`, `workspace.shell` |

**How they connect:**
- An **agent** is assigned **capabilities** in its config, which gate access to **tools**
- **Skills** provide domain-specific instructions and generate tool definitions that agents can call — skills use the existing **tools** (primitives) to interact with the world
- **Sandbox policy** (`sandboxes/<name>/config.toml`) restricts which agents see which tools and skills, without any code changes
- Adding a new skill = drop a file. Adding a new agent = add config. No recompilation needed.

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

See the [Quickstart Guide](tengu/Quickstart.md) for the full walkthrough.

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

Tasks flow through: **Pending -> InProgress -> Completed/Failed**. The orchestrator is fail-fast by default: tool failures surface immediately instead of being retried automatically. Use `capabilities` for hard runtime permissions and `skill_packages` for workflow-specific skill context.

In Telegram multi-agent mode, plain messages are orchestrated across the team automatically, while `@role: message` forces a specific agent. `/team <goal>` remains available as an explicit planning command. Independent tasks run in parallel batches; dependent tasks receive prior step output embedded inline in their prompt (no file-path indirection). After each multi-agent run, the orchestrator auto-summarizes results into a topic overview stored in memory. Before planning new goals, prior topic overviews are recalled via RAG and injected into the planner context. Use `/project <name>` to create isolated project subfolders within the workspace without restarting.

See the [Fleet Orchestration Guide](tengu/Orchestrator.md), [Sandboxes Guide](tengu/Sandboxes.md) for full setup.

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

**API skills** — YAML frontmatter wrapping API docs. These inject workflow/reference context into the prompt; agents execute them with generic tools such as `http_request`, `sign_message`, or `sign_and_send_transaction`:

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

See the [Skills Guide](tengu/Skills.md) for the full format and examples.

## On-Chain Signing

All on-chain signing uses **Privy agentic wallets** — server-side wallets with policy-based guardrails. The full DeSci minting pipeline (reservation, metadata upload, terms signing, mint transaction) runs natively inside `src/adapters/desci_tools.rs`. Alloy is used for ABI encoding only.

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

See the [Deployment Guide](tengu/Deployment.md) for full setup, GPU configuration, cloud provisioning, and production checklist.

## Token Budget Gates

Each conversation flow has a configurable token limit (`max_tokens_per_flow`, default: 100,000). The system enforces two thresholds:

- **80% warning**: a notice is sent to the user showing current usage and remaining budget
- **100% hard limit**: further requests are blocked with a message to use `/reset`

```toml
[agents.main.limits]
max_tokens_per_flow = 100_000
```

## Documentation

All documentation lives in the `tengu/` Obsidian vault:

| Guide | What It Covers |
|-------|---------------|
| [Overview](tengu/Overview.md) | System concept map, all guides index |
| [Quickstart](tengu/Quickstart.md) | Installation, first config, first chat, next steps |
| [Deployment](tengu/Deployment.md) | Docker, Docker Compose, GPU (CUDA/Metal), cloud provisioning, production checklist |
| [Configuration](tengu/Configuration.md) | Config overview, memory, Telegram, secrets vault |
| [Configuration Reference](tengu/Configuration%20Reference.md) | Every config field, env var, default value, and validation rule |
| [Skills](tengu/Skills.md) | Skill file format, parameters, execution, policy, per-agent filtering |
| [Orchestrator](tengu/Orchestrator.md) | Multi-agent setup, roles, task lifecycle, parallel execution |
| [Sandboxes](tengu/Sandboxes.md) | Domain-specific multi-agent teams, per-agent tool restrictions |
| [Wallet](tengu/Wallet.md) | Privy agentic wallets, policy setup, on-chain transaction signing |
| [DeSci](tengu/DeSci.md) | End-to-end IP-NFT minting with generic HTTP/crypto tools plus plug-and-play skills |
| [Architecture](tengu/Architecture.md) | Hexagonal architecture rules and project structure |
| [Memory](tengu/Memory.md) | Three-layer memory architecture, RAG, research comparison |

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

This project uses **hexagonal architecture** as a **mandatory** requirement. See [Architecture](tengu/Architecture.md) for the full rules.

- `src/domain/` — pure business rules, no I/O
- `src/application/` — use-case orchestration via ports
- `src/adapters/` — infrastructure implementations

## Development Principles

- **Hexagonal architecture** — mandatory. Domain has no I/O, application depends on ports, adapters implement ports. See [Architecture](tengu/Architecture.md).
- **DRY** — no duplicated logic. Shared channel runtime, shared tool UI, shared memory init.
- **KISS** — simplest solution that works. No premature abstractions, no feature flags for hypothetical futures.
- **Idiomatic Rust** — `Send + Sync` bounds for async concurrency, `Arc<T>` for shared state, trait objects for polymorphism, feature gates for optional dependencies. Follow clippy lints.
- **Every task includes tests** — architecture enforcement tests, integration tests for new adapters, unit tests for domain logic.
- **Docs stay current** — every feature change updates the corresponding documentation.

```bash
cargo fmt --all
cargo test --workspace
```

Architecture guardrails are enforced by `tests/hex_architecture_enforcement.rs`.
