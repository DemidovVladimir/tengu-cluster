# Configuration

Tengu is configured via a single TOML file. Default location: `~/.tengu/config.toml`. Override with `--config <path>` or sandboxes (`--sandbox <name>` → `sandboxes/<name>/config.toml`).

See `config.example.toml` in the repo root for a commented reference.

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
workspace_tools = ["shared_cache"]      # Optional workspace tools
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

## Secrets

Store API keys encrypted:
```bash
tengu secret init
tengu secret set OPENROUTER_API_KEY sk-or-...
```

## Sandboxes

Domain-specific configurations in `sandboxes/<name>/config.toml`:

```bash
tengu telegram --sandbox aura           # OpenRouter-backed DeSci
tengu telegram --sandbox aura-claude    # Claude Code-backed DeSci
```

## Validation

Config is validated at load time. Invalid values produce clear error messages:
- Engine must be `openrouter` or `claude_code`
- Profile must be `none`, `read_only`, `editor`, or `editor_shell`
- Limits must be positive, cost thresholds consistent

## Related
- [[architecture]] — system overview
- [[engine-backends]] — engine comparison
- [[skills]] — skill_packages configuration
