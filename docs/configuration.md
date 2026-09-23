# Configuration

One TOML file — channel settings, `[egress]`, the planner and **every agent** (in-process and subagent alike). Resolution: `--sandbox <name>` loads `sandboxes/<name>/config.toml` and replaces the base config wholesale; without it, `-c/--config <path>` > `TENGU_CONFIG=<path>` > `<TENGU_HOME>/config.toml` (`TENGU_HOME` defaults to `~/.tengu`). The `run-agent` child gets the same file (`--sandbox` over IPC; otherwise `TENGU_CONFIG`, which the parent pins to its resolved path) and takes `[agents.<name>]` from it. Commented reference: `config.example.toml`. Code side (schema structs, defaults, validation, who reads each section, how to add a field): `docs/code-map.md` §3–§4.

## Initial setup

```bash
tengu secret init                              # vault + master password
tengu secret set OPENROUTER_API_KEY sk-or-...
tengu secret set TELEGRAM_BOT_TOKEN 123:ABC-..
mkdir -p ~/.tengu && cp config.example.toml ~/.tengu/config.toml
make tor                                       # Tor proxy (default network); or set [egress] network = "open"
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
| `[webhooks]` | Inbound HTTP listener (feature `webhooks`); each POST runs a one-shot turn through `[orchestrator].agent` (the endpoint `agent` is informational) — `docs/webhooks-2026-05-11.md` |
| `[scaffold]` | Workspace directory/file scaffolding |
| `[claude_code]` | Global Claude Code backend settings |
| `[skill_lifecycle]` | `tengu eval` / `tengu skill metrics` / `tengu skill evolve` |
| `[egress]` | `network = "tor"` (default) / `"open"`, proxy, host allowlist, shell isolation, audit log — `docs/egress-2026-09-16.md` |

## Agent configuration

```toml
[agents.main]
engine = "openrouter"              # "openrouter" | "claude_code"
model = "anthropic/claude-sonnet-4-6"   # claude_code wants the bare slug: "claude-sonnet-4-6"
default = true                     # at most one default agent
workspace = "~/projects/my-app"
default_lens = "eco"               # "eco" | "standard" | "precise"
role = "backend_engineer"          # optional
skill_packages = ["aura-orchestrator"]   # `skills = [...]` is an accepted alias
workspace_tools = ["shared_cache", "skill_distill"]
# --- subagent view (planner-routable when `description` is set) ---
description = "What this agent handles and what it is NOT for — read by the planner LLM."
example_queries = ["what is the BTC price?"]
tools = ["http_request", "read_file"]   # allow-list for `tengu run-agent` steps; empty = every base tool
```

| Field | Notes |
|---|---|
| `engine` | `openrouter` needs `OPENROUTER_API_KEY`; `claude_code` needs the `claude` CLI + `--features claude_code`. See [[engine-backends]]. |
| `workspace_tools` | Allow-list: `agentic_memory`, `shared_cache`, `persistent_store`, `skill_distill`, `apply_improver_proposal`, `manage_skill` (`domain/tools.rs::WORKSPACE_TOOLS`). |
| `description` | Present ⇒ rendered into `TENGU_PLANNER_REGISTRY.md`; the planner may dispatch plan steps to this agent as a `tengu run-agent` subprocess. Absent ⇒ in-process only (planner role, `@role:` chat). |
| `tools` | Subprocess tool allow-list. Names from the workspace-tools allow-list listed here are opted in like `workspace_tools`. `compress_and_store` is appended implicitly — never list it. |
| Unknown keys | `AgentConfig` is not strict — a typo is silently ignored. Check `tengu status`. |

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
stream_event_timeout_secs = 120    # idle gap between stream events
request_timeout_secs = 600         # total budget for one non-streaming HTTP call (body read included)
compact_result_limit = 200
max_output_tokens_per_turn = 4096  # optional (<= context_window); unset = omit max_tokens, model default applies
max_cost_per_flow = 5.0            # optional (USD)
warn_at_cost = 4.0                 # optional (<= max_cost_per_flow)
step_timeout_secs = 600            # wall clock for one plan step when this agent runs as a subprocess
```

As a subagent, `max_tool_rounds` is also the LLM-turn cap per plan step and `step_timeout_secs` the wall clock the parent enforces.

### Claude Code

```toml
[claude_code]
cli_path = "claude"

[agents.main.claude_code]
builtin_tools_profile = "editor_shell"  # "none" | "read_only" | "editor" | "editor_shell"
```

Don't put a `claude_code` agent in the planner role unless you accept that the CLI keeps MCP tool access (planner-side tool stripping only works for OpenRouter) — see `CLAUDE.md` gotchas.

## Orchestrator

Schema: `src/config/mod.rs::OrchestratorConfig`.

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
| `engine` defaults to `"rag"`; any other value fails validation | `config/mod.rs::OrchestratorConfig`, `validation_errors` |
| Subagents are the `[agents.*]` blocks with a `description`; registry regenerated into `TENGU_PLANNER_REGISTRY.md` each planner turn | `orchestrator/shared_files.rs::routable_agents` |
| The child re-loads the same config (sandbox name over IPC) and takes `[agents.<step.agent>]`; unknown names fail before any spawn | `runner.rs::run_step`, `adapters/inbound/cli/run_agent.rs::run_agent_subprocess` |
| Accepted plan reaches the child as IPC `plan_state`; `TENGU_PLAN.md` is a debug mirror | `orchestrator/replan.rs`, `shared_files.rs` |

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
fixture_runner_agent = "fixture-runner"   # optional and unused — `tengu eval` runs rows on the eval config default agent
default_max_evolve_cycles = 3
default_rolling_window    = 10

[agents.skill-improver]
engine = "openrouter"
model  = "anthropic/claude-opus-4-7"
tools  = ["read_file", "list_directory"]
```

See [[skills#Metrics & Evolution]] for the frontmatter contract.

## Egress

Tor by default. Full reference: `docs/egress-2026-09-16.md`.

```toml
[egress]
network = "tor"          # default; "open" = plain internet
# proxy = "socks5h://127.0.0.1:9050"   # tor default (TENGU_TOR_PROXY overrides)
# route_llm_api = true                 # tor default; false under open
```

## Sandboxes

`sandboxes/<name>/config.toml` — `aura` (DeSci pipeline, `network = "open"`), `storage-test`, `unlimited` (single OpenRouter agent; bench recipe in `sandboxes/unlimited/BENCH.md`). Each file carries its own agents — there is no shared `agents/` directory.

```bash
tengu chat --sandbox aura
tengu telegram --sandbox aura
cargo run --features claude_code -- chat --sandbox aura   # aura's agents use engine = "claude_code"
```

## Environment variables

| Var | Read by | Default | Purpose |
|---|---|---|---|
| `OPENROUTER_API_KEY` | `adapters/outbound/engines/mod.rs`, `orchestrator/planner.rs`, `mcp_bridge.rs`, `main.rs` | — (required) | OpenRouter chat + embeddings |
| `OPENROUTER_BASE_URL` | `adapters/outbound/engines/mod.rs`, `outbound/tools/agentic_memory/mod.rs` | `https://openrouter.ai/api` | Override API base |
| `OPENROUTER_REFERER` | `adapters/outbound/engines/mod.rs` | unset | `HTTP-Referer` header |
| `OPENROUTER_TITLE` | `adapters/outbound/engines/mod.rs` | unset | `X-Title` header |
| `TELEGRAM_BOT_TOKEN` | `adapters/inbound/telegram.rs` | — (required for `telegram`) | Bot token |
| `TENGU_TELEGRAM_ALLOWED_USERS` | `adapters/inbound/telegram.rs::build_allowed_users` | unset | Comma-separated user ids merged with `[telegram].allowed_users` |
| `TENGU_HOME` | `config/paths.rs::resolve_tengu_home` | `~/.tengu` | State root (config, vault, logs, memory) |
| `TENGU_CONFIG` | `main.rs` (config resolution) | `~/.tengu/config.toml` | Path to config.toml (`-c/--config` wins) |
| `TENGU_MASTER_PASSWORD` | `main.rs`, `adapters/outbound/secrets.rs` | unset (prompt) | Vault password |
| `TENGU_SESSION_ID` | `bootstrap/orchestrator.rs::build_orchestrator`, `adapters/outbound/engines/claude_code.rs`, `outbound/tools/agentic_memory/mod.rs`, `memory/vector/embedder.rs` | fresh UUID | Pin the session id shared by planner + runner |
| `TENGU_MEMORY_DATABASE_URL` | `outbound/tools/agentic_memory/mod.rs` | — (required for `postgres_memory`) | Postgres + pgvector DSN |
| `TENGU_WIKI_COMPILER_MODEL` | `outbound/tools/agentic_memory/mod.rs::wiki_compiler_model` | `anthropic/claude-sonnet-4-6` | `compile_wiki` model |
| `TENGU_TUI_METRICS` | `tui/mod.rs` | off | Token/latency status line in the TUI |
| `TENGU_TUI_RAG_DEBUG` | `tui/mod.rs` | off | Planner recall hits as a System bubble |
| `TENGU_GPU_HINT` | `config/mod.rs::detect_gpu` | auto-detect | `none|cpu|off|false` or `gpu|cuda|metal|mps|on|true` for `runtime_profile = "auto"` |
| `TENGU_PERSISTENT_STORE_CHUNK_SIZE` | `adapters/outbound/engines/claude_code.rs` (forwarded to the MCP bridge child) | `[memory].persistent_store_chunk_size` | Chunk size for `persistent_store` |
| `TENGU_PERSISTENT_STORE_CHUNK_OVERLAP` | `adapters/outbound/engines/claude_code.rs` (forwarded to the MCP bridge child) | `[memory].persistent_store_chunk_overlap` | Chunk overlap for `persistent_store` |
| `CHAIN_ID` | `outbound/tools/crypto/helpers.rs::resolve_default_chain_id` | `DEFAULT_CHAIN_ID` | Chain id when a tool call omits it |
| `PRIVY_APP_ID` | `outbound/tools/crypto/helpers.rs` | — | Privy agentic wallet |
| `PRIVY_APP_SECRET` | `outbound/tools/crypto/helpers.rs` | — | Privy agentic wallet |
| `PRIVY_WALLET_ID` | `outbound/tools/crypto/helpers.rs` | — | Privy agentic wallet |
| `EVM_RPC_URL` | `outbound/tools/crypto/helpers.rs` | `https://ethereum-rpc.publicnode.com` | JSON-RPC endpoint |
| `TENGU_EGRESS` | `adapters/outbound/egress.rs::install` | unset | Parent → child resolved `[egress]` hand-off (JSON), set by `runner.rs` / `adapters/outbound/engines/claude_code.rs`; wins over the child's config |
| `TENGU_TOR_PROXY` | `config/egress.rs::EgressConfig::resolved` | `socks5h://127.0.0.1:9050` | Tor proxy under `network = "tor"` when `[egress].proxy` is unset (`docker-compose.tor.yml` sets `socks5h://tor:9050`) |
| `LYREBIRD_RS_DIR` | `Makefile` (exported to compose as `LYREBIRD_RS_SRC`, absolute) | `../lyrebird-rs` | Where the Tor image builds lyrebird-rs from (dir or git URL) |
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
- `[egress]`: unknown keys fail; `network` must be `tor|open`; `proxy` must be `socks5h|http|https` with a port (`socks5://` rejected — DNS leak); `route_llm_api = true` needs a `proxy`; `shell_network = "isolated"` is macOS-only and needs a loopback proxy.
- `limits.step_timeout_secs > 0`; `description`, when set, must not be empty.

## Reset

```bash
rm -rf ~/.tengu                          # everything
rm -rf ~/.tengu/state                    # sessions only
tengu prune                              # global cached/ephemeral state (keeps config + secrets)
tengu prune --sandbox <name>             # + that sandbox's workspace memory/tasks/attachments/storage + pipeline dirs
tengu prune --sandbox <name> --hard      # empties the workspace root entirely
                                         #   (every child: .tengu/, memory/, and any
                                         #   agent-created dirs), keeping the root itself
```

`prune` never touches `sandboxes/<name>/config.toml`, secrets, or managed skills.
Soft prune removes a known allow-list under `~/.tengu` and each agent `workspace`
(memory, tasks, attachments, storage, `scaffold.project.directories`). `--hard`
bypasses the allow-list and removes **every** entry inside the workspace root —
this catches arbitrary agent-created folders, which the allow-list can't know
about — while leaving the root dir itself so the sandbox is reusable. Safe because
the workspace root holds only generated files (config lives in the repo). `--hard`
requires `--sandbox`; it does **not** clear the Postgres `agentic_memory` store —
use `make clean` for that (global, destructive).

## Related
- [[architecture]] · `docs/architecture-2026-04-27.md` (canonical)
- [[engine-backends]] · [[skills]] · `docs/webhooks-2026-05-11.md`
