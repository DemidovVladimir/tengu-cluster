---
tags:
  - reference
  - configuration
  - agents
  - sandboxes
aliases:
  - Agent Config
  - Sandbox Config
---

# Agent Configuration

Concise reference for configuring [[Agents]] and [[Sandboxes]]. For the full field-by-field reference, see [[Configuration Reference]].

## Minimal Agent

```toml
[agents.main]
default = true
engine = "openrouter"
model = "anthropic/claude-sonnet-4.6"

[agents.main.identity]
name = "Assistant"
instructions = "You are a helpful assistant."
```

## Full Agent Structure

```toml
[agents.<id>]
engine = "openrouter"                    # Required: openrouter | anthropic | openai | ollama | huggingface | claude-code
model = "anthropic/claude-sonnet-4.6"    # Required: provider-specific model ID
default = false                          # At most one agent can be default
role = "backend_engineer"                # Any string. Used for orchestrator routing.
requires = ["researcher"]                # Dependency: this agent runs after listed roles
workspace = "~/my-project"               # Workspace root for file tools. Tilde expanded.
skill_packages = ["privy", "beach-science"]  # Skills loaded into this agent's prompt
capabilities = [                         # Platform tools this agent can use
  "workspace.read",
  "workspace.list",
  "workspace.write",
  "workspace.shell",
  "http.request",
  "crypto.sign_tx",
  "crypto.sign_message",
  "crypto.wallet_address",
  "crypto.abi_encode",
  "memory.remember"
]

[agents.<id>.identity]
name = "Backend Engineer"                # Display name (shown in logs and Telegram)
instructions = """Multi-line system prompt.
Define what the agent does and how it should behave."""

[agents.<id>.flow]
scope = "per-sender"                     # "per-sender" | "global"
reset_mode = "idle"                      # "idle" | "manual"
idle_timeout_minutes = 30                # Auto-reset after inactivity
# max_history_turns = 20                 # Max conversation turns kept
# compaction_threshold_ratio = 0.82      # Trigger compaction at this % of budget

[agents.<id>.limits]
max_tokens_per_flow = 100_000            # Hard limit. 80% warning, 100% stops.
# context_window_override = 1_000_000    # Override auto-detected context window
# max_output_tokens_per_turn = 16384     # Override auto-detected output cap
# max_cost_per_flow = 5.0                # USD cost limit (optional)

[agents.<id>.prompt_budget]
max_skill_context_tokens = 4000          # Per-skill context in system prompt
max_total_tokens = 8000                  # Total system prompt budget
```

## Capabilities Quick Reference

> [!tip] Default behavior
> When `capabilities` is omitted, all platform tools are available. Only specify capabilities when you need to **restrict** access.

| ID | Tool | Risk |
|----|------|------|
| `workspace.read` | `read_file` | Low |
| `workspace.list` | `list_directory` | Low |
| `workspace.write` | `write_file` | Medium |
| `workspace.shell` | `run_command` | High |
| `http.request` | `http_request` | Medium |
| `crypto.sign_tx` | `sign_and_send_transaction` | High |
| `crypto.sign_message` | `sign_message` | High |
| `crypto.wallet_address` | `get_wallet_address` | Low |
| `crypto.abi_encode` | `abi_encode` | Low |
| `memory.remember` | `remember` | Medium |

See [[Capabilities]] for full details on effect classes and filtering.

## Sandbox Structure

```
sandboxes/
  desci/
    config.toml          # Self-contained config for the DeSci team
  webstudio/
    config.toml          # Self-contained config for the web dev team
```

Run with `--sandbox <name>`:

```bash
tengu telegram --sandbox desci
tengu orchestrate --sandbox webstudio
```

> [!important]
> Each sandbox is a **complete, standalone config**. There is no inheritance from `~/.tengu/config.toml`.

## Common Patterns

### Read-only analyst

```toml
[agents.analyst]
capabilities = ["workspace.read", "workspace.list"]
# No write, shell, http, or crypto — purely reads workspace files
```

### API-connected agent

```toml
[agents.api_agent]
capabilities = ["workspace.read", "workspace.list", "workspace.write", "http.request"]
skill_packages = ["beach-science"]
# Can read/write files and make HTTP calls guided by skill docs
```

### On-chain agent

```toml
[agents.minter]
capabilities = ["workspace.read", "workspace.list", "workspace.write", "http.request", "crypto.sign_tx", "crypto.sign_message", "crypto.wallet_address", "crypto.abi_encode", "memory.remember"]
skill_packages = ["poi-register", "ipnft-mint"]
```

### Dependency chain

```toml
[agents.researcher]
role = "researcher"
default = true         # Receives unrouted messages

[agents.minter]
role = "minter"
requires = ["researcher"]   # Runs after researcher

[agents.publisher]
role = "publisher"
requires = ["researcher", "minter"]   # Runs after both
```

## Context Window Defaults

Auto-detected from model ID (overridable via `context_window_override`):

| Model Family | Context Window |
|-------------|---------------|
| Claude Sonnet/Opus 4.6 | 1,000,000 |
| Claude (older) | 200,000 |
| GPT-4.1 | 1,047,576 |
| GPT-4o / GPT-4-turbo | 128,000 |
| Gemini | 1,000,000 |
| Llama, Mistral, DeepSeek | 128,000 |

## Prompt Budget

Controls how much of the context window is used for the system prompt:

| Field | Default | Description |
|-------|---------|-------------|
| `max_file_tokens` | 2,000 | Max tokens per workspace file in prompt |
| `max_skill_context_tokens` | 4,000 | Max tokens per skill's documentation |
| `max_total_tokens` | 8,000 | Total system prompt budget |

> [!tip] Large skills
> For skills with extensive documentation (like [[Skill - IP-NFT Mint]] with its 10-step pipeline), increase `max_skill_context_tokens` and `max_total_tokens` accordingly. The DeSci onchain_minter uses 16,000 / 48,000.

## Planner Model

The [[Orchestrator]] uses a lightweight LLM call to classify requests (single-agent vs multi-agent) and generate task plans. By default, it reuses the default agent's engine/model — which can be expensive if the default agent uses a premium model like Claude Sonnet 4.6.

Configure a dedicated cheaper/faster model for planning:

```toml
[orchestrator]
enabled = true
planner_engine = "openrouter"
planner_model = "google/gemini-2.5-flash"
```

> [!tip] Cost savings
> The planner only generates ~50-200 tokens of JSON. Using Gemini Flash ($0.15/M input) instead of Claude Sonnet 4.6 ($3/M input) saves 20x on planning calls.

When `planner_engine` + `planner_model` are not set, falls back to the default agent's engine (backwards compatible).

## Scaffold (Auto-create workspace)

```toml
[scaffold]
root = "~/my-workspace"

[scaffold.project]
directories = ["src", "docs", "data"]

[[scaffold.project.files]]
path = "README.md"
content = "# Project\nManaged by Tengu."
```

Runs once at startup. Never overwrites existing files.

## Related

- [[Agents]] — agent concepts
- [[Capabilities]] — permission model
- [[Skills]] — skill loading and filtering
- [[Sandboxes]] — sandbox examples
- [[Configuration Reference]] — full field reference
- [[DeSci]] — DeSci sandbox walkthrough
