---
tags:
  - getting-started
  - setup
  - quickstart
---

# Quickstart

Get Tengu Cluster running in under 5 minutes.

## Choose Your Path

- **Docker** (recommended) — no Rust toolchain needed, works on any OS
- **Native** — build from source, best for development and macOS Metal GPU

---

## Docker Quickstart

**Prerequisites:** [Docker](https://docker.com) installed.

```bash
git clone https://github.com/user/tengu-cluster.git
cd tengu-cluster
make setup                    # creates .env and config.toml
nano .env                     # set OPENROUTER_API_KEY (or other API keys)
make up                       # build & start tengu
```

Verify it's running:

```bash
make status                   # show running services
make doctor                   # run diagnostics
```

Interact with the agent:

```bash
docker compose exec -it tengu tengu chat          # interactive chat
# or configure Telegram bot (see below) and run:
# make down && make up        # restarts with telegram as default
```

For GPU acceleration, Qdrant memory, or cloud deployment, see [[Deployment]].

---

## Native Quickstart

### Prerequisites

- **Rust toolchain** (1.75+): <https://rustup.rs>
- **An API key** from one of:
  - [OpenRouter](https://openrouter.ai) (recommended — one key for all providers)
  - [Anthropic](https://console.anthropic.com)
  - [OpenAI](https://platform.openai.com)
  - [Hugging Face](https://huggingface.co/settings/tokens)
  - Or a local [Ollama](https://ollama.com) instance (no key needed)

### Step 1: Build

```bash
git clone https://github.com/user/tengu-cluster.git
cd tengu-cluster
cargo build
```

### Step 2: Set Your API Key

Use the encrypted secrets vault (AES-256-GCM, master-password protected):

```bash
# Create the vault — prompts for a master password (12+ chars recommended)
cargo run -- secret init

# Store your API key (prompts for master password)
# Option A: OpenRouter (recommended — access Claude, GPT, Gemini, Llama, etc.)
cargo run -- secret set OPENROUTER_API_KEY sk-or-...

# Option B: Direct provider
cargo run -- secret set ANTHROPIC_API_KEY sk-ant-...
# or
cargo run -- secret set OPENAI_API_KEY sk-...
# or
cargo run -- secret set HF_TOKEN hf_...
```

The vault is stored at `~/.tengu/secrets.vault` with `chmod 600`. At startup, Tengu prompts for your master password to decrypt the vault and load secrets into the environment. Set `TENGU_MASTER_PASSWORD` env var to skip the interactive prompt. To change your password later: `cargo run -- secret change-password`.

Alternatively, you can still use plain environment variables:

```bash
export OPENROUTER_API_KEY=sk-or-...
```

### Step 3: Create Config

```bash
mkdir -p ~/.tengu
cp config.example.toml ~/.tengu/config.toml
```

The default config uses OpenRouter with `nvidia/nemotron-3-super-120b-a12b:free` (free tier). If you set `OPENROUTER_API_KEY`, it works out of the box.

To use a different provider, edit `~/.tengu/config.toml`. See [[Configuration]] for all available fields.

```toml
[agents.main]
default = true
engine = "anthropic"             # or "openai", "ollama", "huggingface", "claude-code"
model = "claude-sonnet-4-20250514"
```

### Step 4: Chat

```bash
cargo run -- chat
```

Type a message and press Enter. The agent responds in real time with streaming output.

Your conversation history is saved automatically at `~/.tengu/state/flows/` and restored next session.

### Chat Commands

Once inside the chat, type these commands:

| Command | What It Does |
|---------|-------------|
| `/help` | List all commands |
| `/cost` | Show token usage for this session |
| `/context` | Show context window usage and compaction status |
| `/engine` | Show current engine, model, and capabilities |
| `/eco` | Switch to eco lens (lowest cost, summaries only) |
| `/standard` | Switch to standard lens (balanced) |
| `/precise` | Switch to precise lens (full fidelity) |
| `/reset` | Clear conversation and start fresh |

### Step 5: Verify Setup

Check that your backend is reachable:

```bash
cargo run -- doctor
```

This probes the configured engine endpoint and reports connectivity, model availability, and flow store health.

View your current configuration:

```bash
cargo run -- status
```

## Switching Models

With OpenRouter, switch models by editing one line in config:

```toml
model = "nvidia/nemotron-3-super-120b-a12b:free"  # Nemotron 120B (free)
model = "google/gemini-2.5-flash"                 # Gemini Flash (cheap)
model = "anthropic/claude-sonnet-4"               # Claude Sonnet 4 (premium)
model = "openai/gpt-4o"                           # GPT-4o
model = "deepseek/deepseek-chat-v3"               # DeepSeek V3 (cheap)
```

Browse all models: <https://openrouter.ai/models>

## Adding Skills

[[Skills]] give your agent the ability to execute commands. Place markdown files in a `skills/` directory at the project root:

```markdown
# search

Search files in the workspace by pattern.

## Parameters
- `pattern` (string, required): glob pattern to search for

## Execution
```bash
find . -name "{{pattern}}"
```
```

The agent can now call this skill as a tool during conversation. See [[Skills]] for the full format, API skills, discovery rules, and per-agent filtering.

## Adding a Workspace

Point your agent at a project directory to enable file [[Tools]] (read, write, list):

```toml
[agents.main]
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"
workspace = "~/projects/my-app"
```

The agent can now read files, list directories, and write files within that workspace. Place `IDENTITY.md`, `PROFILE.md`, or `CONTEXT.md` in the workspace root to inject custom system prompt context.

## Running Multiple Agents

Configure a fleet of specialized agents with roles via the [[Orchestrator]]:

```toml
[orchestrator]
enabled = true

[agents.qa]
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"
role = "qa"
skill_packages = ["search", "test_runner"]

[agents.backend]
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"
role = "backend_engineer"
```

```bash
cargo run -- orchestrate
```

See [[Orchestrator]] for the full setup, task planning, parallel execution, and dependency resolution.

## Telegram Bot

Chat with your agent from Telegram instead of the terminal (one of the available [[Channels]]):

```bash
# Store your bot token (get one from @BotFather on Telegram)
cargo run -- secret set TELEGRAM_BOT_TOKEN 123456:ABC-DEF...

# Add your Telegram user ID to config (get it from @userinfobot)
# In ~/.tengu/config.toml:
# [telegram]
# enabled = true
# allowed_users = ["YOUR_USER_ID"]

# Run the bot
cargo run -- telegram
```

Send a message to your bot in Telegram and it responds with full agent capabilities including [[Tools]], [[Skills]], memory, and file attachments. Dangerous tools (`write_file`, `run_command`) prompt you with inline keyboard Approve/Deny buttons before execution. See [[Configuration]] for Telegram-specific settings.

## Next Steps

- [[Deployment]] — Docker, Docker Compose, GPU (CUDA/Metal), cloud provisioning
- [[Configuration]] — every config field, env var, and default value
- [[Skills]] — define custom tools for your agent
- [[Orchestrator]] — multi-agent setup, roles, task lifecycle
- [[Tools]] — workspace primitives and tool approval system
- [[Channels]] — Telegram, TUI, and other communication adapters
