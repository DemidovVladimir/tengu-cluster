---
tags:
  - reference
  - configuration
  - agents
---

# Configuration Reference

Detailed per-field reference tables for all [[Configuration]] sections. This page covers every configurable field, validation rule, environment variable, Docker path, and file location.

---

## Engine and Model

```toml
[agents.main]
default = true
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"
workspace = "~/projects/my-app"
default_lens = "eco"
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `engine` | string | (required) | See engine table below |
| `model` | string | (required) | Provider-specific model ID |
| `default` | bool | `false` | At most one agent can be default |
| `workspace` | string? | none | Path to local workspace for file [[Tools]] |
| `default_lens` | string | `"eco"` | `"eco"`, `"standard"`, `"precise"` |

**Supported engines:**

| Engine | API Style | Model ID Format | Example |
|--------|----------|----------------|---------|
| `openrouter` | OpenAI-compatible | `provider/model` | `nvidia/nemotron-3-super-120b-a12b:free` |
| `anthropic` | Anthropic native | Anthropic model ID | `claude-sonnet-4-20250514` |
| `openai` | OpenAI native | OpenAI model ID | `gpt-4o` |
| `ollama` | Ollama HTTP | Ollama model name | `llama3.2` |
| `huggingface` | OpenAI-compatible | `org/model:variant` | `THUDM/GLM-4.7:fastest` |
| `claude-code` | Subprocess CLI | Claude model ID | `claude-sonnet-4-5-20250929` |

See [[Architecture]] for how engines map to backend adapters.

**OpenRouter model examples:**

```toml
model = "nvidia/nemotron-3-super-120b-a12b:free"  # Nemotron 120B (free)
model = "google/gemini-2.5-flash"                 # Gemini Flash (cheap)
model = "anthropic/claude-sonnet-4"               # Claude Sonnet 4 (premium)
model = "openai/gpt-4o"                           # GPT-4o
model = "deepseek/deepseek-chat-v3"               # DeepSeek V3 (cheap)
```

---

## Identity

```toml
[agents.main.identity]
name = "Tengu"
instructions = "You are a Rust programming assistant. Be concise."
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `name` | string? | none | Agent display name in prompts and UI |
| `instructions` | string? | none | Free-form instructions injected into the system prompt |

A system prompt is **always** sent to the model, even with no workspace or identity files. The composition order is:

1. **Preamble** (always): `"You are {name}, an AI assistant."` (name defaults to `"Tengu"`)
2. **Role fragment**: if `role` is set, role-specific guidance is appended
3. **Custom instructions**: the `instructions` field verbatim
4. **Workspace files**: `IDENTITY.md`, `PROFILE.md`, `CONTEXT.md` from the workspace directory
5. **Available tools**: tool descriptions generated dynamically from `ToolDef` metadata (workspace primitives + subsystem tools + [[Skills]])

---

## Flow (Session Behavior)

```toml
[agents.main.flow]
scope = "per-sender"
reset_mode = "idle"
idle_timeout_minutes = 30
max_history_turns = 80
compaction_threshold_ratio = 0.82
compaction_keep_turns = 24
compaction_summary_max_tokens = 320
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `scope` | string | `"per-sender"` | How conversation flows are isolated |
| `reset_mode` | string | `"idle"` | When to clear conversation history |
| `idle_timeout_minutes` | u32 | `30` | Minutes of inactivity before auto-reset (when `reset_mode = "idle"`) |
| `max_history_turns` | u32? | none | Hard cap on user-agent turn pairs in history |
| `compaction_threshold_ratio` | f32? | none | Fraction of `max_tokens_per_flow` that triggers compaction (0.0-1.0) |
| `compaction_keep_turns` | u32? | none | Recent user turns to preserve during compaction |
| `compaction_summary_max_tokens` | u32? | none | Token budget for the compaction summary |

**Flow scopes explained:**

| Scope | Key Format | Use Case |
|-------|-----------|----------|
| `main` | `{agent}:main` | Single shared conversation for all users |
| `per-sender` | `{agent}:{peer_id}` | One conversation per user (default, recommended) |
| `per-pipe-sender` | `{agent}:{pipe}:{peer_id}` | One conversation per user per [[Channels|channel]] |
| `per-group` | `{agent}:{pipe}:{thread_id}` | One conversation per group/thread |

**Reset modes:**

| Mode | Behavior |
|------|----------|
| `idle` | Auto-reset after `idle_timeout_minutes` of no messages |
| `manual` | Only reset on `/reset` command |
| `time` | Reset on configured interval (future) |

**Compaction defaults** (when not overridden, depends on scope):

| Scope | Threshold Ratio | Keep Turns |
|-------|----------------|------------|
| `main` | 0.88 | 60 |
| `per-group` | 0.86 | 40 |
| `per-pipe-sender` | 0.84 | 32 |
| `per-sender` | 0.82 | 24 |

**How compaction works:**
1. When `flow_token_usage >= max_tokens_per_flow * threshold_ratio`, compaction triggers.
2. The oldest messages (beyond `keep_turns` recent user turns) are summarized into a single message.
3. The summary replaces the old messages, freeing token budget.
4. Requires `refiner.mode` to not be `"off"` for actual summarization.

---

## Limits

```toml
[agents.main.limits]
max_tokens_per_flow = 100_000
context_window_override = 200000
max_output_tokens_per_turn = 4096
max_cost_per_flow = 5.0
warn_at_cost = 4.0
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `max_tokens_per_flow` | u64 | `100_000` | Hard token limit for entire conversation flow. A warning is sent at 80% usage. |
| `context_window_override` | u32? | none | Override engine-reported context window size |
| `max_output_tokens_per_turn` | u32? | none | Cap output tokens per engine turn |
| `max_cost_per_flow` | f64? | none | USD cost limit for the flow |
| `warn_at_cost` | f64? | none | USD threshold for cost warning (must be <= `max_cost_per_flow`) |

**Token budget behavior:**
- At **80% usage**: the system sends a warning notice showing current token consumption and remaining budget.
- At **100% usage**: further requests are blocked with a message to use `/reset` to start a new session.
- Token usage is tracked per-flow (cumulative across all turns in the conversation).

**Context window defaults by model** (when no override):

| Model Family | Default Window |
|-------------|---------------|
| Claude (Anthropic) | 200,000 |
| GPT-4.1 (OpenAI) | 1,000,000 |
| GPT-4o/4o-mini | 128,000 |
| Gemini 2.5 Pro | 1,000,000 |
| Llama 4 Maverick | 128,000 |
| Mistral Large | 128,000 |
| DeepSeek V3 | 128,000 |
| Ollama (any) | 8,192 |

---

## Lens

Fine-tune retrieval behavior per lens mode.

```toml
[agents.main.lens]
eco_max_tokens = 100
standard_threshold = 0.7
precise_budget = 0.5
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `eco_max_tokens` | u32 | `100` | Max tokens retrieved in eco mode |
| `standard_threshold` | f32 | `0.7` | Confidence threshold for auto-expand in standard mode (0.0-1.0) |
| `precise_budget` | f32 | `0.5` | Fraction of available tokens used in precise mode (0.0-1.0) |

**Lens mode comparison:**

| Lens | Cost | Fidelity | Best For |
|------|------|----------|----------|
| `eco` | Lowest | Summary only | Long conversations, cost-sensitive |
| `standard` | Medium | Auto-expands relevant sections | General use |
| `precise` | Highest | Full content up to budget | Complex reasoning, detail-critical |

Switch lens during chat with `/eco`, `/standard`, or `/precise`.

---

## Prompt Budget

Control how much of the system prompt token budget is allocated to workspace files, [[Skills|skill]] context fragments, and the total prompt.

```toml
[agents.main.prompt_budget]
max_file_tokens = 2000
max_skill_context_tokens = 4000
max_total_tokens = 8000
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `max_file_tokens` | usize | `2000` | Max tokens per workspace file (IDENTITY.md, PROFILE.md, CONTEXT.md) and instructions |
| `max_skill_context_tokens` | usize | `4000` | Max tokens per skill context fragment (API docs from frontmatter [[Skills]]) |
| `max_total_tokens` | usize | `8000` | Max total tokens for the assembled system prompt |

**Constraints:** all values must be > 0, and both `max_file_tokens` and `max_skill_context_tokens` must be <= `max_total_tokens`.

For [[Agents]] with large API skill docs, increase `max_skill_context_tokens` and `max_total_tokens` to ensure the full context reaches the model.

---

## Role, Tools, and Skills

For fleet orchestration [[Agents]]:

```toml
[agents.qa]
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"
role = "qa"
capabilities = ["workspace.read", "workspace.list", "workspace.shell", "skill.search", "skill.test_runner"]
skill_packages = ["search", "test_runner"]
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `role` | string? | none | Any non-empty string (e.g., `"qa"`, `"frontend_engineer"`, `"warehouse_manager"`). Used for task routing in [[Orchestrator]]. |
| `requires` | string[] | `[]` | Role keys this agent depends on. Tasks for this agent must follow tasks from these roles. Used by the planner to enforce correct dependency ordering. |
| `capabilities` | string[] | `[]` | Hard runtime permissions. Examples: `workspace.read`, `workspace.write`, `workspace.shell`, `memory.remember`, `skill.aura_orchestrator`, `desci.poi.register`. See [[Capabilities]]. |
| `skill_packages` | string[] | `[]` | [[Skills|Skill]]/workflow packages to load into the agent prompt and tool registry. |

Roles are fully dynamic -- any non-empty string is valid. Define role-specific behavior through `identity.instructions`.

See [[Orchestrator]] for fleet execution and [[Agents]] for the full agent model.

---

## Environment Variables

All environment variables. Export them in your shell, `direnv`, process manager, or store them in the encrypted secrets vault (see [[Configuration]]).

### Required (by engine)

| Variable | Required When | Example |
|----------|--------------|---------|
| `OPENROUTER_API_KEY` | `engine = "openrouter"` or `memory.enabled = true` | `sk-or-v1-abc...` |
| `ANTHROPIC_API_KEY` | `engine = "anthropic"` | `sk-ant-api03-abc...` |
| `OPENAI_API_KEY` | `engine = "openai"` | `sk-abc...` |
| `HF_TOKEN` | `engine = "huggingface"` | `hf_abc...` |
| `TELEGRAM_BOT_TOKEN` | `tengu telegram` command | `123456:ABC-DEF...` |

### Optional

| Variable | Default | Notes |
|----------|---------|-------|
| `TENGU_MASTER_PASSWORD` | none | Master password for secrets vault (skips interactive prompt) |
| `TENGU_HOME` | `~/.tengu` | Base config/state directory |
| `OLLAMA_HOST` | `http://localhost:11434` | Ollama endpoint |
| `ANTHROPIC_BASE_URL` | `https://api.anthropic.com` | Anthropic endpoint override |
| `OPENAI_BASE_URL` | `https://api.openai.com` | OpenAI endpoint override |
| `HF_BASE_URL` | `https://router.huggingface.co/v1` | Hugging Face endpoint override |
| `OPENROUTER_BASE_URL` | `https://openrouter.ai/api` | OpenRouter endpoint override |
| `OPENROUTER_REFERER` | none | Your site URL for OpenRouter leaderboard |
| `OPENROUTER_TITLE` | none | App name for OpenRouter leaderboard |
| `CLAUDE_CODE_BIN` | `claude` | Path to Claude Code CLI binary |
| `CLAUDE_CODE_PERMISSION_MODE` | `dontAsk` | Claude Code permission mode |
| `QDRANT_API_KEY` | none | Qdrant Cloud API key (when `backend = "qdrant"`) |
| `TENGU_TELEGRAM_ALLOWED_USERS` | none | Comma-separated Telegram user IDs (merged with config `allowed_users`) |
| `TENGU_GPU_HINT` | none | Force GPU detection: `"gpu"`, `"cuda"`, `"metal"`, `"none"`, `"cpu"` |
| `TENGU_RELAY_URL` | none | Cloudflare Worker relay URL (planned -- will inject API keys server-side) |
| `RUST_LOG` | `info` | Log level (trace, debug, info, warn, error) |

### Skill and Tool-Specific

| Variable | Required When | Notes |
|----------|--------------|-------|
| `PRIVY_APP_ID` | privy / aura-orchestrator [[Skills|skills]] | Privy app identifier from dashboard.privy.io |
| `PRIVY_APP_SECRET` | privy / aura-orchestrator [[Skills|skills]] | Privy secret key for API auth |
| `PRIVY_WALLET_ID` | aura-orchestrator [[Skills|skill]] | Privy agentic wallet ID for on-chain operations |
| `MOLECULE_API_KEY` | aura-orchestrator [[Skills|skill]] | Molecule DeSci Labs API key (sent as `x-api-key` header) |
| `MOLECULE_LABS_URL` | aura-orchestrator [[Skills|skill]] | GraphQL endpoint (e.g., `https://staging.graphql.api.molecule.xyz/graphql`) |
| `MOLECULE_CLIENT_URL` | aura-orchestrator | Client URL for project links (e.g., `https://testnet.molecule.xyz`) |
| `MOLECULE_SERVICE_TOKEN` | aura-orchestrator (Workflows 2-4) | Service token JWT for file uploads and announcements |
| `POI_API_KEY` | POI registration (Workflow 1) | Bearer token for `testnet.molecule.xyz/api/v1/inventions` |
| `EVM_RPC_URL` | [[DeSci]] minting (aura-orchestrator) | Sepolia RPC endpoint (e.g., `https://rpc.sepolia.org`) -- used for read-only chain queries |

---

## Config Validation Rules

The config is validated at startup. Invalid configs produce clear error messages.

**Global:**
- At least 1 agent must be defined
- At most 1 agent can have `default = true`
- `runtime_profile` must be one of: `auto`, `cloud`, `desktop`, `minimal`

**Per agent:**
- `engine` must be non-empty
- `model` must be non-empty
- `default_lens` must be one of: `eco`, `standard`, `precise`
- `flow.scope` must be one of: `main`, `per-group`, `per-pipe-sender`, `per-sender`
- `flow.reset_mode` must be one of: `idle`, `manual`, `time`
- `flow.compaction_threshold_ratio` must be in (0.0, 1.0] if set
- `flow.max_history_turns` must be > 0 if set
- `flow.compaction_keep_turns` must be > 0 if set
- `flow.compaction_summary_max_tokens` must be > 0 if set
- `limits.max_tokens_per_flow` must be > 0
- `limits.max_cost_per_flow` must be > 0.0 if set
- `limits.warn_at_cost` must be > 0.0 and <= `max_cost_per_flow` if set
- `limits.context_window_override` must be > 0 if set
- `limits.max_output_tokens_per_turn` must be > 0 and <= `context_window_override` if set
- `role` must be a non-empty string if set (any role name is valid)
- `lens.eco_max_tokens` must be > 0
- `lens.standard_threshold` must be in [0.0, 1.0]
- `lens.precise_budget` must be in [0.0, 1.0]
- `prompt_budget.max_file_tokens` must be > 0
- `prompt_budget.max_skill_context_tokens` must be > 0
- `prompt_budget.max_total_tokens` must be > 0
- `prompt_budget.max_file_tokens` must be <= `max_total_tokens`
- `prompt_budget.max_skill_context_tokens` must be <= `max_total_tokens`

---

## Docker Configuration

When running in Docker (via `docker-compose.yml` or `make up`), paths differ from native. See [[Deployment]] for Docker Compose profiles, GPU setup, and cloud provisioning.

| Native Path | Docker Path | Notes |
|-------------|-------------|-------|
| `~/.tengu/config.toml` | `/opt/tengu/config.toml` | Mounted from `./config.toml` (read-only) |
| `~/.tengu/` | `/opt/tengu/data` | Persistent volume `tengu-data` |

**Environment variables** are loaded from `.env` in the project root. Set API keys there instead of the secrets vault when using Docker.

**Ollama connection** -- when Ollama runs as a compose service, Tengu connects via Docker networking:

```toml
# config.toml — no changes needed, compose handles networking
[agents.local]
engine = "ollama"
model = "llama3.2"
```

The `OLLAMA_HOST` is set automatically by the compose network. When using native Ollama on macOS (for Metal GPU), set in `.env`:

```bash
OLLAMA_HOST=http://host.docker.internal:11434
```

**Hub bind address** -- for external access (cloud [[Deployment]]), change in `config.toml`:

```toml
[hub]
bind = "0.0.0.0"    # default: 127.0.0.1
```

---

## File Locations

| Path | Purpose |
|------|---------|
| `~/.tengu/config.toml` | Main configuration file |
| `~/.tengu/secrets.vault` | AES-256-GCM encrypted secrets vault |
| `~/.tengu/state/flows/` | Conversation history persistence |
| `~/.tengu/state/flows/index.json` | Flow metadata index |
| `~/.tengu/memory/vectors.bin` | Global disk [[Memory|memory]] store (bincode, when no workspace; overridden to `<workspace>/memory/vectors.bin` for sandboxed [[Agents|agents]]) |
| `~/.tengu/logs/tengu.log` | Runtime log file (in chat mode) |
| `skills/*.md` | [[Skills|Skill]] definitions (project root) |
| `{workspace}/IDENTITY.md` | Agent identity system prompt |
| `{workspace}/PROFILE.md` | Agent [[Capabilities|capabilities]] system prompt |
| `{workspace}/CONTEXT.md` | Domain context system prompt |

---

## Related

- [[Configuration]] -- overview, minimal config, and section summaries
- [[Agents]] -- agent model and lifecycle
- [[Capabilities]] -- permission system
- [[Skills]] -- skill packages and loading
- [[Tools]] -- tool definitions and approval
- [[Memory]] -- backend config and RAG pipeline
- [[Orchestrator]] -- multi-agent execution
- [[Channels]] -- adapter details
- [[Deployment]] -- Docker and cloud setup
- [[DeSci]] -- DeSci-specific environment variables
- [[Architecture]] -- hexagonal design
