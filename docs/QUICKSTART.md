# Quickstart

Get Tengu Cluster running in under 5 minutes.

## Prerequisites

- **Rust toolchain** (1.75+): <https://rustup.rs>
- **An API key** from one of:
  - [OpenRouter](https://openrouter.ai) (recommended — one key for all providers)
  - [Anthropic](https://console.anthropic.com)
  - [OpenAI](https://platform.openai.com)
  - [Hugging Face](https://huggingface.co/settings/tokens)
  - Or a local [Ollama](https://ollama.com) instance (no key needed)

## Step 1: Build

```bash
git clone https://github.com/anthropics/tengu-cluster.git
cd tengu-cluster
cargo build
```

## Step 2: Set Your API Key

Pick one provider and export its key:

```bash
# Option A: OpenRouter (recommended — access Claude, GPT, Gemini, Llama, etc.)
export OPENROUTER_API_KEY=sk-or-...

# Option B: Direct provider
export ANTHROPIC_API_KEY=sk-ant-...
# or
export OPENAI_API_KEY=sk-...
# or
export HF_TOKEN=hf_...
```

## Step 3: Create Config

```bash
mkdir -p ~/.tengu
cp config.example.toml ~/.tengu/config.toml
```

The default config uses OpenRouter with `anthropic/claude-sonnet-4`. If you set `OPENROUTER_API_KEY`, it works out of the box.

To use a different provider, edit `~/.tengu/config.toml`:

```toml
[agents.main]
default = true
engine = "anthropic"             # or "openai", "ollama", "huggingface", "claude-code"
model = "claude-sonnet-4-20250514"
```

## Step 4: Chat

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

## Step 5: Verify Setup

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
model = "anthropic/claude-sonnet-4"      # Claude
model = "openai/gpt-4o"                  # GPT-4o
model = "google/gemini-2.5-pro"          # Gemini
model = "meta-llama/llama-4-maverick"    # Llama 4
model = "mistralai/mistral-large"        # Mistral
model = "deepseek/deepseek-chat-v3"      # DeepSeek
```

Browse all models: <https://openrouter.ai/models>

## Adding Skills

Skills give your agent the ability to execute commands. Place markdown files in a `skills/` directory at the project root:

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

The agent can now call this skill as a tool during conversation. See [Skills Guide](SKILLS.md) for the full format.

## Adding a Workspace

Point your agent at a project directory to enable file tools (read, write, list):

```toml
[agents.main]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
workspace = "~/projects/my-app"
```

The agent can now read files, list directories, and write files within that workspace. Place `IDENTITY.md`, `PROFILE.md`, or `CONTEXT.md` in the workspace root to inject custom system prompt context.

## Running Multiple Agents

Configure a fleet of specialized agents with roles:

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

See [Fleet Orchestration Guide](FLEET.md) for the full setup.

## Next Steps

- [Configuration Reference](CONFIGURATION.md) — every config field, env var, and default value
- [Skills Guide](SKILLS.md) — define custom tools for your agent
- [Fleet Orchestration Guide](FLEET.md) — multi-agent setup, roles, task lifecycle
