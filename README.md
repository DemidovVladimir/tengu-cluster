# Tengu Cluster

Multi-agent harness in Rust. Single binary. One config file per sandbox (`sandboxes/<name>/config.toml`) holds every agent; a planner LLM picks which `[agents.<name>]` handles each message and subagents run as `tengu run-agent` subprocesses with their own tools. All traffic goes through **Tor by default** (Arti + [lyrebird-rs](https://github.com/DemidovVladimir/lyrebird-rs)); a sandbox opts out with `[egress] network = "open"`. Doctrine: **LLM = heart, Open Brain + Karpathy LLM Wiki = brain, tools = hands** (see `CLAUDE.md`).

## Prerequisites

| Requirement | Notes |
|---|---|
| Rust 1.78+ | `Cargo.lock` is v4; Docker image builds on `rust:1.91` |
| OpenRouter API key | [openrouter.ai](https://openrouter.ai) — planner + subagents by default |
| Docker + `lyrebird-rs` checkout | The Tor proxy (`make tor`) is a container: Arti + lyrebird-rs built from `../lyrebird-rs` (`LYREBIRD_RS_DIR=<dir or git URL>` to override). Skip only if every config sets `[egress] network = "open"` |
| Claude Code CLI (optional) | `claude` installed + authenticated; build with `--features claude_code` |
| Postgres + pgvector (optional) | Agentic memory; build with `--features postgres_memory` |

## Quickstart

```bash
git clone https://github.com/DemidovVladimir/tengu-cluster.git
git clone https://github.com/DemidovVladimir/lyrebird-rs.git         # sibling checkout, built into the Tor proxy image
cd tengu-cluster
cargo build                                            # default features: openrouter + telegram
cargo run -- secret init                               # encrypted vault ~/.tengu/secrets.vault
cargo run -- secret set OPENROUTER_API_KEY sk-or-...
mkdir -p ~/.tengu && cp config.example.toml ~/.tengu/config.toml
make tor                                               # Tor proxy on 127.0.0.1:9050 (waits until IsTor=true)
cargo run -- doctor --tor                              # policy + live Tor-exit check
cargo run -- chat                                      # single-agent TUI, over Tor
cargo run -- chat --sandbox aura                       # orchestrated sandbox (planner + subagents; aura is network = "open")
```

`export TENGU_MASTER_PASSWORD=...` skips the vault prompt. Config path resolution: `--sandbox <name>` (`sandboxes/<name>/config.toml`, replaces the rest wholesale) > `-c/--config <path>` > `TENGU_CONFIG` > `~/.tengu/config.toml`. No Tor: put `[egress] network = "open"` in the config.

### tengu + lyrebird-rs over Tor

`make tor` builds one container — Arti 2.6.0 + your `lyrebird-rs` (managed obfs4/snowflake PT) — and every tengu request (tools, OpenRouter, embeddings, Claude Code CLI, Telegram) goes through `socks5h://127.0.0.1:9050`, fail-closed. Verify the transport before anything else:

```bash
make tor                                   # waits until check.torproject.org reports IsTor=true
cargo run -- doctor --sandbox <name> --tor # → proxy reachable · IsTor=true · exit=<ip> · llm api: via proxy
```

| Symptom | Meaning | Fix |
|---|---|---|
| `doctor --tor` shows `IsTor=true` | Arti + lyrebird-rs transport is up | — |
| proxy port unreachable | `make tor` not running / still bootstrapping (~30s obfs4) | `make tor` then `make tor-logs` |
| LLM call returns `403 "Just a moment…"` (`cZone: openrouter.ai`) | **Transport works** — the request reached the provider; its Cloudflare is challenging the Tor exit IP | see below |

**Cloudflare-fronted providers block Tor exits.** OpenRouter (and Molecule / Privy / Beach) sit behind Cloudflare, which serves a managed challenge to Tor exit IPs — the `403` is an application-layer block on the exit node, not a lyrebird/Arti/tengu fault (`curl --socks5-hostname 127.0.0.1:9050 https://api.ipify.org` still returns `200` + the exit IP). For those, set `[egress] network = "open"` on the sandbox (as `sandboxes/aura` does). Tools and non-Cloudflare traffic still route over Tor cleanly; only the challenged provider needs the opt-out.

## CLI

| Subcommand | What it does | Required feature | Required env |
|---|---|---|---|
| `chat [--sandbox <name>]` | Interactive TUI; orchestrated when the config has `[orchestrator]` | default | `OPENROUTER_API_KEY` |
| `telegram [--sandbox <name>]` | Telegram bot channel | `telegram` (default) | `TELEGRAM_BOT_TOKEN`, `OPENROUTER_API_KEY` |
| `webhooks [--sandbox <name>]` | Inbound HMAC-verified HTTP listener on port 7080; one-shot orchestrator turn per POST | `webhooks` | `OPENROUTER_API_KEY`, the `secret_env` per endpoint |
| `eval [<skill>...] [--sandbox] [--judge-model] [--concurrency N] [--format table\|json] [--out DIR]` | Run skill evals, LLM-judge scored | default | `OPENROUTER_API_KEY` |
| `skill evolve\|metrics\|accept-proposal\|remove\|list\|doctor [--sandbox]\|export\|install` | Skill lifecycle | default | `OPENROUTER_API_KEY` (evolve) |
| `secret init\|set\|remove\|list\|change-password\|path` | Encrypted vault | default | `TENGU_MASTER_PASSWORD` (optional) |
| `status` | Print resolved config snapshot | default | — |
| `doctor [--sandbox <name>] [--tor]` | Runtime/environment diagnostics (Docker healthcheck); `[egress]` proxy reachability, `--tor` live Tor-exit check | default | — |
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
| Subagents | Any `[agents.<name>]` block **with a `description`** in the same file (`engine`, `model`, `tools`, `skill_packages`, `example_queries`, `limits.max_tool_rounds`, `limits.step_timeout_secs`, …). Same schema as in-process agents — no separate spec files |
| Registry | `TENGU_PLANNER_REGISTRY.md` regenerated from those blocks + skills + tools each planner turn; `TENGU_PLAN.md` carries the accepted plan to subagents (both gitignored) |
| Done signal | `compress_and_store` is appended implicitly to every subagent — never list it in `tools` |

```bash
cargo run -- chat --sandbox aura
cargo run --features claude_code -- chat --sandbox aura     # sandbox agents use engine = "claude_code"
cargo run -- telegram --sandbox aura
```

Adding an agent = add an `[agents.<name>]` block with a `description` and restart. Adding a skill = drop `skills/<name>/SKILL.md` and restart.

### Mixed engines

Planner on OpenRouter, subagents on Claude Code. In the agent block set `engine = "claude_code"` and `model = "claude-sonnet-4-6"` (bare slug); OpenRouter agents use `anthropic/claude-sonnet-4-6`. Per `CLAUDE.md`: **use OpenRouter for the planner agent and Claude Code for subagents** — the Claude Code CLI keeps MCP tool access at engine-construction time, so planner-side tool stripping does not stop it from calling tools instead of emitting plan JSON. Build with `--features claude_code`.

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

`skills/<name>/SKILL.md` with YAML frontmatter (`name`, `description`). Three-tier scan with shadowing: `~/.tengu/skills/` → `<workspace>/.tengu/skills/` → `skills/`. Load per agent with `skill_packages = [...]` (`skills = [...]` is accepted too). Details: `docs/skills.md`.

## Sandboxes

One file per sandbox — channel settings, `[egress]`, the planner and every agent.

| Sandbox | Network | Description |
|---|---|---|
| `aura` | `open` (Molecule / Privy / Beach block Tor exits) | DeSci pipeline (research, mint, publish); agents `aura` (planner + pipeline), `researcher`, `learning-agent`, `skill-improver`, `fixture-runner` on `claude_code`; webhook `test` endpoint |
| `storage-test` | tor | Storage agent smoke config (`persistent_store`) |
| `unlimited` | tor | Single OpenRouter agent (`qwen/qwen3.8-27b`), no orchestrator. Bench recipe: `sandboxes/unlimited/BENCH.md` |

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
make tor                                     # Tor proxy only (native tengu): Arti + lyrebird-rs on 127.0.0.1:9050
make up                                      # `tengu telegram` on ./config.toml
make up SANDBOX=unlimited                    # `tengu telegram` on sandboxes/unlimited/config.toml
make up-memory [SANDBOX=<name>]              # + Postgres/pgvector, image rebuilt with postgres_memory
make logs | make status | make doctor | make down | make down-all | make clean | make tor-down | make tor-logs   # pass the same SANDBOX=
make chat SANDBOX=unlimited                  # interactive TUI in a throwaway container (Ctrl-C to quit)
```

Network follows the chosen config's `[egress] network`: Tor (tengu's only exit is the `tor` container) unless it says `"open"`. `NETWORK=tor|open` overrides; a mismatch with the config breaks all traffic.

| Fact | Detail |
|---|---|
| Tor proxy | `deploy/tor/`: Arti 2.6.0 + lyrebird-rs (managed obfs4/snowflake transport). Built from the sibling `../lyrebird-rs` checkout (`LYREBIRD_RS_DIR=<dir or git URL>` to override). `make tor-bridges` prints fresh bridge lines for `deploy/tor/arti.toml` |
| `NETWORK=tor` (default) | `docker-compose.tor.yml` includes `deploy/tor/compose.yml` and puts tengu + Postgres on an internal network whose only exit is the `tor` container (`TENGU_TOR_PROXY=socks5h://tor:9050`); no host ports are published (7080 nor 9050) |
| Baked into the image | `skills/`, `sandboxes/` under `/opt/tengu` |
| Config | `./config.toml` (or `sandboxes/<name>/config.toml` with `SANDBOX=<name>`) mounted read-only at `/opt/tengu/config.toml` = `TENGU_CONFIG`; TOML edits apply on container restart |
| `engine = "claude_code"` sandboxes (`aura`) | Not runnable in the image: it has no Claude Code CLI. `doctor` fails → container unhealthy |
| `~` in sandbox paths | Expands to `/root` in the container — not the `tengu-data` volume, so `~/<name>-workspace` is lost on container recreate |
| Port | `7080` = webhook listener only (`TENGU_WEBHOOK_PORT` on the host, `NETWORK=open` only); nothing listens on `[hub].port` |
| Features | `TENGU_FEATURES=openrouter,telegram,postgres_memory docker compose --profile postgres-memory up -d --build` |
| Healthcheck | `tengu doctor` exits non-zero when any agent engine fails to build or the Tor proxy is unreachable — a config with `engine = "claude_code"` agents needs `claude_code` in `TENGU_FEATURES` or the container reports unhealthy |

VPS one-liner: `curl -fsSL https://raw.githubusercontent.com/DemidovVladimir/tengu-cluster/main/deploy/install.sh | bash` (`TENGU_NETWORK=open` to skip Tor, `TENGU_PROFILE=postgres-memory` for memory; clones lyrebird-rs next to the checkout). Cloud-init: `deploy/cloud-init.yml`.

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
| `[agents.<id>]` | engine, model, workspace, `skill_packages`, `workspace_tools`, `tools`, limits, identity; add `description` (+ `example_queries`) to make it a planner-routable subagent |
| `[orchestrator]` | `agent`, `engine = "rag"`, `max_attempts_per_step`, `max_replans`, `route_explicit_agents` |
| `[memory]` | disk vector store + `within_session_output_top_k` |
| `[telegram]` / `[webhooks]` / `[claude_code]` / `[skill_lifecycle]` / `[scaffold]` | channel + backend + lifecycle knobs |
| `[egress]` | `network = "tor"` (default) or `"open"`; proxy, host allowlist, shell sandbox, JSONL audit — `docs/egress-2026-09-16.md` |

## Documentation

| Doc | Covers |
|---|---|
| `docs/architecture-2026-04-27.md` (+ `.svg`, `.html`) | **Canonical** — seven steps from prompt to reply, file map per subsystem |
| `docs/SESSION_HANDOFF.md` | Running state log, open items, gotchas |
| `docs/agentic-memory-*-2026-05-13.md` | Open Brain + LLM Wiki spec |
| `docs/context-management-2026-04-27.md` | Every mechanism that shapes what an LLM sees |
| `docs/configuration.md` · `docs/engine-backends.md` · `docs/skills.md` · `docs/mcp-bridge.md` · `docs/webhooks-2026-05-11.md` · `docs/egress-2026-09-16.md` | Reference |

## Module map

Flat: all code in `src/adapters/` + `src/main.rs`, no sub-crates. Ownership per file: `docs/architecture-2026-04-27.md` §2.

```
src/main.rs                     CLI entry (clap subcommands, run-agent subprocess body)
src/adapters/
  channel_runtime.rs            build_orchestrator, register_core_plugins, WORKSPACE_TOOLS_ALLOWLIST, subagent_config
  chat_builder.rs               per-turn runtime (process_user_text)
  claude_code_engine.rs         Claude Code CLI engine (feature claude_code)
  config.rs                     Config / AgentConfig (in-process + subagent fields) / OrchestratorConfig / WebhookConfig
  egress.rs                     [egress] policy: network tor|open, proxy, host allowlist, shell sandbox, JSONL audit
  engine_builder.rs             OpenRouter engine + tool loop
  eval_builder.rs               tengu eval runner + LLM judge
  flow_builder.rs               session/flow management
  mcp_bridge.rs                 MCP stdio server (tengu mcp-bridge)
  memory/                       disk vector store, embedder, MemoryManager
  metrics.rs                    MetricsRecord + global sink
  orchestrator/                 RagPlanner, replan, shared_files (TENGU_PLANNER_REGISTRY.md from [agents.*] / TENGU_PLAN.md)
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
  noop.rs · mod.rs              stubs / module root
```
