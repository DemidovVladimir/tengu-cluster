# Tengu Cluster

Model-agnostic AI agent fleet runtime in Rust. Single binary, zero dependencies.

## What It Does

- **Single-agent chat** — talk to any AI model from your terminal with persistent history
- **Multi-agent fleet** — run specialized agents (QA, backend, integration) as a coordinated team
- **Any model, one key** — use [OpenRouter](https://openrouter.ai) to access Claude, GPT, Gemini, Llama, Mistral, DeepSeek and hundreds more behind one API key
- **Custom skills** — define tools as markdown files, agents execute them during conversation
- **Telegram channel** — chat with your agent from your phone

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

## Multi-Agent Fleet

Run specialized agents as a team:

```toml
[orchestrator]
enabled = true

[agents.qa]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
role = "qa"
skills = ["search", "test_runner"]

[agents.backend]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
role = "backend_engineer"
```

```bash
cargo run -- orchestrate
```

Tasks flow through: **Pending -> InProgress -> Completed** (with automatic retry on failure).

See the [Fleet Orchestration Guide](docs/FLEET.md) for the full setup.

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

## Documentation

| Guide | What It Covers |
|-------|---------------|
| [Quickstart](docs/QUICKSTART.md) | Installation, first config, first chat, next steps |
| [Configuration Reference](docs/CONFIGURATION.md) | Every config field, env var, default value, and validation rule |
| [Skills Guide](docs/SKILLS.md) | Skill file format, parameters, execution, policy, per-agent filtering |
| [Fleet Orchestration](docs/FLEET.md) | Multi-agent setup, roles, task lifecycle, heartbeat, events |
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
| `evm` | off | EVM wallet signing and transaction submission (alloy) |

```bash
# Build with Qdrant vector store
cargo build --features qdrant

# Build with EVM signing
cargo build --features evm

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
