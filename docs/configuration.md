# Configuration

Tengu is configured via a single TOML file. Default location: `~/.tengu/config.toml`. Override with `--config <path>` or sandboxes (`--sandbox <name>` → `sandboxes/<name>/config.toml`).

See `config.example.toml` in the repo root for a commented reference.

## Initial Setup

```bash
# 1. Create encrypted secrets vault
tengu secret init                              # prompts for master password

# 2. Store API keys
tengu secret set OPENROUTER_API_KEY sk-or-...  # required for OpenRouter engine
tengu secret set TELEGRAM_BOT_TOKEN 123:ABC-.. # required for Telegram bot

# 3. Create config
mkdir -p ~/.tengu
cp config.example.toml ~/.tengu/config.toml

# 4. Run
tengu chat                                     # interactive TUI
tengu telegram                                 # Telegram bot
```

To skip the master password prompt, set `TENGU_MASTER_PASSWORD` env var.

## Root Sections

| Section | Purpose |
|---------|---------|
| `runtime_profile` | `"auto"` / `"cloud"` / `"desktop"` / `"minimal"` |
| `[hub]` | Bind address, port, auth mode, hot-reload |
| `[agents.<id>]` | Per-agent configuration (see below) |
| `[orchestrator]` | Fleet orchestration settings |
| `[memory]` | Persistent vector memory |
| `[telegram]` | Telegram bot adapter |
| `[scaffold]` | Workspace directory/file scaffolding |
| `[claude_code]` | Global Claude Code backend settings |
| `[skill_lifecycle]` | Skill eval / metrics / evolve (required to activate `tengu skill-evolve`) |

## Agent Configuration

```toml
[agents.main]
engine = "openrouter"              # "openrouter" | "claude_code"
model = "anthropic/claude-sonnet-4.6"
default = true                     # At most one agent can be default
workspace = "~/projects/my-app"    # Workspace root
default_lens = "eco"               # "eco" | "standard" | "precise"
role = "backend_engineer"          # Orchestration role (optional)
skill_packages = ["aura-orchestrator"]  # Skills to load
workspace_tools = ["shared_cache", "skill_distill"]  # Optional workspace tools (see [[skills#Distillation]])
```

### Engine Selection

The `engine` field selects the execution backend. See [[engine-backends]] for details.

| Value | Backend | Requires |
|-------|---------|----------|
| `openrouter` | OpenRouter API | `OPENROUTER_API_KEY` env var |
| `claude_code` | Claude Code CLI | `claude` CLI installed, `--features claude_code` |

### Identity

```toml
[agents.main.identity]
name = "My Agent"
instructions = "You are a helpful assistant."
```

### Flow

```toml
[agents.main.flow]
scope = "per-sender"                # "main" | "per-group" | "per-pipe-sender" | "per-sender"
reset_mode = "idle"                 # "idle" | "manual" | "time"
idle_timeout_minutes = 30
max_history_turns = 20              # optional
compaction_threshold_ratio = 0.82   # optional (0.0, 1.0]
compaction_keep_turns = 24          # optional
compaction_summary_max_tokens = 320 # optional
```

### Limits

```toml
[agents.main.limits]
max_tokens_per_flow = 100_000      # Hard limit (warning at 80%)
context_window = 1_000_000
max_tool_rounds = 70               # Max tool-loop iterations
max_tool_result_chars = 300_000    # Truncation threshold per result
stream_event_timeout_secs = 120
compact_result_limit = 200         # Old-round tool result compaction
max_output_tokens_per_turn = 4096  # optional (must be <= context_window)
max_cost_per_flow = 5.0            # optional (USD)
warn_at_cost = 4.0                 # optional (must be <= max_cost_per_flow)
```

## Claude Code Configuration

### Global

```toml
[claude_code]
cli_path = "claude"   # Path to Claude Code CLI binary (default: "claude")
```

### Per-Agent

```toml
[agents.main.claude_code]
builtin_tools_profile = "editor_shell"  # "none" | "read_only" | "editor" | "editor_shell"
```

The profile controls which Claude-native tools are allowed. See [[engine-backends#Builtin Tools Profiles]].

This section is only used when `engine = "claude_code"`.

## Orchestrator

```toml
[orchestrator]
enabled = true
max_retries = 2
planner_engine = "openrouter"       # or "claude_code"
planner_model = "anthropic/claude-sonnet-4.6"
```

## Memory

```toml
[memory]
enabled = true
embedding_model = "text-embedding-3-small"
max_recall_entries = 5
max_recall_tokens = 600
store_path = "~/.tengu/memory/"
backend = "disk"              # "disk" | "qdrant" (requires --features qdrant)
```

Memory embeddings require `OPENROUTER_API_KEY` regardless of engine backend.

## Telegram

```toml
[telegram]
enabled = true
allowed_users = ["123456789"]    # Telegram user IDs (get from @userinfobot)
```

Set `TELEGRAM_BOT_TOKEN` in the secrets vault or as an env var. Get a token from @BotFather on Telegram.

## Secrets

Store API keys encrypted in `~/.tengu/secrets.vault`:

```bash
tengu secret init                  # create vault + set master password
tengu secret set KEY VALUE         # store a secret
tengu secret list                  # list stored keys
tengu secret remove KEY            # remove a secret
```

Skip the interactive password prompt with:
```bash
export TENGU_MASTER_PASSWORD="your-password"
```

## Sandboxes

Domain-specific configurations in `sandboxes/<name>/config.toml`:

```bash
tengu telegram --sandbox aura           # OpenRouter-backed DeSci
tengu telegram --sandbox aura-claude    # Claude Code-backed DeSci
```

## Feature Flags

| Flag | Default | Purpose |
|------|---------|---------|
| `openrouter` | on | OpenRouter API backend |
| `telegram` | on | Telegram bot channel |
| `claude_code` | off | Claude Code CLI backend |
| `qdrant` | off | Qdrant vector store for memory |

```bash
cargo build                              # default: openrouter + telegram
cargo build --features claude_code       # + Claude Code backend
cargo build --all-features               # everything
```

## Reset / Fresh Start

```bash
rm -rf ~/.tengu              # remove all config, secrets, logs, and state
tengu chat                   # starts with built-in defaults
```

Selective cleanup:

```bash
rm -rf ~/.tengu/state        # session state only
rm -rf ~/.tengu/logs         # logs only
tengu prune                  # remove cached/ephemeral state (keeps config & secrets)
```

## Validation

Config is validated at load time. Invalid values produce clear error messages:
- Engine must be `openrouter` or `claude_code`
- Profile must be `none`, `read_only`, `editor`, or `editor_shell`
- Limits must be positive, cost thresholds consistent
- `workspace_tools` entries must be in `["shared_cache", "persistent_store", "skill_distill"]`

## Skill Lifecycle

Enables `tengu eval <skill>`, `tengu skill-metrics <skill>`, and `tengu skill-evolve <skill>`. See [[skills#Metrics & Evolution]] for the frontmatter contract. This block is optional — absence disables the evolve CLI but does not affect chat/eval of skills that don't declare metrics.

```toml
[skill_lifecycle]
improver_agent       = "skill-improver"     # Name of the agent (in [agents.*]) that proposes rewrites
fixture_runner_agent = "fixture-runner"     # Name of the agent that executes eval fixtures
default_max_evolve_cycles = 3               # Tier-2 cap on rewrite→rescore cycles per evolve run
default_rolling_window    = 10              # Window over which metrics.json pass_rate is computed

[agents.skill-improver]
engine = "openrouter"
model  = "anthropic/claude-opus-4-7"
workspace_tools = []                         # Read-only; harness applies diffs, not the agent

[agents.skill-improver.identity]
name = "Skill Improver"
instructions = """
You are a skill-improver. Given a skill that is under-performing on a specific
metric, propose a REVISED skill body that raises the metric's pass rate without
regressing others. Emit ONE JSON object and nothing else:
{"proposal":{"body_markdown":"...","metrics":[...]?,"rationale":"..."}}
"""

[agents.fixture-runner]
engine = "openrouter"
model  = "anthropic/claude-sonnet-4-6"
workspace_tools = []
```

Agents that should be able to author skills mid-conversation add `"skill_distill"` to their own `workspace_tools`:

```toml
[agents.main]
workspace_tools = ["skill_distill"]
```

## Related
- [[architecture]] — system overview
- [[engine-backends]] — engine comparison
- [[skills]] — skill_packages configuration + `[skill_lifecycle]` details
- `docs/superpowers/specs/2026-04-20-skill-metrics-evolution-design.md` — design spec
