# Tengu Cluster

Multi-agent harness in Rust. Single binary. A planner LLM picks which `agents/<name>.toml` subagent handles each message; subagents run as `tengu run-agent` subprocesses with their own tools. Doctrine: **LLM = heart, Open Brain + Karpathy LLM Wiki = brain, tools = hands** (see `CLAUDE.md`).

## Prerequisites

| Requirement | Notes |
|---|---|
| Rust 1.78+ | `Cargo.lock` is v4; Docker image builds on `rust:1.91` |
| OpenRouter API key | [openrouter.ai](https://openrouter.ai) — planner + subagents by default |
| Claude Code CLI (optional) | `claude` installed + authenticated; build with `--features claude_code` |
| Docker (optional) | Compose file ships the image + a Postgres/pgvector service |
| Postgres + pgvector (optional) | Agentic memory; build with `--features postgres_memory` |

## Quickstart

```bash
git clone https://github.com/DemidovVladimir/tengu-cluster.git && cd tengu-cluster
cargo build                                            # default features: openrouter + telegram
cargo run -- secret init                               # encrypted vault ~/.tengu/secrets.vault
cargo run -- secret set OPENROUTER_API_KEY sk-or-...
mkdir -p ~/.tengu && cp config.example.toml ~/.tengu/config.toml
cargo run -- chat                                      # single-agent TUI
cargo run -- chat --sandbox aura                       # orchestrated sandbox (planner + subagents)
```

`export TENGU_MASTER_PASSWORD=...` skips the vault prompt. Config path resolution: `-c/--config <path>` > `TENGU_CONFIG` > `--sandbox <name>` (`sandboxes/<name>/config.toml`) > `~/.tengu/config.toml`.

## CLI

| Subcommand | What it does | Required feature | Required env |
|---|---|---|---|
| `chat [--sandbox <name>]` | Interactive TUI; orchestrated when the config has `[orchestrator]` | default | `OPENROUTER_API_KEY` |
| `telegram [--sandbox <name>]` | Telegram bot channel | `telegram` (default) | `TELEGRAM_BOT_TOKEN`, `OPENROUTER_API_KEY` |
| `webhooks [--sandbox <name>]` | Inbound HMAC-verified HTTP listener on port 7080; one-shot orchestrator turn per POST | `webhooks` | `OPENROUTER_API_KEY`, the `secret_env` per endpoint |
| `eval [<skill>...] [--sandbox] [--judge-model] [--concurrency N] [--format table\|json] [--out DIR]` | Run skill evals, LLM-judge scored | default | `OPENROUTER_API_KEY` |
| `skill evolve\|metrics\|accept-proposal\|remove\|list\|doctor\|export\|install` | Skill lifecycle | default | `OPENROUTER_API_KEY` (evolve) |
| `secret init\|set\|remove\|list\|change-password\|path` | Encrypted vault | default | `TENGU_MASTER_PASSWORD` (optional) |
| `status` | Print resolved config snapshot | default | — |
| `doctor` | Runtime/environment diagnostics (Docker healthcheck) | default | — |
| `prune [--sandbox <name>] [--yes]` | Remove cached/ephemeral state (keeps config + secrets) | default | — |
| `mcp-bridge` | MCP stdio server exposing Tengu tools; spawned by the Claude Code engine | default | — |
| `agentic-memory-server` | Standalone MCP stdio server exposing `agentic_memory` to external agents | `postgres_memory` | `TENGU_MEMORY_DATABASE_URL`, `OPENROUTER_API_KEY` |
| `run-agent` | Internal subprocess mode used by `SubprocessRunner`; refuses to run without `TENGU_AGENT_IPC=1` | — | — |

### Chat commands

| Command | Description |
|---|---|
| `/help` · `/cost` · `/context` · `/engine` | Info |
| `/eco` / `/standard` / `/precise` | Lens mode |
| `/reset` · `/purge` | Clear conversation (+ wipe persistent memory) |
| `/reload` · `/skills` | Re-read env + re-scan skills · list skills |

## Orchestration

| Piece | Where |
|---|---|
| Planner agent | `sandboxes/<name>/config.toml` → `[orchestrator] agent = "<planner agent>"`, `engine = "rag"` (historical name; file-registry planner — the only supported value) |
| Subagents | `agents/<name>.toml` (`name`, `description`, `engine`, `model`, `tools`, `skills`, `max_turns`, …) — shipped: `aura`, `learning-agent`, `researcher`, `storage` |
| Registry | `TENGU_PLANNER_REGISTRY.md` regenerated from `agents/` + skills + tools each planner turn; `TENGU_PLAN.md` carries the accepted plan to subagents (both gitignored) |
| Done signal | `compress_and_store` is appended implicitly to every subagent — never list it in `tools` |

```bash
cargo run -- chat --sandbox aura
cargo run --features claude_code -- chat --sandbox aura     # sandbox agents use engine = "claude_code"
cargo run -- telegram --sandbox aura
```

Adding an agent = drop `agents/<name>.toml` and restart. Adding a skill = drop `skills/<name>/SKILL.md` and restart.

### Mixed engines

Planner on OpenRouter, subagents on Claude Code. In `agents/<name>.toml` set `engine = "claude_code"` and `model = "claude-sonnet-4-6"` (bare slug); OpenRouter agents use `anthropic/claude-sonnet-4-6`. Per `CLAUDE.md`: **use OpenRouter for the planner agent and Claude Code for subagents** — the Claude Code CLI keeps MCP tool access at engine-construction time, so planner-side tool stripping does not stop it from calling tools instead of emitting plan JSON. Build with `--features claude_code`.

### Agentic memory (Postgres)

```bash
docker compose --profile postgres-memory up -d postgres-memory
export TENGU_MEMORY_DATABASE_URL=postgres://tengu:tengu@localhost:5432/tengu_memory
cargo run --features postgres_memory -- chat --sandbox aura
```

Open Brain lives in Postgres `agentic_memory` (pgvector, 1536-dim `text-embedding-3-small`); the `compile_wiki` operation builds the Karpathy LLM Wiki. To check a write landed, query Postgres directly via `TENGU_MEMORY_DATABASE_URL`. Spec: `docs/agentic-memory-prd-2026-05-13.md`, `-implementation-`, `-examples-`.

### Webhooks

```bash
cargo build --features webhooks
cargo run --features webhooks -- webhooks --sandbox aura
```

`[webhooks.endpoints.<name>]` blocks bind `/webhooks/<name>` to an agent; `X-Tengu-Signature: sha256=<hex>` HMAC. Operator doc: `docs/webhooks-2026-05-11.md`.

## Engines

| Backend | Config | Model slug | Feature |
|---|---|---|---|
| OpenRouter | `engine = "openrouter"` | `anthropic/claude-sonnet-4-6` | `openrouter` (default) |
| Claude Code | `engine = "claude_code"` | `claude-sonnet-4-6` | `claude_code` |

Claude Code runs agents through the local `claude` CLI; Tengu tools reach it via the MCP bridge as `mcp__tengu-tools__<name>` (`docs/mcp-bridge.md`).

## Feature flags

| Flag | Default | Purpose |
|---|---|---|
| `openrouter` | on | OpenRouter API backend |
| `telegram` | on | Telegram bot channel |
| `claude_code` | off | Claude Code CLI backend |
| `postgres_memory` | off | Postgres + pgvector agentic memory, `agentic-memory-server` |
| `webhooks` | off | `tengu webhooks` listener |

```bash
cargo build --features claude_code,postgres_memory,webhooks
cargo build --all-features
```

## Skills

`skills/<name>/SKILL.md` with YAML frontmatter (`name`, `description`). Three-tier scan with shadowing: `~/.tengu/skills/` → `<workspace>/.tengu/skills/` → `skills/`. Load per agent with `skill_packages = [...]` (config agents) or `skills = [...]` (`agents/*.toml`). Details: `docs/skills.md`.

## Sandboxes

| Sandbox | Description |
|---|---|
| `aura` | DeSci pipeline (research, mint, publish); agents on `claude_code`, webhook `test` endpoint |
| `storage-test` | Storage agent smoke config |
| `unlimited` | OpenRouter DeepSeek model bench — `unlimited` (v4-flash, default), `pro` (v4-pro), `r1` (r1-0528); no orchestrator, Telegram `@pro:` picks the model. Recipe: `sandboxes/unlimited/BENCH.md` |

## Telegram

```bash
cargo run -- secret set TELEGRAM_BOT_TOKEN "123456:ABC-..."
# ~/.tengu/config.toml:  [telegram] enabled = true  allowed_users = ["YOUR_USER_ID"]
cargo run -- telegram --sandbox aura
```

`@role: message` targets a specific agent (bypasses the planner unless `route_explicit_agents = true`).

## Docker

```bash
make setup                                   # .env + config.toml from examples
make up                                      # OpenRouter + Telegram (runs `tengu telegram`)
make up-memory                               # + Postgres/pgvector, image rebuilt with postgres_memory
make logs | make status | make doctor | make down | make clean
docker compose run tengu chat --sandbox aura
```

| Fact | Detail |
|---|---|
| Baked into the image | `skills/`, `agents/`, `sandboxes/` under `/opt/tengu` |
| Config | `./config.toml` mounted read-only, passed via `TENGU_CONFIG=/opt/tengu/config.toml` |
| Port | `7080` = webhook listener only (`TENGU_WEBHOOK_PORT` on the host); nothing listens on `[hub].port` |
| Features | `TENGU_FEATURES=openrouter,telegram,postgres_memory docker compose --profile postgres-memory up -d --build` |
| Healthcheck | `tengu doctor` exits non-zero when any agent engine fails to build — a config with `engine = "claude_code"` agents needs `claude_code` in `TENGU_FEATURES` or the container reports unhealthy |

VPS one-liner: `curl -fsSL https://raw.githubusercontent.com/DemidovVladimir/tengu-cluster/main/deploy/install.sh | bash` (`TENGU_PROFILE=postgres-memory` for memory). Cloud-init: `deploy/cloud-init.yml`.

### Systemd (native binary)

```ini
[Service]
WorkingDirectory=/opt/tengu-cluster
Environment=TENGU_MASTER_PASSWORD=your-password
Environment=RUST_LOG=tengu=info
ExecStart=/opt/tengu-cluster/target/release/tengu telegram --sandbox aura
Restart=on-failure
```

## Configuration

`config.example.toml` is the commented reference; `docs/configuration.md` has the full schema + environment-variable table.

| Section | Purpose |
|---|---|
| `[agents.<id>]` | engine, model, workspace, `skill_packages`, `workspace_tools`, limits, identity |
| `[orchestrator]` | `agent`, `engine = "rag"`, `max_attempts_per_step`, `max_replans`, `route_explicit_agents` |
| `[memory]` | disk vector store + `within_session_output_top_k` |
| `[telegram]` / `[webhooks]` / `[claude_code]` / `[skill_lifecycle]` / `[scaffold]` | channel + backend + lifecycle knobs |

## Documentation

| Doc | Covers |
|---|---|
| `docs/architecture-2026-04-27.md` (+ `.svg`, `.html`) | **Canonical** — seven steps from prompt to reply, file map per subsystem |
| `docs/SESSION_HANDOFF.md` | Running state log, open items, gotchas |
| `docs/agentic-memory-*-2026-05-13.md` | Open Brain + LLM Wiki spec |
| `docs/context-management-2026-04-27.md` | Every mechanism that shapes what an LLM sees |
| `docs/configuration.md` · `docs/engine-backends.md` · `docs/skills.md` · `docs/mcp-bridge.md` · `docs/webhooks-2026-05-11.md` | Reference |

## Module map

Flat: all code in `src/adapters/` + `src/main.rs`, no sub-crates. Ownership per file: `docs/architecture-2026-04-27.md` §2.

```
src/main.rs                     CLI entry (clap subcommands, run-agent subprocess body)
src/adapters/
  agents/                       AgentSpec loader for agents/<name>.toml
  channel_runtime.rs            build_orchestrator, register_core_plugins, WORKSPACE_TOOLS_ALLOWLIST
  chat_builder.rs               per-turn runtime (process_user_text)
  claude_code_engine.rs         Claude Code CLI engine (feature claude_code)
  config.rs                     Config / AgentConfig / OrchestratorConfig / WebhookConfig
  engine_builder.rs             OpenRouter engine + tool loop
  eval_builder.rs               tengu eval runner + LLM judge
  flow_builder.rs               session/flow management
  mcp_bridge.rs                 MCP stdio server (tengu mcp-bridge)
  memory/                       disk vector store, embedder, MemoryManager
  metrics.rs                    MetricsRecord + global sink
  orchestrator/                 RagPlanner, replan, shared_files (TENGU_PLANNER_REGISTRY.md / TENGU_PLAN.md)
  plugins/                      tools: workspace, http, crypto, cache, memory, skill, mcp, agentic_memory, skill_lifecycle, manage_skill, ...
  ports.rs                      ToolScope, ShellExecutionPort, EmbeddingPort, MemoryStorePort
  prompt_budget.rs              per-turn prompt assembly budget
  prune.rs                      tengu prune
  runner.rs                     SubprocessRunner (spawns tengu run-agent)
  scaffold.rs                   workspace scaffolding
  secret_builder.rs             encrypted vault
  shell_executor.rs             LocalShellExecutor
  skill_builder.rs              skill registry + three-tier scanner
  skill_lifecycle/              evolve / metrics / proposals
  telegram_builder.rs           Telegram adapter
  token.rs · usage.rs           token counting + usage accounting
  tool_builder.rs · tool_plugin.rs · tool_utils.rs   tool plumbing
  tui/                          terminal UI
  types.rs                      Engine trait, Message, ToolCall, StreamEvent
  webhook_builder.rs            tengu webhooks listener (feature webhooks)
  noop.rs · mod.rs · rag/       stubs / module root / legacy facade (see docs/SESSION_HANDOFF.md)
```
