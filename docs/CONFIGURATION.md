# Configuration Reference

All configuration lives in a single TOML file at `~/.tengu/config.toml`. Copy `config.example.toml` from the repository as a starting point.

## Table of Contents

- [Minimal Working Config](#minimal-working-config)
- [Root Settings](#root-settings)
- [Hub](#hub)
- [Refiner](#refiner)
- [Agents](#agents)
  - [Engine and Model](#engine-and-model)
  - [Identity](#identity)
  - [Flow (Session Behavior)](#flow-session-behavior)
  - [Limits](#limits)
  - [Lens](#lens)
  - [Role and Skills](#role-and-skills)
- [Orchestrator](#orchestrator)
- [Memory](#memory)
  - [Disk Backend (default)](#disk-backend-default)
  - [Qdrant Backend](#qdrant-backend)
- [Telegram](#telegram)
- [Environment Variables](#environment-variables)
- [Config Validation Rules](#config-validation-rules)

---

## Minimal Working Config

```toml
[agents.main]
default = true
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
```

This is enough to run `cargo run -- chat`. Everything else has sensible defaults.

---

## Root Settings

```toml
runtime_profile = "auto"
```

| Field | Type | Default | Options |
|-------|------|---------|---------|
| `runtime_profile` | string | `"auto"` | `"auto"`, `"cloud"`, `"desktop"`, `"minimal"` |

**Profiles control runtime behavior heuristics:**

| Profile | When Selected (auto) | Behavior |
|---------|---------------------|----------|
| `cloud` | RAM > 16 GB + GPU detected | Full resource usage |
| `desktop` | RAM > 4 GB | Standard resource usage |
| `minimal` | RAM < 4 GB | Reduced buffers and concurrency |

**GPU auto-detection checks** (in order):
1. `TENGU_GPU_HINT` env var (`"gpu"`, `"cuda"`, `"metal"`, `"mps"`, `"true"` = yes)
2. `CUDA_VISIBLE_DEVICES` set = yes
3. macOS + ARM64 (Apple Silicon) = yes
4. Otherwise = no

---

## Hub

Network and reload configuration.

```toml
[hub]
bind = "127.0.0.1"
port = 7070
auth_mode = "token"

[hub.reload]
mode = "hybrid"
debounce_ms = 300
```

| Field | Type | Default | Options |
|-------|------|---------|---------|
| `bind` | string | `"127.0.0.1"` | Any valid IP address |
| `port` | u16 | `7070` | Must be > 0 |
| `auth_mode` | string | `"token"` | `"token"`, `"open"` |
| `auth_token` | string? | none | Required when `auth_mode = "token"` and hub is exposed |
| `reload.mode` | string | `"hybrid"` | `"hybrid"`, `"hot"`, `"restart"`, `"off"` |
| `reload.debounce_ms` | u64 | `300` | Milliseconds to wait before applying config changes |

---

## Refiner

Prompt optimization and history summarization engine.

```toml
[refiner]
mode = "off"
```

| Field | Type | Default | Options |
|-------|------|---------|---------|
| `mode` | string | `"off"` | `"off"`, `"rules"`, `"local"`, `"remote"` |
| `model` | string? | none | Required when `mode = "local"` |
| `url` | string? | none | Required when `mode = "remote"` |

**Mode details:**

| Mode | What It Does | Requirements |
|------|-------------|-------------|
| `off` | No prompt refinement or compaction summarization | None |
| `rules` | Built-in rule-based compression (filler removal, extractive summary) | None |
| `local` | Use a local model for summarization | `model` must be set |
| `remote` | Call a remote HTTP service for summarization | `url` must be set |

---

## Agents

Each agent is defined under `[agents.<AGENT_ID>]`. You need at least one agent. At most one agent can have `default = true`.

### Engine and Model

```toml
[agents.main]
default = true
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
workspace = "~/projects/my-app"
default_lens = "eco"
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `engine` | string | (required) | See engine table below |
| `model` | string | (required) | Provider-specific model ID |
| `default` | bool | `false` | At most one agent can be default |
| `workspace` | string? | none | Path to local workspace for file tools |
| `default_lens` | string | `"eco"` | `"eco"`, `"standard"`, `"precise"` |

**Supported engines:**

| Engine | API Style | Model ID Format | Example |
|--------|----------|----------------|---------|
| `openrouter` | OpenAI-compatible | `provider/model` | `anthropic/claude-sonnet-4` |
| `anthropic` | Anthropic native | Anthropic model ID | `claude-sonnet-4-20250514` |
| `openai` | OpenAI native | OpenAI model ID | `gpt-4o` |
| `ollama` | Ollama HTTP | Ollama model name | `llama3.2` |
| `huggingface` | OpenAI-compatible | `org/model:variant` | `THUDM/GLM-4.7:fastest` |
| `claude-code` | Subprocess CLI | Claude model ID | `claude-sonnet-4-5-20250929` |

**OpenRouter model examples:**

```toml
model = "anthropic/claude-sonnet-4"        # Claude Sonnet 4
model = "openai/gpt-4o"                    # GPT-4o
model = "google/gemini-2.5-pro"            # Gemini 2.5 Pro
model = "meta-llama/llama-4-maverick"      # Llama 4 Maverick
model = "mistralai/mistral-large"          # Mistral Large
model = "deepseek/deepseek-chat-v3"        # DeepSeek V3
```

### Identity

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
5. **Workspace tools**: tool descriptions when the backend supports runtime tool use

### Flow (Session Behavior)

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
| `per-pipe-sender` | `{agent}:{pipe}:{peer_id}` | One conversation per user per channel |
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

### Limits

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
| `max_tokens_per_flow` | u64 | `100_000` | Hard token limit for entire conversation flow |
| `context_window_override` | u32? | none | Override engine-reported context window size |
| `max_output_tokens_per_turn` | u32? | none | Cap output tokens per engine turn |
| `max_cost_per_flow` | f64? | none | USD cost limit for the flow |
| `warn_at_cost` | f64? | none | USD threshold for cost warning (must be <= `max_cost_per_flow`) |

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

### Lens

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

### Role and Skills

For fleet orchestration agents:

```toml
[agents.qa]
engine = "openrouter"
model = "anthropic/claude-sonnet-4"
role = "qa"
skills = ["search", "test_runner"]
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `role` | string? | none | `"qa"`, `"backend_engineer"`, `"integration_master"` |
| `skills` | string[]? | none | Allowlist of skill names. Omit for all skills. |

See [Fleet Orchestration Guide](FLEET.md) for role details.

---

## Orchestrator

Fleet management configuration. Only needed for multi-agent mode.

```toml
[orchestrator]
enabled = true
heartbeat_interval_s = 30
max_retries = 3
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `enabled` | bool | `false` | Enable fleet orchestration |
| `heartbeat_interval_s` | u64 | `30` | Seconds between heartbeat checks |
| `max_retries` | u32 | `3` | Max retry attempts for failed tasks |

---

## Memory

Persistent vector memory enables cross-session knowledge retrieval. When enabled, agents can store facts/insights via the `remember` tool and relevant memories are automatically recalled before each engine turn.

**How it works:**

1. **Remember**: user text → `EmbeddingPort::embed()` → `Vec<f32>` embedding → `MemoryStorePort::store()` (persisted with content, agent ID, timestamp)
2. **Recall**: each turn, the user's message is embedded → `MemoryStorePort::search_by_vector()` finds the top-k closest memories by cosine similarity → results are budget-trimmed to fit token limits → injected as a `[Relevant memories]` system message before chat history

Both operations use the same embedding model, guaranteeing that stored vectors and query vectors share the same embedding space.

```toml
[memory]
enabled = true
embedding_model = "text-embedding-3-small"
max_recall_entries = 5
max_recall_tokens = 600
store_path = "~/.tengu/memory/"
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `enabled` | bool | `false` | Enable the memory subsystem |
| `embedding_model` | string | `"text-embedding-3-small"` | OpenRouter embedding model ID |
| `embedding_provider` | string | `"openrouter"` | Embedding API provider |
| `max_recall_entries` | usize | `5` | Max memories to retrieve per turn |
| `max_recall_tokens` | usize | `600` | Token budget for recalled memories |
| `store_path` | string | `"~/.tengu/memory/"` | Disk backend storage directory |
| `backend` | string | `"disk"` | `"disk"` or `"qdrant"` |
| `qdrant_url` | string | `"http://localhost:6334"` | Qdrant gRPC endpoint |
| `qdrant_api_key` | string? | none | API key for Qdrant Cloud |
| `qdrant_collection` | string | `"tengu-memory"` | Qdrant collection name |
| `vector_size` | u64 | `1536` | Embedding dimensionality (must match model) |

Requires `OPENROUTER_API_KEY` for embedding generation.

### Disk Backend (default)

Zero-config. Stores all entries in memory with bincode persistence to `{store_path}/vectors.bin`. Search is brute-force cosine similarity — sufficient for hundreds to low thousands of memories.

### Qdrant Backend

Requires `cargo build --features qdrant` and a running Qdrant instance.

```bash
# Start Qdrant
docker run -p 6333:6333 -p 6334:6334 qdrant/qdrant
```

```toml
[memory]
enabled = true
backend = "qdrant"
qdrant_url = "http://localhost:6334"
qdrant_collection = "tengu-memory"
vector_size = 1536
# qdrant_api_key = "${QDRANT_API_KEY}"  # for Qdrant Cloud
```

The collection is auto-created with cosine distance on first connect. If the `qdrant` feature is not compiled in but `backend = "qdrant"` is set, the system logs a warning and falls back to the disk backend.

**Common embedding models and their vector sizes:**

| Model | Dimensions | Notes |
|-------|-----------|-------|
| `text-embedding-3-small` | 1536 | Default, good cost/quality balance |
| `text-embedding-3-large` | 3072 | Higher quality, 2x storage |
| `text-embedding-ada-002` | 1536 | Legacy OpenAI model |

---

## Telegram

Run the agent as a headless Telegram bot. Users chat with the bot in Telegram and get full agent capabilities: tools, skills, memory, secret redaction.

```toml
[telegram]
enabled = true
allowed_users = ["123456789", "987654321"]
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `enabled` | bool | `false` | Enable Telegram bot mode |
| `allowed_users` | string[] | `[]` | Telegram user IDs allowed to interact with the bot |

**Setup:**
1. Create a bot via [@BotFather](https://t.me/BotFather) on Telegram to get a bot token
2. Store the token: `tengu secret set TELEGRAM_BOT_TOKEN 123456:ABC-DEF...`
3. Get your Telegram user ID from [@userinfobot](https://t.me/userinfobot)
4. Add your user ID to `allowed_users` in config (or set `TENGU_TELEGRAM_ALLOWED_USERS` env var, comma-separated)
5. Run: `cargo run -- telegram`

**Access control:** If `allowed_users` is empty and `TENGU_TELEGRAM_ALLOWED_USERS` is not set, all messages are rejected. Both sources are merged into a single allowlist.

**Message chunking:** Telegram has a 4096-character message limit. Long responses are automatically split at paragraph (`\n\n`) boundaries into chunks of at most 4000 characters.

**Per-user conversations:** Each Telegram user gets their own `ChatLoopState` (keyed by sender user ID), following the agent's configured `flow.scope` setting.

---

## Secrets Vault

API keys and other secrets are stored in an **AES-256-GCM encrypted vault** at `~/.tengu/secrets.vault`, protected by a master password (PBKDF2-HMAC-SHA256, 600,000 iterations).

### Setup

```bash
cargo run -- secret init                              # create vault, prompts for password twice
cargo run -- secret set OPENROUTER_API_KEY sk-or-...  # prompts for password
cargo run -- secret list                              # prompts for password, shows key names
cargo run -- secret remove OPENROUTER_API_KEY         # prompts for password
cargo run -- secret change-password                   # change the vault master password
cargo run -- secret path                              # prints vault file path
```

### Choosing a Master Password

The master password protects all your secrets (API keys, tokens, etc.) with a single encryption key. Choose it carefully:

- **At least 12 characters** (8 minimum enforced, 12+ strongly recommended)
- **Mix character types**: uppercase, lowercase, numbers, symbols
- **Do NOT reuse** a password from another service
- **Use a password manager** (1Password, Bitwarden, KeePass) to generate and store it
- **Avoid** dictionary words, personal info, or common patterns like `Password123!`

**Good examples:** `kT9#mPx$vR2nLq7!`, a random passphrase like `correct-horse-battery-staple`

You can change the master password at any time without losing your secrets:

```bash
cargo run -- secret change-password
```

### Startup Behavior

At startup, if `~/.tengu/secrets.vault` exists, Tengu prompts for the master password and injects all stored key-value pairs into the process environment. Existing env vars are not overwritten (shell env > vault > `.env`).

Set the `TENGU_MASTER_PASSWORD` env var to skip the interactive prompt (useful for CI/scripts):

```bash
TENGU_MASTER_PASSWORD=mypass cargo run -- doctor
```

**Warning:** Setting `TENGU_MASTER_PASSWORD` in your shell profile or `.env` file reduces security — anyone with access to those files can decrypt your vault. Prefer the interactive prompt for local use and reserve the env var for CI/automation where the value is injected securely (e.g., from a CI secrets manager).

### Vault File Format

Binary format: `TENGU_VAULT\x01` magic header (12 bytes) + 32-byte PBKDF2 salt + 12-byte AES-GCM nonce + ciphertext with 16-byte GCM authentication tag. The decrypted plaintext is simple `KEY=VALUE\n` pairs.

---

## Environment Variables

All environment variables. Export them in your shell, `direnv`, process manager, or store them in the encrypted secrets vault.

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
| `RUST_LOG` | `info` | Log level (trace, debug, info, warn, error) |

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
- `role` must be one of: `qa`, `backend_engineer`, `integration_master` if set
- `lens.eco_max_tokens` must be > 0
- `lens.standard_threshold` must be in [0.0, 1.0]
- `lens.precise_budget` must be in [0.0, 1.0]

---

## File Locations

| Path | Purpose |
|------|---------|
| `~/.tengu/config.toml` | Main configuration file |
| `~/.tengu/secrets.vault` | AES-256-GCM encrypted secrets vault |
| `~/.tengu/state/flows/` | Conversation history persistence |
| `~/.tengu/state/flows/index.json` | Flow metadata index |
| `~/.tengu/memory/vectors.bin` | Disk memory store (bincode, when `backend = "disk"`) |
| `~/.tengu/logs/tengu.log` | Runtime log file (in chat mode) |
| `skills/*.md` | Skill definitions (project root) |
| `{workspace}/IDENTITY.md` | Agent identity system prompt |
| `{workspace}/PROFILE.md` | Agent capabilities system prompt |
| `{workspace}/CONTEXT.md` | Domain context system prompt |
