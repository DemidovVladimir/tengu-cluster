# Tengu Cluster

Model-agnostic AI agent fleet runtime in Rust. Single binary, zero dependencies.

## What It Does

- **Single-agent chat** — talk to any AI model from your terminal with persistent history
- **Multi-agent fleet** — run specialized agents (QA, backend, integration) as a coordinated team
- **Any model, one key** — use [OpenRouter](https://openrouter.ai) to access Claude, GPT, Gemini, Llama, Mistral, DeepSeek and hundreds more behind one API key
- **Custom skills** — define tools as markdown files, agents execute them during conversation
- **Telegram channel** — chat with your agent from your phone, with inline keyboard approval for dangerous tools
- **Persistent memory** — cross-session vector memory with automatic recall (disk or Qdrant)
- **Token budget gates** — per-flow token limits with 80% warning threshold and hard cutoff

## Quickstart

```bash
# Build
cargo build

# Configure
mkdir -p ~/.tengu
cp config.example.toml ~/.tengu/config.toml

# Store your API key in the encrypted vault
cargo run -- secret init                              # prompts for master password
cargo run -- secret set OPENROUTER_API_KEY sk-or-...  # prompts for master password

# Chat
cargo run -- chat
```

The default config uses `anthropic/claude-sonnet-4` via OpenRouter. Change the model with one line:

```toml
[agents.main]
default = true
engine = "openrouter"
model = "openai/gpt-4o"    # or google/gemini-2.5-pro, meta-llama/llama-4-maverick, etc.
```

See the [Quickstart Guide](docs/QUICKSTART.md) for the full walkthrough.

## Supported Backends

| Engine | API Key | Model Format |
|--------|---------|-------------|
| **OpenRouter** (recommended) | `OPENROUTER_API_KEY` | `provider/model` (e.g. `anthropic/claude-sonnet-4`) |
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
| `/purge` | Clear conversation + wipe persistent memory |
| `/reload` | Re-read env vars + re-scan skills |
| `/skills` | List discovered skills |
| `/enable N` / `/disable N` | Enable/disable a skill |
| `/team <goal>` | Plan & execute goal across multiple agents (Telegram) |
| `/project <name>` | Create new project subfolder in workspace (Telegram) |
| `/agents` | List available agents and roles (Telegram) |

## Built-In Workspace Tools

When an agent has `workspace` configured, these tools are available automatically:

| Tool | Risk | Approval | Description |
|------|------|----------|-------------|
| `read_file` | Low | No | Read file contents (text and PDF) |
| `list_directory` | Low | No | List files and directories |
| `write_file` | Medium | Yes | Write content to file |
| `run_command` | High | Yes | Execute shell command in workspace |
| `remember` | Low | No | Store fact in long-term memory (when memory enabled) |

Tools marked "Yes" for approval require user confirmation before execution — via dialog in TUI mode, or inline keyboard buttons in Telegram mode.

## Multi-Agent Fleet

Run specialized agents as a coordinated team. Roles are fully dynamic — any string works:

```toml
[orchestrator]
enabled = true

[agents.qa]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
role = "qa"
allowed_tools = ["read_file", "list_directory", "run_command"]

[agents.qa.identity]
name = "QA Agent"
instructions = "You review code, run tests, and verify correctness."

[agents.backend]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
role = "backend_engineer"
allowed_tools = ["read_file", "list_directory", "write_file", "run_command"]

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

Tasks flow through: **Pending -> InProgress -> Completed** (with automatic retry on failure). Use `allowed_tools` to restrict which workspace tools each agent can access.

In Telegram, use `/team <goal>` to decompose a goal into tasks with dependency tracking. Independent tasks run in parallel batches; dependent tasks wait for their prerequisites. Use `/project <name>` to create isolated project subfolders within the workspace without restarting.

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

Agents call these tools during conversation. Restrict skills per agent with `skills = ["tool1", "tool2"]`.

See the [Skills Guide](docs/SKILLS.md) for the full format and examples.

## Standalone Tools

The `tools/` directory contains standalone CLI binaries that agents invoke via `run_command`:

| Tool | Description |
|------|-------------|
| `tools/ipnft-minter` | IP-NFT minting CLI for Molecule DeSci Labs (agreement, metadata, terms, on-chain mint) |

These are separate Cargo packages — not part of the main workspace. Build them independently:

```bash
cd tools/ipnft-minter && cargo build --release
```

### ipnft-minter

Handles steps 2-9 of the IPNFT minting flow (agreement, image, metadata, terms, sign, mint). The POI registration and on-chain submission (step 1) must be done separately — see `skills/aura-orchestrator/SKILL.md`.

```bash
ipnft-minter \
  --reservation-id "TOKEN_ID_FROM_POI" \
  --poi-tx-hash "0xPOI_TX_HASH" \
  --merkle-root "MERKLE_ROOT_HASH" \
  --name "Project" --description "Desc" --symbol SYM \
  --organization "Org" --lead-name "Name" --lead-email "email" --topic "Topic"
```

| Flag | Required | Description |
|------|----------|-------------|
| `--reservation-id` | Recommended | Token ID from POI on-chain transaction. If omitted, falls back to `reserve()` (sequential ID, not POI-linked). |
| `--poi-tx-hash` | With `--reservation-id` | Transaction hash of the POI on-chain submission. Required for POI-based assignments. |
| `--merkle-root` | With `--reservation-id` | Merkle root hash from POI response (`data.proof.tree[0]`). Required for POI-based assignments. |
| `--name` | Yes | Project name |
| `--symbol` | Yes | Token symbol |
| `--image` | No | Path to cover image (uses 1x1 placeholder if omitted) |

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
| [Configuration Reference](docs/CONFIGURATION.md) | Every config field, env var, default value, and validation rule |
| [Skills Guide](docs/SKILLS.md) | Skill file format, parameters, execution, policy, per-agent filtering |
| [Fleet Orchestration](docs/FLEET.md) | Multi-agent setup, roles, task lifecycle, heartbeat, events |
| [Sandboxes](docs/SANDBOXES.md) | Domain-specific multi-agent teams, per-agent tool restrictions |
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
