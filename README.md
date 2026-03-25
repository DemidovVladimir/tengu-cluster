# Tengu Cluster

A composable multi-agent runtime built in Rust. Single binary, two engine backends, plug-and-play skills.

## Quickstart

### Prerequisites

- **Rust** 1.75+ ([rustup.rs](https://rustup.rs))
- An **OpenRouter** API key ([openrouter.ai](https://openrouter.ai)) — or a Claude Code CLI subscription

### Build & Run

```bash
git clone https://github.com/user/tengu-cluster.git && cd tengu-cluster
cargo build

# Set up secrets vault
cargo run -- secret init                              # prompts for master password
cargo run -- secret set OPENROUTER_API_KEY sk-or-...  # store your API key

# Create config
mkdir -p ~/.tengu
cp config.example.toml ~/.tengu/config.toml

# Start chatting
cargo run -- chat
```

The default config uses `nvidia/nemotron-3-super-120b-a12b:free` on OpenRouter (free tier). Change `model` in config to use any of 200+ models on OpenRouter.

### With Claude Code Backend

Use your Claude subscription instead of API tokens:

```bash
# Install Claude Code CLI: https://docs.anthropic.com/en/docs/claude-code
claude --version  # must be installed and authenticated

# Build with Claude Code support
cargo build --features claude_code

# Set engine in config:
# [agents.main]
# engine = "claude_code"
# model = "claude-sonnet-4-20250514"
```

## Engine Backends

| Backend | Config | Cost | Feature Flag |
|---------|--------|------|-------------|
| **OpenRouter** | `engine = "openrouter"` | Pay-per-token | `openrouter` (default) |
| **Claude Code** | `engine = "claude_code"` | Claude subscription | `claude_code` (opt-in) |

OpenRouter gives access to Anthropic, OpenAI, Google, Meta, DeepSeek, etc. behind one API key. Claude Code runs agents through your local `claude` CLI with native workspace tools + Tengu tools bridged via MCP.

You can mix backends in the same config — some agents on OpenRouter, others on Claude Code.

## CLI Commands

```bash
cargo run -- chat                        # Interactive TUI chat (default)
cargo run -- telegram                    # Telegram bot
cargo run -- telegram --sandbox aura     # Telegram with sandbox config
cargo run -- orchestrate                 # Multi-agent fleet
cargo run -- orchestrate --sandbox aura  # Fleet with sandbox config
cargo run -- status                      # Show config summary
cargo run -- doctor                      # Check backend connectivity
cargo run -- secret init                 # Create encrypted secrets vault
cargo run -- secret set KEY VALUE        # Store a secret
cargo run -- secret list                 # List stored secret keys
cargo run -- secret remove KEY           # Remove a secret
cargo run -- prune                       # Remove cached/ephemeral state
cargo run -- prune --sandbox aura        # Also clean workspace state
cargo run --features claude_code,telegram -- telegram --sandbox aura-claude # Run with claude subscription
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

## Concepts

| Concept | What It Is |
|---------|-----------|
| **Agent** | An LLM-backed actor with a role, identity, and permissions. Defined in config. |
| **Tool** | A platform primitive (read_file, write_file, list_directory, run_command, http_request, crypto, memory, cache). |
| **Skill** | Domain knowledge packaged as a markdown file. Drop into `skills/` — no code changes. |
| **Capability** | A permission gating which tools an agent can use. |

Skills compose platform tools into domain workflows. Adding a new skill = drop a file. Adding a new agent = add config.

## Skills

Define domain workflows as `skills/<name>/SKILL.md` files. Two formats:

**Documentation skills** — workflow reference injected into agent prompt, agent uses platform tools (http_request, crypto, etc.) to execute:
```markdown
---
name: my-api
description: My external API workflow
---
# API Documentation
Full reference here...
```

**Shell skills** — named tools with execution templates:
```markdown
# test_runner
Run the project test suite.
## Parameters
- `filter` (string, optional): test name filter
## Execution
\```bash
cargo test {{filter}} 2>&1
\```
```

Load per agent with `skill_packages = ["skill-name"]` in config.

## Multi-Agent Fleet

Configure specialized agents as a team:

```toml
[orchestrator]
enabled = true

[agents.researcher]
engine = "claude_code"
model = "claude-sonnet-4-20250514"
role = "researcher"
workspace = "~/research"

[agents.researcher.identity]
name = "Researcher"
instructions = "You research topics and write summaries."

[agents.coder]
engine = "openrouter"
model = "anthropic/claude-sonnet-4.6"
role = "backend_engineer"
workspace = "~/project"

[agents.coder.identity]
name = "Coder"
instructions = "You write server code and APIs."
```

```bash
cargo run -- orchestrate
```

The main agent decides when to spawn subagents via tools (`sessions_spawn`, `sessions_fan_out`). No static DAG — the LLM drives orchestration.

## Sandboxes

Domain-specific team configs in `sandboxes/<name>/config.toml`:

| Sandbox | Backend | Description |
|---------|---------|-------------|
| `aura` | OpenRouter | DeSci pipeline (research, mint, publish) |
| `aura-claude` | Claude Code | Same pipeline, Claude subscription |

```bash
cargo run --features claude_code,telegram -- telegram --sandbox aura-claude
```

## Telegram Bot

```bash
# Build with Telegram support (on by default)
cargo build --features telegram

# Set bot token (get from @BotFather)
cargo run -- secret set TELEGRAM_BOT_TOKEN "your-token"

# Add your user ID to config:
# [telegram]
# enabled = true
# allowed_users = ["YOUR_USER_ID"]

cargo run -- telegram
```

In multi-agent mode: plain messages are orchestrated across the team, `@role: message` targets a specific agent.

## Deployment

Deploy with Docker Compose:

```bash
make setup       # creates .env and config.toml
make up          # start tengu
make down        # stop
make logs        # tail logs
```

See `docker-compose.yml`, `Dockerfile`, and `deploy/` for cloud provisioning.

## Feature Flags

| Flag | Default | Purpose |
|------|---------|---------|
| `openrouter` | on | OpenRouter API backend |
| `telegram` | on | Telegram bot channel |
| `claude_code` | off | Claude Code CLI backend |
| `qdrant` | off | Qdrant vector store for memory |

```bash
cargo build --features claude_code,telegram    # Claude Code + Telegram
cargo build --all-features                     # everything
```

## Configuration

See `config.example.toml` for a fully commented reference. Key sections:

- `[agents.<id>]` — engine, model, workspace, skills, limits, identity
- `[orchestrator]` — fleet orchestration
- `[memory]` — persistent vector memory (requires `OPENROUTER_API_KEY` for embeddings)
- `[telegram]` — bot adapter
- `[claude_code]` — Claude Code CLI settings
- `[scaffold]` — workspace directory templates

## Documentation

Detailed docs live in the `docs/` folder:

| Doc | What It Covers |
|-----|---------------|
| [Architecture](docs/architecture.md) | System overview, core loop, module map |
| [Engine Backends](docs/engine-backends.md) | OpenRouter vs Claude Code comparison |
| [Configuration](docs/configuration.md) | Full config reference |
| [Skills](docs/skills.md) | Skill types, loading, cross-engine compatibility |
| [MCP Bridge](docs/mcp-bridge.md) | How Tengu tools reach Claude Code |

## Architecture

Flat module structure — all code in `src/adapters/` + `src/main.rs`. No sub-crates.

```
src/
  main.rs                      # CLI entry point
  adapters/
    config.rs                  # TOML config, validation
    types.rs                   # Engine trait, Message, StreamEvent
    engine_builder.rs          # Engine factory + OpenRouter impl
    claude_code_engine.rs      # Claude Code engine (feature-gated)
    mcp_bridge.rs              # MCP stdio server for tool bridging
    chat_builder.rs            # Per-turn runtime (memory, prompt, history)
    channel_runtime.rs         # Shared channel adapter logic
    tool_builder.rs            # Tool definitions + workspace executor
    skill_builder.rs           # Skill parsing and registry
    memory_builder.rs          # Memory service + disk store
    orchestrator.rs            # Multi-agent fleet orchestrator
    telegram_builder.rs        # Telegram bot adapter
    tui/mod.rs                 # Terminal UI adapter
    ...
```
