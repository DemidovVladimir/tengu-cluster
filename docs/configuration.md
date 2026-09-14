# Configuration

One TOML file. Resolution: `--sandbox <name>` loads `sandboxes/<name>/config.toml` and replaces the base config wholesale; without it, `-c/--config <path>` > `TENGU_CONFIG=<path>` > `<TENGU_HOME>/config.toml` (`TENGU_HOME` defaults to `~/.tengu`). The `run-agent` child follows the same chain. Commented reference: `config.example.toml`.

## Initial setup

```bash
tengu secret init                              # vault + master password
tengu secret set OPENROUTER_API_KEY sk-or-...
tengu secret set TELEGRAM_BOT_TOKEN 123:ABC-..
mkdir -p ~/.tengu && cp config.example.toml ~/.tengu/config.toml
tengu chat                                     # or: tengu chat --sandbox aura
```

`TENGU_MASTER_PASSWORD` skips the vault prompt.

## Root sections

| Section | Purpose |
|---|---|
| `runtime_profile` | `"auto"` / `"cloud"` / `"desktop"` / `"minimal"` |
| `[hub]` | Bind, port, auth mode, hot-reload (config-only — nothing listens on `[hub].port`) |
| `[agents.<id>]` | Per-agent configuration |
| `[orchestrator]` | Planner + subprocess runner (see below) |
| `[memory]` | Disk vector store + agentic-memory knobs |
| `[telegram]` | Telegram bot adapter |
| `[webhooks]` | Inbound HTTP listener (feature `webhooks`) — `docs/webhooks-2026-05-11.md` |
| `[scaffold]` | Workspace directory/file scaffolding |
| `[claude_code]` | Global Claude Code backend settings |
| `[skill_lifecycle]` | `tengu eval` / `tengu skill metrics` / `tengu skill evolve` |

## Agent configuration

```toml
[agents.main]
engine = "openrouter"              # "openrouter" | "claude_code"
model = "anthropic/claude-sonnet-4-6"   # claude_code wants the bare slug: "claude-sonnet-4-6"
default = true                     # at most one default agent
workspace = "~/projects/my-app"
default_lens = "eco"               # "eco" | "standard" | "precise"
role = "backend_engineer"          # optional
skill_packages = ["aura-orchestrator"]
workspace_tools = ["shared_cache", "skill_distill"]
```

| Field | Notes |
|---|---|
| `engine` | `openrouter` needs `OPENROUTER_API_KEY`; `claude_code` needs the `claude` CLI + `--features claude_code`. See [[engine-backends]]. |
| `workspace_tools` | Allow-list: `agentic_memory`, `shared_cache`, `persistent_store`, `skill_distill`, `apply_improver_proposal`, `manage_skill` (`config.rs::valid_workspace_tools`). |
| Subagent specs | `agents/<name>.toml` (`AgentSpec`) has NO `workspace_tools` field — put opt-ins in `tools = [...]`. |

### Identity / Flow / Limits

```toml
[agents.main.identity]
name = "My Agent"
instructions = "You are a helpful assistant."

[agents.main.flow]
scope = "per-sender"                # "main" | "per-group" | "per-pipe-sender" | "per-sender"
reset_mode = "idle"                 # "idle" | "manual" | "time"
idle_timeout_minutes = 30
max_history_turns = 20              # optional
compaction_threshold_ratio = 0.82   # optional (0.0, 1.0]
compaction_keep_turns = 24          # optional
compaction_summary_max_tokens = 320 # optional

[agents.main.limits]
max_tokens_per_flow = 100_000      # hard limit (warning at 80%)
context_window = 1_000_000
max_tool_rounds = 70
max_tool_result_chars = 300_000
stream_event_timeout_secs = 120
compact_result_limit = 200
max_output_tokens_per_turn = 4096  # optional (<= context_window)
max_cost_per_flow = 5.0            # optional (USD)
warn_at_cost = 4.0                 # optional (<= max_cost_per_flow)
```

### Claude Code

```toml
[claude_code]
cli_path = "claude"

[agents.main.claude_code]
builtin_tools_profile = "editor_shell"  # "none" | "read_only" | "editor" | "editor_shell"
```

Don't put a `claude_code` agent in the planner role unless you accept that the CLI keeps MCP tool access (planner-side tool stripping only works for OpenRouter) — see `CLAUDE.md` gotchas.

## Orchestrator

Schema: `src/adapters/config.rs::OrchestratorConfig`.

```toml
[orchestrator]
agent = "aura"                 # agent (in [agents.*]) that runs the planner LLM call
engine = "rag"                 # REQUIRED — only supported value (historical name; file-registry planner)
max_attempts_per_step = 3      # Tier 1: retries per step before escalation
max_replans = 2                # Tier 2: replans before bailing out
route_explicit_agents = false  # true = `@role:` messages also go through the planner
```

| Fact | Where |
|---|---|
| `engine` defaults to `"static"`, which logs a warn and disables orchestration | `channel_runtime.rs` (`cfg.engine != "rag"`) |
| Subagents are `agents/<name>.toml`; registry regenerated into `TENGU_PLANNER_REGISTRY.md` each planner turn | `orchestrator/shared_files.rs` |
| Accepted plan written to `TENGU_PLAN.md` for subagents | `orchestrator/replan.rs` |

## Memory

```toml
[memory]
enabled = true
embedding_model = "text-embedding-3-small"   # must stay 1536-dim (Postgres schema hardcodes vector(1536))
max_recall_entries = 5
max_recall_tokens = 600
store_path = "~/.tengu/memory/"
backend = "disk"                              # only built-in backend (bincode on disk)
session_recent_n = 10
cross_plan_top_k = 5
within_session_output_top_k = 3               # 0 = off; needs postgres_memory
persistent_store_chunk_size = 1000
persistent_store_chunk_overlap = 200
```

Durable cross-session memory is the Postgres `agentic_memory` plugin: build with `--features postgres_memory`, set `TENGU_MEMORY_DATABASE_URL`. Spec: `docs/agentic-memory-*-2026-05-13.md`. Embeddings need `OPENROUTER_API_KEY` regardless of engine.

## Telegram

```toml
[telegram]
enabled = true
allowed_users = ["123456789"]    # merged with TENGU_TELEGRAM_ALLOWED_USERS
```

## Skill lifecycle

```toml
[skill_lifecycle]
improver_agent       = "skill-improver"   # required for `tengu skill evolve`
fixture_runner_agent = "fixture-runner"   # serde-required but unused — `tengu eval` runs rows on the eval config default agent
default_max_evolve_cycles = 3
default_rolling_window    = 10

[agents.skill-improver]
engine = "openrouter"
model  = "anthropic/claude-opus-4-7"
tools  = ["read_file", "list_directory"]
```

See [[skills#Metrics & Evolution]] for the frontmatter contract.

## Sandboxes

`sandboxes/<name>/config.toml` — `aura` (DeSci pipeline), `storage-test`, `unlimited` (OpenRouter DeepSeek model bench, see `sandboxes/unlimited/BENCH.md`).

```bash
tengu chat --sandbox aura
tengu telegram --sandbox aura
cargo run --features claude_code -- chat --sandbox aura   # aura's agents use engine = "claude_code"
```

## Environment variables

| Var | Read by | Default | Purpose |
|---|---|---|---|
| `OPENROUTER_API_KEY` | `engine_builder.rs`, `orchestrator/planner.rs`, `mcp_bridge.rs`, `main.rs` | — (required) | OpenRouter chat + embeddings |
| `OPENROUTER_BASE_URL` | `engine_builder.rs`, `plugins/agentic_memory/mod.rs` | `https://openrouter.ai/api` | Override API base |
| `OPENROUTER_REFERER` | `engine_builder.rs` | unset | `HTTP-Referer` header |
| `OPENROUTER_TITLE` | `engine_builder.rs` | unset | `X-Title` header |
| `TELEGRAM_BOT_TOKEN` | `telegram_builder.rs` | — (required for `telegram`) | Bot token |
| `TENGU_TELEGRAM_ALLOWED_USERS` | `telegram_builder.rs::build_allowed_users` | unset | Comma-separated user ids merged with `[telegram].allowed_users` |
| `TENGU_HOME` | `main.rs::resolve_tengu_home` | `~/.tengu` | State root (config, vault, logs, memory) |
| `TENGU_CONFIG` | `main.rs` (config resolution) | `~/.tengu/config.toml` | Path to config.toml (`-c/--config` wins) |
| `TENGU_MASTER_PASSWORD` | `main.rs`, `secret_builder.rs` | unset (prompt) | Vault password |
| `TENGU_SESSION_ID` | `channel_runtime.rs::build_orchestrator`, `claude_code_engine.rs`, `plugins/agentic_memory/mod.rs`, `memory/vector/embedder.rs` | fresh UUID | Pin the session id shared by planner + runner |
| `TENGU_MEMORY_DATABASE_URL` | `plugins/agentic_memory/mod.rs` | — (required for `postgres_memory`) | Postgres + pgvector DSN |
| `TENGU_WIKI_COMPILER_MODEL` | `plugins/agentic_memory/mod.rs::wiki_compiler_model` | `anthropic/claude-sonnet-4-6` | `compile_wiki` model |
| `TENGU_TUI_METRICS` | `tui/mod.rs` | off | Token/latency status line in the TUI |
| `TENGU_TUI_RAG_DEBUG` | `tui/mod.rs` | off | Planner recall hits as a System bubble |
| `TENGU_GPU_HINT` | `config.rs::detect_gpu` | auto-detect | `none|cpu|off|false` or `gpu|cuda|metal|mps|on|true` for `runtime_profile = "auto"` |
| `TENGU_PERSISTENT_STORE_CHUNK_SIZE` | `claude_code_engine.rs` (forwarded to the MCP bridge child) | `[memory].persistent_store_chunk_size` | Chunk size for `persistent_store` |
| `TENGU_PERSISTENT_STORE_CHUNK_OVERLAP` | `claude_code_engine.rs` (forwarded to the MCP bridge child) | `[memory].persistent_store_chunk_overlap` | Chunk overlap for `persistent_store` |
| `CHAIN_ID` | `plugins/crypto/helpers.rs::resolve_default_chain_id` | `DEFAULT_CHAIN_ID` | Chain id when a tool call omits it |
| `PRIVY_APP_ID` | `plugins/crypto/helpers.rs` | — | Privy agentic wallet |
| `PRIVY_APP_SECRET` | `plugins/crypto/helpers.rs` | — | Privy agentic wallet |
| `PRIVY_WALLET_ID` | `plugins/crypto/helpers.rs` | — | Privy agentic wallet |
| `EVM_RPC_URL` | `plugins/crypto/helpers.rs` | `https://ethereum-rpc.publicnode.com` | JSON-RPC endpoint |
| `RUST_LOG` | `main.rs` (`tracing_subscriber::EnvFilter`) | `info` | Log filter; `tengu=info` prints one `metrics` line per LLM call |

## Feature flags

| Flag | Default | Purpose |
|---|---|---|
| `openrouter` | on | OpenRouter API backend |
| `telegram` | on | Telegram bot channel |
| `claude_code` | off | Claude Code CLI backend |
| `postgres_memory` | off | Postgres + pgvector agentic memory, `tengu agentic-memory-server` |
| `webhooks` | off | `tengu webhooks` inbound HTTP listener |

## Validation

- Engine must be `openrouter` or `claude_code`; `builtin_tools_profile` must be `none|read_only|editor|editor_shell`.
- Limits positive; `warn_at_cost <= max_cost_per_flow`; `max_output_tokens_per_turn <= context_window`.
- `workspace_tools` entries must be in the allow-list above.

## Reset

```bash
rm -rf ~/.tengu              # everything
rm -rf ~/.tengu/state        # sessions only
tengu prune                  # cached/ephemeral state (keeps config + secrets)
```

## Related
- [[architecture]] · `docs/architecture-2026-04-27.md` (canonical)
- [[engine-backends]] · [[skills]] · `docs/webhooks-2026-05-11.md`
