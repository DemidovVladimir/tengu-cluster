---
tags:
  - reference
  - configuration
---

# Configuration

All configuration lives in a single TOML file at `~/.tengu/config.toml`. Copy `config.example.toml` from the repository as a starting point.

## Config Sources

| Source | Location | Priority |
|--------|----------|----------|
| Default | `~/.tengu/config.toml` | Base |
| CLI override | `--config <path>` | Higher |
| Sandbox | `sandboxes/<name>/config.toml` | Highest |
| Env vars | `.env` + shell + `secrets.vault` | Per-variable |

## Minimal Working Config

```toml
[agents.main]
default = true
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"
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

See [[Agents]] for the full agent model, [[Capabilities]] for permission details, and [[Skills]] for skill package loading.

For the detailed per-field reference of all agent sub-sections (Engine/Model, Identity, Flow, Limits, Lens, Prompt Budget, Role/Tools/Skills), see [[Configuration Reference]].

### Quick Example

```toml
[agents.main]
default = true
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"
workspace = "~/projects/my-app"
default_lens = "eco"

[agents.main.identity]
name = "Tengu"
instructions = "You are a Rust programming assistant. Be concise."

[agents.main.flow]
scope = "per-sender"
reset_mode = "idle"
idle_timeout_minutes = 30

[agents.main.limits]
max_tokens_per_flow = 100_000

[agents.main.lens]
eco_max_tokens = 100

[agents.main.prompt_budget]
max_file_tokens = 2000
max_skill_context_tokens = 4000
max_total_tokens = 8000
```

For fleet orchestration agents with roles:

```toml
[agents.qa]
engine = "openrouter"
model = "nvidia/nemotron-3-super-120b-a12b:free"
role = "qa"
capabilities = ["workspace.read", "workspace.list", "workspace.shell", "skill.search", "skill.test_runner"]
skill_packages = ["search", "test_runner"]
```

Roles are fully dynamic -- any non-empty string is valid. Define role-specific behavior through `identity.instructions`. See [[Orchestrator]] for fleet orchestration details.

---

## Orchestrator

Fleet management configuration. Only needed for multi-agent mode. See [[Orchestrator]] for the execution model.

```toml
[orchestrator]
enabled = true
max_retries = 3
```

| Field | Type | Default | Notes |
|-------|------|---------|-------|
| `enabled` | bool | `false` | Enable fleet orchestration |
| `max_retries` | u32 | `3` | Max retry attempts for failed tasks |

---

## Memory

Persistent vector memory enables cross-session knowledge retrieval. When enabled, the memory subsystem registers its own [[Tools]] (e.g., `remember`) and relevant memories are automatically recalled before each engine turn. See [[Memory]] for full backend details.

**How it works:**

1. **Remember**: user text -> `EmbeddingPort::embed()` -> `Vec<f32>` embedding -> `MemoryStorePort::store()` (persisted with content, agent ID, timestamp, and optional metadata tags)
2. **Recall**: each turn, the user's message is embedded -> `MemoryStorePort::search_by_vector()` finds the top-k closest memories by cosine similarity -> results are budget-trimmed to fit token limits -> injected as a `[Relevant memories]` system message before chat history
3. **Filtered recall**: the [[Orchestrator]] uses `recall_filtered()` to retrieve only memories matching specific metadata (e.g., `kind=topic_overview, source=orchestrator`), preventing unrelated memories from polluting planner context

Both operations use the same embedding model, guaranteeing that stored vectors and query vectors share the same embedding space.

**Per-workspace isolation**: When a sandbox agent has a `workspace` configured, memory is automatically stored in `<workspace>/memory/` instead of the global `~/.tengu/memory/`. For Qdrant, a workspace-scoped collection name is used (e.g., `tengu-memory-desci-sandbox`). No config changes needed -- isolation is automatic.

**Metadata tagging**: Each memory entry can carry `metadata: HashMap<String, String>` with convention keys like `kind` (fact, outcome, topic_overview), `source` (user, agent, orchestrator), `goal`, `workspace_id`, and `run_id`. The `remember` tool accepts an optional `metadata` object parameter so agents can tag memories at storage time. Old `vectors.bin` files deserialize with empty metadata (backward compatible via `#[serde(default)]`).

**Orchestrator topic overviews**: After each multi-agent run, the [[Orchestrator]] automatically compresses all step results into a topic overview and stores it in memory with `{kind: "topic_overview", source: "orchestrator", goal: "<goal>", workspace_id: "<ws>"}`. Before planning new goals, the orchestrator recalls prior topic overviews (filtered by `kind` + `source`) and injects them as "Relevant Prior Work" context for the planner.

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
| `store_path` | string | `"~/.tengu/memory/"` | Disk backend storage directory (overridden to `<workspace>/memory/` when agent has a workspace) |
| `backend` | string | `"disk"` | `"disk"` or `"qdrant"` |
| `qdrant_url` | string | `"http://localhost:6334"` | Qdrant gRPC endpoint |
| `qdrant_api_key` | string? | none | API key for Qdrant Cloud |
| `qdrant_collection` | string | `"tengu-memory"` | Qdrant collection name (auto-scoped to `tengu-memory-<workspace>` when agent has a workspace) |
| `vector_size` | u64 | `1536` | Embedding dimensionality (must match model) |

Requires `OPENROUTER_API_KEY` for embedding generation.

### Disk Backend (default)

Zero-config. Stores all entries in memory with bincode persistence to `{store_path}/vectors.bin`. Search is brute-force cosine similarity -- sufficient for hundreds to low thousands of memories.

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

Run the agent as a headless Telegram bot. Users chat with the bot in Telegram and get full agent [[Capabilities]]: [[Tools]], [[Skills]], [[Memory]], secret redaction, file attachments, and inline keyboard tool approval. See [[Channels]] for adapter details.

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

**Multi-agent routing:** All configured [[Agents]] are loaded at startup. Route messages to specific agents with `@role: message` (e.g., `@backend_engineer: add rate limiting`). In multi-agent Telegram mode, unrouted plain messages go through the team [[Orchestrator]] automatically. Use `/agents` to list available agents. When using sandboxes (`tengu telegram --sandbox webstudio`), all sandbox agents are available.

**Per-user conversations:** Each Telegram user gets their own `ChatLoopState` per agent (keyed by sender + agent ID), following the agent's configured `flow.scope` setting.

**File attachments:** Documents and photos sent to the bot are downloaded and saved to `{workspace}/.tengu-attachments/`. File paths are prepended to the message content so the agent can reference them. Text for media messages comes from the caption field. Multiple files sent as a media group (e.g., PDF + image together) are automatically buffered and merged into a single message.

**Typing indicator:** A "typing..." chat action is sent every 4 seconds during processing, running as an independent task so it stays alive during long synchronous tool execution.

**Tool approval:** [[Tools]] with `requires_approval: true` in their policy metadata trigger an inline keyboard message with Approve / Deny buttons. The agent blocks until the user responds or the 60-second timeout expires (auto-deny on timeout). Approval dialogs are generated generically from tool metadata (risk level, description), not hardcoded per tool name.

**Token budget warnings:** When a conversation reaches 80% of `max_tokens_per_flow`, a warning message is sent showing current usage and remaining budget. At 100%, further requests are blocked until `/reset`.

**Slash commands in Telegram:** The bot supports the same slash commands as TUI mode (`/help`, `/cost`, `/reset`, `/purge`, `/reload`, `/skills`, etc.) plus Telegram-specific commands.

`/purge` clears conversation state, wipes persistent [[Memory]], and cleans workspace disk artifacts (`.tengu-tasks/`, `.tengu-attachments/`). Use the CLI equivalent `tengu prune --sandbox <name>` to also clean global state (flows, logs, global memory).

**Telegram-specific commands:**

| Command | Description |
|---------|-------------|
| `/team <goal>` | Explicitly decompose goal into tasks with dependencies, execute in parallel batches |
| `/project <name>` | Create a new project subfolder, switch all agents to it |
| `/agents` | List available agents and their roles |
| `/stop` | Cancel the current operation (works during `/team` orchestration) |
| `/wallet` | Show Privy wallet address and balance |
| `/wallet status` | Show wallet ID, address, balance, and policy |

### Wallet

The `/wallet` and `/wallet status` commands query the Privy agentic wallet API to show wallet address and balance.

**[[DeSci]] minting** uses Privy agentic wallets for all on-chain signing. Alloy is used for ABI encoding only, not transaction signing. See [[Wallet]] for full details.

**Privy setup** (optional):
1. Create a Privy app at [dashboard.privy.io](https://dashboard.privy.io)
2. Store credentials: `cargo run -- secret set PRIVY_APP_ID ...` and `cargo run -- secret set PRIVY_APP_SECRET ...`
3. Store the wallet ID: `cargo run -- secret set PRIVY_WALLET_ID ...`

**Prerequisites for `/wallet`:** Set `PRIVY_APP_ID`, `PRIVY_APP_SECRET`, and `PRIVY_WALLET_ID` environment variables.

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

**Warning:** Setting `TENGU_MASTER_PASSWORD` in your shell profile or `.env` file reduces security -- anyone with access to those files can decrypt your vault. Prefer the interactive prompt for local use and reserve the env var for CI/automation where the value is injected securely (e.g., from a CI secrets manager).

### Vault File Format

Binary format: `TENGU_VAULT\x01` magic header (12 bytes) + 32-byte PBKDF2 salt + 12-byte AES-GCM nonce + ciphertext with 16-byte GCM authentication tag. The decrypted plaintext is simple `KEY=VALUE\n` pairs.

---

## Sandboxes

Domain-specific team configs in `sandboxes/<name>/config.toml`:

- `sandboxes/desci/` -- [[DeSci]] minting team
- `sandboxes/webstudio/` -- web development team

```bash
cargo run -- telegram --sandbox desci
cargo run -- orchestrate --sandbox webstudio
```

---

## Related

- [[Configuration Reference]] -- detailed per-field reference tables for all agent sub-sections, environment variables, validation rules, Docker config, and file locations
- [[Agents]] -- agent model and lifecycle
- [[Capabilities]] -- permission config
- [[Skills]] -- `skill_packages` filtering
- [[Tools]] -- tool definitions and approval
- [[Memory]] -- backend config and RAG pipeline
- [[Orchestrator]] -- multi-agent execution model
- [[Channels]] -- adapter details (Telegram, TUI)
- [[Deployment]] -- runtime setup and Docker
- [[Architecture]] -- project structure
