# Configuration

One TOML file per sandbox holds the channel settings, `[egress]`, the planner and **every agent** (in-process and subagent alike). Commented reference: `config.example.toml`. Code side (structs, who reads each section, how to add a field): `docs/code-map.md` §3–§4. Every section below lists its struct in `src/config/`, its defaults and its load rules (a violated rule fails `Config::load`, all errors listed at once).

## Load pipeline

| Step | What | Where |
|---|---|---|
| 1 | `.env` from the cwd or its nearest parent; variables already in the shell win | `cli/mod.rs::run` (`dotenvy`) |
| 2 | Vault `<TENGU_HOME>/secrets.vault` → env (`TENGU_MASTER_PASSWORD` or a prompt on `/dev/tty`: Enter skips it, `</dev/null` does not; a process with no controlling terminal skips it with a warning). An inherited `TENGU_SECRETS_LOADED` means an ancestor `tengu` opened it — no prompt. `mcp-bridge`, `run-agent`, `agentic-memory-server` never prompt | `outbound/secrets.rs` |
| 3 | Base config: `-c/--config` > `$TENGU_CONFIG` > `<TENGU_HOME>/config.toml` (`TENGU_HOME` defaults to `~/.tengu`); a missing file = `Config::default()` (one `main` agent, `openrouter`, `anthropic/claude-sonnet-4.6`); `TENGU_CONFIG` pinned to that path | `cli/mod.rs::run`, `config/paths.rs` |
| 4 | `--sandbox <s>`: `sandboxes/<s>/config.toml` **relative to the cwd** replaces the base config wholesale (the base still loads first — an invalid base fails the command); `TENGU_CONFIG` re-pinned to the absolute sandbox file; its `[egress]` installed | `bootstrap/sandbox.rs::load_sandbox_or` |
| 5 | `Config::load`: read → `${VAR}` substitution (`${UPPER_CASE}` only; an unset var stays literal) → TOML parse → validation (+ a hardened sandbox's config-file reach rule) → warnings logged → `fold_default_scopes` (default scopes into every agent, `~` in `fs_roots`, hardened flag, signer path, the sandbox sections tools read) | `config/mod.rs` |
| 6 | Children: `run-agent` gets the sandbox name over IPC and the resolved policy as `TENGU_EGRESS`; `tengu mcp-bridge` loads `TENGU_CONFIG` and runs tools as `[agents.<TENGU_BRIDGE_AGENT>]` | `outbound/subprocess_runner.rs`, `inbound/mcp_bridge.rs` |

The sandbox name of a config file = `<name>` of `…/sandboxes/<name>/config.toml` however it was loaded (`-c` and `TENGU_CONFIG` included), else `default` — the paper ledger's account owner (`config/sections.rs`).

## Initial setup

```bash
tengu secret init                              # vault + master password
tengu secret set OPENROUTER_API_KEY sk-or-...
tengu secret set TELEGRAM_BOT_TOKEN 123:ABC-..
mkdir -p ~/.tengu && cp config.example.toml ~/.tengu/config.toml
make tor                                       # Tor proxy (default network); or set [egress] network = "open"
tengu chat                                     # or: tengu chat --sandbox lping
```

## Root sections

`Config` is `deny_unknown_fields`: an unknown or misspelled top-level key (`[rsik]`) fails the load.

| Key | Struct (`src/config/`) | Absent = | Unknown keys inside |
|---|---|---|---|
| `runtime_profile` | `Config` (`mod.rs`) | `"auto"` | — |
| `[hub]` | `HubConfig` | defaults | ignored |
| `[agents.<id>]` | `AgentConfig` | validation error (at least one agent) | **error** (direct keys, `.local`); ignored in `.limits`, `.flow`, `.identity`, `.lens`, `.prompt_budget`, `.claude_code`, `.scopes.<tool>` |
| `[orchestrator]` | `OrchestratorConfig` | single-agent dispatch | ignored |
| `[memory]` | `MemoryConfig` | `enabled = false` | ignored |
| `[telegram]` · `[webhooks]` | `TelegramConfig` · `WebhookConfig` | off | ignored |
| `[decision_loops.<n>]` | `DecisionLoopConfig` (`decision_loop.rs`) | none | **error** |
| `[scaffold]` | `ScaffoldConfig` | none | ignored |
| `[claude_code]` | `ClaudeCodeConfig` | defaults | ignored |
| `[default_scopes.<tool>]` | `ToolScope` (`domain/scope.rs`) | permissive fallback per tool | ignored |
| `[egress]` | `EgressConfig` (`egress.rs`) | Tor | **error** |
| `[[mcp_servers]]` | `McpServerConfig` | none | ignored |
| `[solana]` | `SolanaConfig` (`solana.rs`) | write tools simulate only | **error** |
| `[xmarket]` | `XmarketConfig` (`xmarket.rs`) | not an xmarket sandbox | **error** (calendars, `weekend_fade` too) |
| `[risk]` · `[paper]` | `RiskConfig` · `PaperConfig` (`risk.rs`) | exec tools refuse (`risk_config_missing`) | **error** (nested tables too) |
| `[rate_limits.<name>]` | `RateLimitConfig` (`rate_limits.rs`) | that name unlimited | **error** |
| `[runtime]` | `RuntimeConfig` (`runtime.rs`) | defaults | **error** |
| `[recorder]` | `RecorderConfig` (`recorder.rs`) | off | **error** |
| `[backtest]` | `BacktestConfig` (`backtest.rs`) | no backtests | **error** (costs and `half_spread` too) |
| `[strategy_ranking]` | `StrategyRankingConfig` (`strategy_ranking.rs`) | no rankings (`no_strategy_ranking`) | **error** |
| `[feeds.<n>]` | `FeedConfig` (`feeds.rs`) | no feeds | **error** |
| `[sources]` | `SourcesConfig` (`sources.rs`) | no source registry (`tengu sources` and `source_evidence` refuse: `sources_state_missing`) | **error** (every registry row too) |
| `[skill_lifecycle]` | `SkillLifecycleConfig` (`skill_lifecycle.rs`) | `tengu skill evolve` refuses | ignored |

## Agents — `[agents.<id>]`

```toml
[agents.main]
engine = "openrouter"              # "openrouter" | "local" | "claude_code"
model = "anthropic/claude-sonnet-4-6"   # claude_code wants the bare slug: "claude-sonnet-4-6"; local: the server's id
default = true                     # at most one default agent
workspace = "~/projects/my-app"
skill_packages = ["my-skill"]      # `skills = [...]` is an accepted alias
tools = ["http_request", "read_file"]   # allow-list on every surface; empty = every base tool
# --- subagent view (planner-routable when `description` is set) ---
description = "What this agent handles and what it is NOT for — read by the planner LLM."
example_queries = ["what is the BTC price?"]
```

| Field | Default | Notes |
|---|---|---|
| `engine` | required | `openrouter` (`OPENROUTER_API_KEY`), `local` (OpenAI-compatible server on this host / LAN), `claude_code` (`claude` CLI + `--features claude_code`) — `docs/engine-backends.md` |
| `model` | required | non-empty; slug format per engine |
| `default` | `false` | at most one agent |
| `description` | none | present ⇒ in `TENGU_PLANNER_REGISTRY.md`, runnable as a plan step (`tengu run-agent`); absent ⇒ in-process only (planner role, `@role:` chat). No `description` and not `default` = a **private** agent: never reachable from Telegram, the only kind that may hold exec tools or a Solana wallet grant. Non-empty when set |
| `example_queries` | `[]` | rendered under the registry entry |
| `tools` | `[]` | allow-list on every surface: chat (TUI, Telegram, webhooks, eval, `tengu tool`), `run-agent` steps, feeds, decision loops, the Claude Code bridge, `[[mcp_servers]]` tools (`bootstrap::tools::agent_base_tools`). Empty = every always-on tool + configured opt-ins. Opt-in names listed here switch on. Never list `compress_and_store` |
| `workspace_tools` | `[]` | older opt-in list, merged with `tools`; only `src/domain/tools.rs::WORKSPACE_TOOLS` names |
| `workspace` | none | `~` expanded. Without it a plan step / webhook turn runs in a temp dir removed after it (load warns when the agent's tools write files or skills) |
| `skill_packages` (`skills`) | `[]` | skills loaded (`docs/skills.md`) |
| `default_lens` | `"eco"` | `eco` \| `standard` \| `precise` |
| `role` | none | free-form; non-empty when set |
| `scopes.<tool>` | `{}` | `ToolScope` — § Scopes |
| `claude_code.builtin_tools_profile` | `"editor_shell"` | `none` \| `read_only` \| `editor` \| `editor_shell` (read trimmed; unknown = error); `none` required in a hardened sandbox |
| `local.base_url` · `local.api_key_env` | `http://127.0.0.1:8888` · `UNSLOTH_API_KEY` | `engine = "local"` only; a trailing `/v1` is dropped; unset / empty env = no `Authorization` header |

| Sub-table | Fields (default) | Rules |
|---|---|---|
| `.identity` | `name`, `instructions` (none) | `instructions` go into the system prompt |
| `.flow` | `scope` (`per-sender`: `main` \| `per-group` \| `per-pipe-sender` \| `per-sender`), `reset_mode` (`idle`: `idle` \| `manual` \| `time`), `idle_timeout_minutes` (30), `max_history_turns`, `compaction_threshold_ratio`, `compaction_keep_turns`, `compaction_summary_max_tokens` (none) | ratio within (0, 1]; the others > 0 when set |
| `.limits` | `max_tokens_per_flow` (100 000), `max_cost_per_flow`, `warn_at_cost` (none, USD), `context_window` (1 000 000), `max_output_tokens_per_turn` (none = model default), `max_tool_rounds` (70), `max_tool_result_chars` (300 000), `stream_event_timeout_secs` (120), `request_timeout_secs` (600), `compact_result_limit` (200), `max_mcp_result_chars` (50 000), `step_timeout_secs` (600) | `max_tokens_per_flow`, `step_timeout_secs` > 0; costs > 0, `warn_at_cost` ≤ `max_cost_per_flow`; `max_output_tokens_per_turn` ≤ `context_window`; a `local` agent on the default window warns — set the server's real one |
| `.lens` | `eco_max_tokens` (100), `standard_threshold` (0.7), `precise_budget` (0.5) | > 0; both within [0, 1] |
| `.prompt_budget` | `max_file_tokens` (2 000), `max_skill_context_tokens` (16 000), `max_total_tokens` (32 000) | > 0; each ≤ `max_total_tokens` |

| Limit | Meaning |
|---|---|
| `max_tool_rounds` | per chat turn or plan step: engine turns on `openrouter` / `local` (chat then forces one answer without tools), tool calls on `claude_code` (the CLI run is killed past it) |
| `step_timeout_secs` | wall clock the parent enforces on one `run-agent` step |
| `context_window` (`local`) | one tool result is capped at 1/8 of it; a typed row above the cap is compacted to a pointer |

## Scopes — `[default_scopes.<tool>]`, `[agents.<id>.scopes.<tool>]`

| Field | Grants (empty = none) |
|---|---|
| `fs_roots` | filesystem roots (`~` expanded; paths resolved, symlinks followed) |
| `net_hosts` | hosts: exact, `*.suffix` (subdomains only), `*` |
| `env_reads` | env vars a tool may read (`$VAR` headers, `HL_API_URL`, `SOLANA_RPC_URL`, …); `*` = any |
| `shell_bins` | `run_command` / shell skills: the command's first word only (leading `NAME=value` skipped) — a guard rail, not a sandbox |
| `wallets` | Privy wallet labels; Solana write tools: full pubkeys |

| Rule | Effect |
|---|---|
| Precedence | an agent's own `scopes.<tool>` replaces `[default_scopes.<tool>]` wholesale (never field-merged); a tool with neither gets the permissive fallback (no shell in a hardened sandbox) |
| Plan steps | `run-agent` (and its bridge) add the step workspace to every configured scope's `fs_roots` — except a deny-all scope (every field empty), which stays a deny |
| Writers | `write_file` refuses `.tengu/`, `.claude/`, `.git/`, `skills/` at any depth and `CLAUDE.md` / `CLAUDE.local.md` / `AGENTS.md` / `.mcp.json`; a hardened sandbox also `MEMORY.md` / `USER.md` / `IDENTITY.md` / `PROFILE.md` / `CONTEXT.md` |
| Per-family needs | `docs/tools.md` § Give it to an agent |

## Orchestrator — `[orchestrator]`

```toml
[orchestrator]
agent = "lping"                # the [agents.*] block that runs the planner LLM call (use an OpenRouter agent)
engine = "rag"                 # the only accepted value (historical name; file-registry planner)
max_attempts_per_step = 3      # Tier 1: retries per step before escalation
max_replans = 2                # Tier 2: replans before bailing out
route_explicit_agents = false  # true = `@role:` messages also go through the planner
```

| Fact | Where |
|---|---|
| Presence turns orchestration on; `engine` defaults to `"rag"`, anything else fails validation | `config/mod.rs::OrchestratorConfig` |
| Planner prompt = `skills/orchestrator/SKILL.md` read from the cwd (inline fallback when missing) | `application/orchestrator/planner.rs` |
| Subagents = the `[agents.*]` blocks with a `description`; registry regenerated each planner turn | `orchestrator/shared_files.rs::routable_agents` |
| The child re-loads the same config and takes `[agents.<step.agent>]`; the accepted plan arrives as IPC `plan_state` (`TENGU_PLAN.md` = debug copy) | `cli/run_agent.rs`, `orchestrator/replan.rs` |

## Memory — `[memory]`

| Field | Default | Notes |
|---|---|---|
| `enabled` | `false` | disk vector store + `memory_ingest` / `memory_search`; embeddings need `OPENROUTER_API_KEY` |
| `embedding_model` | `text-embedding-3-small` | must stay 1536-dim (Postgres `vector(1536)`) |
| `max_recall_entries` · `max_recall_tokens` | 5 · 600 | |
| `store_path` | `~/.tengu/memory/` | store dir (`vectors.bin`) of an agent without `workspace` — else `<workspace>/memory/`; the disk bincode store is the only built-in backend (no `backend` key; one set is ignored) |
| `persistent_store_chunk_size` · `_overlap` | 1000 · 200 | characters |
| `session_recent_n` | 10 | last messages reloaded per orchestrator turn |
| `cross_plan_top_k` | 5 | replan recall over `agentic_memory` step outputs |
| `cross_session_msg_top_k` | 0 (off) | cross-session user-message recall |
| `within_session_output_top_k` | 0 (off) | step outputs of this session in the planner prompt; needs `postgres_memory`; 3–5 recommended |

Durable memory is the Postgres `agentic_memory` plugin: `--features postgres_memory` + `TENGU_MEMORY_DATABASE_URL`. Spec: `docs/agentic-memory-*-2026-05-13.md`.

## Telegram — `[telegram]`

| Field | Default | Notes |
|---|---|---|
| `enabled` | `false` | |
| `allowed_users` | `[]` | merged with `TENGU_TELEGRAM_ALLOWED_USERS`; both empty = `tengu telegram` refuses to start; an unlisted sender gets "Unauthorized." |
| `tool_approvals` · `approve_only` | `false` · `[]` | **not implemented** — parsed, gate nothing on any engine or surface; `Config::load` warns while set. Gate tools with `tools` and scopes |

Private agents (no `description`, not `default`) are never `@`-routable from Telegram nor its default.

## Webhooks — `[webhooks]`

| Field | Default | Notes |
|---|---|---|
| `enabled` | `false` | `tengu webhooks` refuses to start when off; `tengu run` mounts the routes only when on (and built with `webhooks`) |
| `bind` · `port` | `127.0.0.1` · 7080 | |
| `endpoints.<n>.agent` | `""` | informational — the planner routes; required unless `loop` is set |
| `endpoints.<n>.loop` | none | feed the payload to `[decision_loops.<loop>]` instead of the planner |
| `endpoints.<n>.secret_env` · `.secret` | none | HMAC-SHA256 key (`X-Tengu-Signature: sha256=<hex>`): env var name, or inline (less safe); exactly one |
| `endpoints.<n>.auth_header_env` | none | static `Authorization` header value (Helius), constant-time compare; never with an HMAC secret |
| `endpoints.<n>.goal_template` | `"A webhook arrived. Process the payload below."` | prefix of the synthesized user message |

Checked when the listener starts: exactly one auth scheme, `loop` names a `[decision_loops]` block, `agent` set when there is no `loop`; an env var missing at request time answers 500. Reply `202 {"session_id": "webhook-<n>-<uuid>"}`; a full loop queue answers 429. Operator doc: `docs/webhooks-2026-05-11.md`.

## Decision loops — `[decision_loops.<n>]`

Jev picks, existing tools run (`docs/decision-loop-plan-2026-09-24.md`). Triggers: webhook `loop = "<n>"`, `tengu decide --loop <n>`, a `[feeds.<f>] kind = "tick"` under `tengu run`, `tengu backtest --gate`.

| Field | Default | Notes |
|---|---|---|
| `model` | `~typesafe/jev-latest` | OpenRouter `/api/alpha/decisions` (`OPENROUTER_API_KEY`) |
| `goal` | required | objective + hard limits |
| `agent` | required | an `[agents.<a>]` block: its tools, scopes, workspace |
| `history` · `max_steps` | 8 · 4 | ≥ 1 |
| `act_at` | 0.8 | (0, 1]; below it the step escalates (or stops) |
| `dry_run` · `escalate` | `true` · `true` | dry run: non-`read_only` actions logged, not run |
| `timeout_secs` | 20 | decisions HTTP timeout |
| `event_reduce` | `{}` | reducer for the event |
| `world` · `world_max_age_secs` | `{}` · 30 | alias → observation key `<schema>:<subject>`, read from the agent's store each step, never fetched |
| `actions.<a>` | ≥ 1, one terminal (no `tool`) | `description` (required), `tool`, `args` (`{slot}` templating), `slots` (static list \| `{from, items, value, top}` \| `{observation, items, value, top}`, top 5), `reduce`, `caps`, `read_only`, `requires` (`world` alias → max age s) |

| Load rule | |
|---|---|
| `agent` exists; with a non-empty `tools`, every action `tool` is in it (a dry-run loop's write actions excepted) | `config/mod.rs` |
| a terminal action has no slots; `from` / `observation` / `requires` / `caps` name an action / `world` alias / slot | `decision_loop.rs` |
| a loop action running a Solana write tool with `read_only = true` sets `args.mode = "simulate"`; one running an exec tool is not `read_only` | `solana.rs`, `risk.rs` |

## Scaffold, hub, Claude Code, MCP servers

| Section | Fields (default) | Notes |
|---|---|---|
| `[scaffold]` | `root` (required), `directories` (`[]`), `files` (`[{path, content}]`), `project.directories` · `project.files` | created when `tengu telegram` starts (files never overwritten); `project` = template of Telegram's `/project <name>`; soft `tengu prune --sandbox` clears `project.directories` |
| `[hub]` | `bind` (`127.0.0.1`), `port` (7070), `auth_mode` (`token` \| `open`), `auth_token`, `reload.mode` (`hybrid` \| `hot` \| `restart` \| `off`), `reload.debounce_ms` (300) | config-only: validated, shown by `tengu status`; nothing listens |
| `[claude_code]` | `cli_path` (`claude`), `timeout_secs` (120) | per-agent `builtin_tools_profile` in `[agents.<a>.claude_code]` |
| `[[mcp_servers]]` | `name`, `transport` (`stdio` \| `http`), `command` (stdio), `url` (http), `env` (`$VAR` resolved at spawn), `auth = { type = "bearer", token = "$VAR" }` | tools `<name>__<tool>`; a schema outside the engine subset is dropped with a warn; refused in a hardened sandbox — `docs/tools.md` § MCP servers |

## Egress — `[egress]`

Tor by default. Full reference: `docs/egress-2026-09-16.md`.

| Field | Default | Notes |
|---|---|---|
| `network` | `"tor"` | `tor` \| `open` |
| `proxy` | tor: `TENGU_TOR_PROXY` or `socks5h://127.0.0.1:9050`; open: none | `socks5h` \| `http` \| `https` with a port; `socks5://` refused (DNS leak) |
| `route_llm_api` | tor: `true`; open: `false` | LLM provider traffic (OpenRouter, embeddings, Jev, the Claude CLI) through the proxy; `true` needs a proxy |
| `allow_hosts` · `deny_hosts` | `[]` | sandbox-wide ceiling for tool traffic (`*.suffix`, `*`); deny checked first; no empty pattern |
| `https_only` | `false` | refuse `http://` |
| `shell_network` | `"proxy_env"` | `proxy_env` (advisory) \| `isolated` (macOS `sandbox-exec`, loopback proxy only) |
| `audit` · `audit_log` | `true` · `<TENGU_HOME>/logs/egress.jsonl` | JSONL per request; `~` expanded |

## Solana signer — `[solana]`

| Field | Default | Rules (`solana.rs`, `hardening.rs`) |
|---|---|---|
| `signer_key_file` | none = write tools simulate only | absolute or `~/…`, non-empty; solana-keygen JSON or base58, 0600; makes the sandbox hardened (§ Hardened sandboxes); a write tool's `wallets` grant only on a private agent's own scope, never in `[default_scopes]` |

Write-tool send rules: `docs/typed-observations-2026-09-24.md` § Write tools.

## xmarket — `[xmarket]`

| Field | Default | Notes |
|---|---|---|
| `state` | `"xmarket"` | one directory name `[A-Za-z0-9._-]` (not `.`, `..`, `flows`): state dir `<TENGU_HOME>/state/<state>` — `ledger.db`, `runtime.db`, `run-<sandbox>.json`, `history/`, `market.db`, `backtests/`; `tengu prune` never deletes it |
| `calendars.<id>` | `{}` | § Calendars |
| `weekend_fade` | none | § Weekend fade (rule W) |

| Load rule (with `[feeds]`, `[risk]` or `[xmarket]`) | Why |
|---|---|
| `[risk]` needs `[xmarket]` | the ledger lives in the state dir |
| every agent named by `[feeds.*].agent` / `[decision_loops.*].agent` or holding an xmarket tool (`XM_TOOLS`) sets the same `workspace`, absolute or `~/…` | one observation store for every process |
| with `[risk]` every agent sets a `workspace` | the process cwd may hold `<TENGU_HOME>/state` |
| the state dir outside every `fs_roots` and agent `workspace` (both ways, symlinks resolved) | file tools must not reach the stores |

### Calendars — `[xmarket.calendars.<id>]`

| `kind` | Fields (a field of another kind is an error) |
|---|---|
| `exchange` | `tz`, `core = ["09:30", "16:00"]`, `pre`, `post`, `overnight` (`false`), `early_close`, `early_post`, `holidays`, `early_closes` (`YYYY-MM-DD`, strings or TOML dates) |
| `weekly` | `tz`, `open` / `close` (`"Sun 20:00"`, local; may wrap the week), `daily_break = ["17:00", "18:00"]` |
| `24x7` | none |

`tz`: `America/New_York` \| `Europe/Paris` \| `UTC` (`domain/tz.rs`). Rows: `config.example.toml`, `tests/fixtures/xmarket/calendars.toml`.

### Weekend fade — `[xmarket.weekend_fade]`

Rule W for the exec tool `xm_weekend_fade`. Every key required: `calendar`, `universe`, `exclude`, `capped_top_n`, `min_abs_signal_bps`, `capped_notional_usd`, `shadow_account`, `shadow_initial_cash_usd`, `shadow_notional_usd`, `expected_edge_bps`, `anchor_max_age_secs`, `entry_max_age_secs`, `entry_lateness_max_secs`, `max_slippage_bps`.

| Load rule | |
|---|---|
| `calendar` is an `exchange` calendar; `universe` non-empty unique full `hyperliquid:<coin>` ids; `exclude` ⊆ `universe` | |
| `capped_top_n` ≤ names not excluded; `capped_notional_usd` ≥ $10 and ≤ `[risk] max_order_notional_usd`; `capped_top_n` × it ≤ `[risk] max_gross_exposure_usd` | |
| `shadow_account` ≠ `[risk] account`; `shadow_notional_usd` ≥ $10; `expected_edge_bps` ≥ `[risk] min_edge_bps` | |
| 0 < `entry_lateness_max_secs` < 54 000; 0 < `max_slippage_bps` < 10 000 | |
| `[risk]` + `[paper]`; `[recorder]` records `mkt_ctx/1` with `heartbeat_secs` / `min_interval_secs` ≤ `anchor_max_age_secs`; every name not excluded allowed by `[risk]`; `require_hedge_for` without `overreaction` | |
| an agent holding `xm_weekend_fade` ⇒ the section exists | |

Doc: `docs/xmarket-risk-paper-2026-09-30.md` § Weekend fade.

## Risk + paper — `[risk]`, `[risk.exits]`, `[paper]`

Every field required (no defaults); `[risk]` and `[paper]` come together. Example: `config.example.toml`; doc: `docs/xmarket-risk-paper-2026-09-30.md`.

| `[risk]` field | Meaning |
|---|---|
| `account` | ledger account in `<state dir>/ledger.db`, `[A-Za-z0-9._-]` |
| `mode` | `paper`; `live` refused until M3b |
| `venues` | `hyperliquid`, `robinhood`, `binance-usdm`, `bybit-linear`, `okx-swap`, `coinbase` |
| `min_lifecycle` | `discovered` … `live_approved`, ≥ `mapped` (M0 permits by `instruments_allow` only) |
| `instruments_allow` · `instruments_deny` | full `<venue>:<native id>`; deny wins; none in both |
| `max_order_notional_usd` ≤ `max_position_notional_usd` ≤ `max_gross_exposure_usd`; `max_asset_exposure_usd`, `max_venue_exposure_usd`, `max_net_exposure_usd`, `max_leverage` | caps after the fill; HL venue ⇒ order ≥ $10 |
| `daily_loss_limit_usd` ≤ `total_loss_limit_usd` | halts: daily clears 00:00 UTC; total only via `tengu risk resume` |
| `min_edge_bps` (≥ 0), `max_slippage_bps`, `min_depth_usd`, `require_hedge_for` | entry gates; `require_hedge_for` names `convergence`, `information_latency`, `overreaction`, `underreaction`, `related_asset` |
| `max_data_age_ms = { book, ctx, reference, quote }`, `max_skew_ms` | input freshness |
| `max_orders_per_min`, `max_open_orders` | per account |
| `kill_switch_file` | present = halted; absolute or `~/…`, outside every fs root |
| `allow_reduce_degraded` | stale data / halted: reduce-only + close still pass |
| `exits = { take_profit_bps, stop_loss_bps, max_hold_secs }` | `[risk.exits]`, all > 0: what `xm_exits` closes |

| `[paper]` field | Meaning |
|---|---|
| `initial_cash_usd` | starting cash of the `[risk]` account |
| `latency_ms` · `latency_jitter_ms` (≤ latency) | sleep, re-read the book, fill against it |
| `fee_tier` (0–6) · `staking_discount_pct` (0–40) | Hyperliquid fees |
| `order_types` | `market`, `ioc` |

| Load rule | |
|---|---|
| numbers finite and > 0 (exceptions above) | |
| exec tools (`paper_order`, `paper_close`, `xm_exits`, `xm_weekend_fade`) only on a private agent: no `description`, not `default`, no webhook `agent` (with or without `[risk]`; the tools check the caller again) | |
| `[default_scopes.sign_and_send_transaction]` + `[default_scopes.sign_message]` present without `wallets`; no agent scope grants one | Privy signing off |
| hardened sandbox | § Hardened sandboxes |

## Hardened sandboxes (`[risk]` or a `[solana]` signer)

| Rule | Enforced |
|---|---|
| every `claude_code` agent sets `builtin_tools_profile = "none"` (the CLI then runs without settings files, hooks, plugins, skills, CLAUDE.md discovery, auto-memory) | load + `engines/claude_code.rs` |
| no `[[mcp_servers]]`; no scope grants `shell_bins`; tools without a scope run no shell; no shell skill loads | load + runtime |
| the signer key, `<TENGU_HOME>/state`, `kill_switch_file` and the config file outside every `fs_roots` and `workspace` | load |
| a plan step's `compose` only narrows its base agent's tools / skills | `run-agent` |

Source: `src/config/hardening.rs`.

## Request budgets — `[rate_limits.<name>]`

| Field | Default | Rules |
|---|---|---|
| `per_minute` | required | ≥ 1, weight units / min (HL: 1200 / min / IP; `l2Book` 2, most info 20) |
| `burst` | `per_minute` | ≥ 1 |
| `exec_reserve` | 0 | < burst; weight reads leave for order calls |

Names `[a-z0-9_-]+`; used: `hyperliquid`, `geckoterminal`, `sec` (SEC EDGAR: `tengu history events`, a `sec_edgar` source row), `ted` (a `ted_search` source row) — a `[sources]` row names its own in `rate_limit`. One bucket per name per process — two processes each get the full budget.

## Runtime — `[runtime]` (`tengu run`)

| Field | Default | Rules |
|---|---|---|
| `shutdown_grace_secs` | 20 | ≤ 3600 |
| `max_decisions_in_flight` | 4 | ≥ 1; loop events across all loops (one per loop at a time); bounds `tengu webhooks` loops too |
| `max_queued_per_loop` | 64 | ≥ 1; one more is refused (warn, webhook 429) |
| `heartbeat_secs` | 5 | 1–3600 |
| `heartbeat_stale_secs` | 30 | > `heartbeat_secs`; `tengu doctor --live` fails past it |

Operator doc: `docs/runtime-2026-09-30.md`.

## Recorder — `[recorder]`

| Field | Default | Rules |
|---|---|---|
| `enabled` | `false` | needs `[xmarket]` and non-empty `schemas` |
| `schemas` | `[]` | `<name>/<version>` or `"*"` |
| `keep_data` | `[]` | full payload for these (else features only); each in `schemas` (or `"*"`) |
| `change_only` · `heartbeat_secs` | `true` · 300 | skip a row equal to its key's last one until it is this old (0 = never) |
| `min_interval_secs` | `{}` | per schema: at most one row per key per interval; each in `schemas` |
| `retention_days` | 30 | older day files deleted; 0 = keep all |

Rows land in `<state dir>/history/<YYYYMMDD>.db`; read with `tengu history range|asof`.

## Backtest — `[backtest]` (xlab)

Needs `[xmarket]` (`market.db` and run dirs live in its state dir). Doc: `docs/xlab-2026-10-01.md` § 5–9.

| Field | Default | Rules |
|---|---|---|
| `notional_usd` | 100 | > 0; per trade when a spec sets none |
| `bootstrap` | 2000 | 100–100 000 resamples per 95 % CI |
| `seed` | 7 | a rerun prints the same numbers |
| `gate` | none | a `[decision_loops.<name>]` — the Jev gate arm of `tengu backtest --gate` |
| `max_candidates` | 50 000 | 1–1 000 000; a run past it stops before any arm or file |
| `keep_runs` | 100 | 0 (keep all) or ≥ 10 run dirs under `<state dir>/backtests/`; the decision cache is never pruned, nor a run the bound generation's or the `[strategy_ranking]` lineage registry cites, nor a run a `<state dir>/strategy-rankings/*/latest.json` names |
| `costs."<prefix>"` | — | `taker_fee_bps` (required), `half_spread` (`{ model = "fixed", bps }` default 0 \| `{ model = "abdi_ranaldo", window_bars, floor_bps }` \| `{ model = "ctx", fallback_bps }`), `slippage_bps` (0), `funding` (`true`); bps 0–10 000; longest id prefix wins |
| `universes.<name>` | — | non-empty full instrument ids; `@<name>` in specs and on the CLI; names `[a-z0-9_]`, 1–48 |
| `splits."<full id>"` | — | `[{ at = "<RFC 3339>", ratio = <new shares per old> }]`: ratio finite, > 0, ≠ 1; sorted by `at` |
| `strategies.<name>` | — | a strategy spec: `kind` = `weekend_window` \| `daily_window` \| `move_trigger` \| `funding_carry` \| `pair_spread` \| `event_window` + its fields (`docs/xlab-2026-10-01.md` § 5) |

| Load rule | |
|---|---|
| every strategy parses and validates (kind, fields, bounds; unknown keys refused) | |
| its `@universe` exists; its calendar (`weekend_window`, `daily_window` with `days = "trading"`) is an `exchange` `[xmarket.calendars.<id>]` | |
| every id it trades has a `costs` prefix (unless the spec sets its own `costs`) | |

## Strategy ranking — `[strategy_ranking]` (xlab-w2)

The ranking contracts a sandbox runs (`lineage/rankings/<id>.toml`); `tengu ranking run`, the `strategy_ranking` tool and its feeds read it. Doc: `docs/strategy-ranking-automation-2026-10-08.md`.

| Field | Rules |
|---|---|
| `registry` | the lineage registry dir, relative to this config file (`"../../lineage"`) |
| `contracts` | non-empty, no id twice; each `<registry>/rankings/<id>.toml` |

| Load rule (`config/strategy_ranking.rs::section_errors`) | |
|---|---|
| the registry loads; the config is a `sandboxes/<name>/config.toml` and each contract's `sandbox` is `<name>` | |
| `[backtest]` present; every contract strategy is a `[backtest.strategies.<name>]` | |
| no Error finding on a contract (shape: `invalid_field`) — `seal_mismatch` excepted: an unsealed or changed contract loads, the publisher refuses it at run time | |

## Sources — `[sources]` (O2 source registry)

One row per approved external source; records go to the append-only `<TENGU_HOME>/state/<state>/sources.db` (never `market.db`). Read by `tengu sources` (the operator's fetch / import / asof / purge / kill switch) and the read-only `source_evidence` tool. Doc: `docs/source-evidence-2026-10-08.md`; example: `sandboxes/soe/config.toml`.

| Field | Default | Rules |
|---|---|---|
| `state` | required | one directory name under `<TENGU_HOME>/state/` (the `[xmarket] state` rule); the dir outside every `fs_roots` entry and agent `workspace` |
| `registry.<id>` | — | row id `[a-z0-9_]+` (the records' `source_id`) |
| `kind` · `class` · `trust` · `revision` | required | `sec_edgar` \| `ted_search` · `law_regulator` \| `company_primary` \| `registry_marketplace` \| `customer_demand` \| `independent_reporting` \| `social_inference` · `primary` \| `corroborating` \| `trigger_only` (the class must allow it: `independent_reporting` never `primary`, `social_inference` only `trigger_only`) · `immutable` \| `in_place` |
| `enabled` · `store_raw` | required | `false` = listed, never fetched · keep raw bodies |
| `hosts` | required | bare hosts, each inside `[egress] allow_hosts` (when set), none in `deny_hosts` |
| `auth` | required | `none` \| `user_agent_env:<VAR>` \| `api_key_env:<VAR>`; `sec_edgar` needs `user_agent_env` |
| `rate_limit` | required | names a `[rate_limits.<name>]` |
| `jurisdiction` · `language` | required | non-empty; `law_regulator`: an ISO 3166 code or `EU` |
| `license` · `terms_url` · `terms_sha256` · `terms_reviewed_at` | none | required when `enabled`: reuse terms, an https URL, sha256 of the reviewed terms page (64 lowercase hex), `YYYY-MM-DD` |
| `raw_retention_days` · `record_retention_days` | none | required when `enabled`; `0` = forever |
| `listing_max_age_days` | none | `registry_marketplace` only, required there, ≥ 1 |
| `forms` · `entities` | all kept forms · none | `sec_edgar` only: SEC forms, no repeat · `sec:cik:<10 digits>` (the CIKs `fetch` reads without `--ciks`) |
| `query` | none | `ted_search` only, required: TED Search syntax with `{from}` and `{to}` (each replaced by the day read, `YYYYMMDD`) |

The kill switch is not TOML: `tengu sources disable` appends a row to `sources.db` that refuses every fetch and import at once; `enable` lifts only that.

## Feeds — `[feeds.<n>]` (`tengu run`)

| Key | Kind | Default | Meaning |
|---|---|---|---|
| `kind` | — | required | `tool` (call a tool) \| `tick` (send a loop event); `poll`, `stream`, `ws`, `rows` refused (not built) |
| `every_secs` | both | — | 1 s – 7 d on the UTC grid |
| `windows` | both | `[]` | `{ days, from, to, every_secs }` local in `tz`, replaces `every_secs` inside; ≤ 32 |
| `at` | both | `[]` | `"Sun 18:00"`, `"daily 09:00"` in `tz`; ≤ 64 |
| `tz` | both | `UTC` | `America/New_York` \| `Europe/Paris` \| `UTC` |
| `jitter_pct` | both | 0 | 0–50 |
| `run_on_start` · `required` | both | `false` · `false` | `required`: `tengu doctor --live` fails when down or stale |
| `stale_after_secs` | both | 3 × longest interval (≥ 60); without `every_secs`: 8 days | ≥ 1 |
| `agent`, `tool` | tool | required | the agent must be able to call the tool (`tools` / `workspace_tools`) |
| `args` · `each` | tool | `{}` | `each = { coin = [...] }` fans out (≤ 500 calls a run; a key not also in `args`) |
| `concurrency` | tool | 1 | 1–32 |
| `target` · `event` | tick | required · `{}` | a `[decision_loops.<target>]`; the scheduler adds `ts_ms` |

At least one of `every_secs`, `windows`, `at`; names `[a-z0-9_-]+`. Call ids `feed:<name>:<slot ms>:<i>`.

## Skill lifecycle — `[skill_lifecycle]`

| Field | Default | Read by |
|---|---|---|
| `improver_agent` | required | `tengu skill evolve` |
| `default_max_evolve_cycles` | 3 | `skill evolve` (`--max-cycles` overrides) |
| `worktree_stale_hours` | 24 | `skill evolve` startup sweep of `.tengu/worktrees/` (0 = off) |
| `fixture_runner_agent` | none | nothing — `tengu eval` runs rows on the eval config's default agent |
| `default_rolling_window` · `max_per_run_reports` · `per_run_dir` | 10 · 10 · `"metrics"` | parsed, not read (`tengu eval` uses 10 and its `--max-runs` flag) |

## Sandboxes

`sandboxes/<name>/config.toml`, run from the repo root. Ten today: `jev-exec`, `lping`, `soe`, `storage-test`, `tor-check`, `unlimited`, `xmarket`, `xmarket-weekend`, `xlab`, `xlab-w2` — purposes and networks in the README § Sandboxes (`soe`: the O2 source registry, rows off, one read-only agent — `docs/source-evidence-2026-10-08.md`). Every one and `config.example.toml` must load (`config::risk::tests::every_sandbox_and_the_example_load`).

```bash
tengu chat --sandbox lping
cargo run --features claude_code -- chat --sandbox xlab   # xlab's agents use engine = "claude_code"
tengu run --sandbox xmarket                               # long-running: feeds, loops, webhooks
tengu backtest --sandbox xlab --strategy weekend_fade --split time:2026-07-01
tengu sources --sandbox soe list                          # the source registry; fetch needs an enabled, reviewed row
```

## Environment variables

| Var | Read by | Default | Purpose |
|---|---|---|---|
| `OPENROUTER_API_KEY` | `engines/mod.rs`, `bootstrap/memory.rs`, `bootstrap/orchestrator.rs`, `outbound/decisions.rs`, `tools/agentic_memory/`, `cli/run_agent.rs`, `inbound/webhooks.rs` | — | OpenRouter chat, embeddings, Jev decisions |
| `OPENROUTER_BASE_URL` | `engines/mod.rs`, `outbound/decisions.rs`, `tools/agentic_memory/` | `https://openrouter.ai/api` | API base |
| `OPENROUTER_REFERER` · `OPENROUTER_TITLE` | `engines/openrouter.rs` | unset | `HTTP-Referer` · `X-OpenRouter-Title` headers |
| `UNSLOTH_API_KEY` | `engines/local.rs` (the default `[agents.<a>.local] api_key_env`) | unset = no auth header | local server key |
| `TELEGRAM_BOT_TOKEN` | `inbound/telegram.rs` | — (required for `telegram`) | bot token |
| `TENGU_TELEGRAM_ALLOWED_USERS` | `inbound/telegram.rs` | unset | comma-separated ids merged with `[telegram] allowed_users` |
| `TENGU_HOME` | `config/paths.rs::resolve_tengu_home` | `~/.tengu` (`~/` expanded) | state root: config, vault, logs, `state/` |
| `TENGU_CONFIG` | `config/paths.rs::default_config_path` | `<TENGU_HOME>/config.toml` | config file; `-c` wins; pinned for children by the CLI |
| `TENGU_MASTER_PASSWORD` | `outbound/secrets.rs` | unset (prompt) | vault password; always redacted |
| `TENGU_SECRETS_LOADED` | `outbound/secrets.rs` | set by the first `tengu` that opens the vault | names of vault vars; children never prompt, register them for redaction |
| `TENGU_SESSION_ID` | `bootstrap/orchestrator.rs`, `engines/claude_code.rs`, `tools/agentic_memory/`, `outbound/memory/embedder.rs`, `outbound/egress.rs`, `tools/xm/exec_common.rs` | fresh UUID | session shared by planner + runner |
| `TENGU_AGENT_NAME` · `TENGU_AGENT_IPC` | set by `run-agent` (`cli/run_agent.rs`) · by its parent (`outbound/subprocess_runner.rs`, `=1`); read by `agentic_memory`, the egress audit, `cli/risk.rs`, `bootstrap/sandbox.rs` | unset | agent of a child process · IPC mode (`run-agent` refuses without `=1`); either set ⇒ `tengu risk halt` / `resume` refused |
| `TENGU_MEMORY_DATABASE_URL` | `tools/agentic_memory/` | — (required for `postgres_memory`) | Postgres + pgvector DSN |
| `TENGU_WIKI_COMPILER_MODEL` | `tools/agentic_memory/` | `anthropic/claude-sonnet-4-6` | `compile_wiki` model |
| `TENGU_EGRESS` | `outbound/egress.rs::install` | unset | resolved `[egress]` handed to children (JSON); wins over the child's config |
| `TENGU_TOR_PROXY` | `config/egress.rs::EgressConfig::resolved` | `socks5h://127.0.0.1:9050` | Tor proxy when `[egress].proxy` is unset (`docker-compose.tor.yml`: `socks5h://tor:9050`) |
| `HL_API_URL` | `outbound/hyperliquid/info.rs`, `outbound/backfill/hl.rs` | `https://api.hyperliquid.xyz` | Hyperliquid API base (testnet `https://api.hyperliquid-testnet.xyz`); tools read it only through `env_reads`, operator commands (`history backfill`, `backtest --fetch`) directly |
| `GECKO_API_URL` | `outbound/backfill/gecko.rs` | `https://api.geckoterminal.com/api/v2` | GeckoTerminal base; same rule as `HL_API_URL` |
| `SEC_USER_AGENT` | `outbound/backfill/sec.rs`; a `[sources]` `sec_edgar` row names its own variable (`auth = "user_agent_env:<VAR>"`, `outbound/sources/sec.rs`) | — (required for SEC) | the declared `User-Agent` SEC fair access asks for ("Name email"); a request header only, never stored or logged |
| `SOLANA_RPC_URL` | `outbound/solana/rpc.rs` | `https://api.mainnet-beta.solana.com` | Solana RPC; through `env_reads` only (else the public RPC, silently) |
| `TENGU_RISK_RESUME_SECRET_FILE` | `cli/risk.rs` | unset = no guard | `tengu risk resume` also asks for this file's content; refused when the file is missing, not 0600, over 4 KiB, empty, or inside an agent's `fs_roots` / `workspace` |
| `CHAIN_ID` · `EVM_RPC_URL` | `tools/crypto/helpers.rs` | `1` · `https://ethereum-rpc.publicnode.com` | Privy tools: chain id when a call omits it · JSON-RPC for receipts |
| `PRIVY_APP_ID` · `PRIVY_APP_SECRET` · `PRIVY_WALLET_ID` | `tools/crypto/helpers.rs` | — | Privy agentic wallet (through `env_reads`) |
| `TENGU_BRIDGE_*` (`AGENT`, `TOOLS`, `WORKSPACE`, `GRANT_WORKSPACE`, `SCOPES`, `MCP_SERVERS`, `SUMMARY_FILE`, `TRANSCRIPT_FILE`, `MAX_RESULT_CHARS`) | `inbound/mcp_bridge.rs` (names: `outbound/bridge_env.rs`) | set by the Claude Code engine | engine → bridge contract — `docs/mcp-bridge.md` |
| `TENGU_PERSISTENT_STORE_CHUNK_SIZE` · `_OVERLAP` | forwarded by `engines/claude_code.rs` | unset | read by nothing — the bridge takes `[memory]` from its config |
| `TENGU_TUI_METRICS` · `TENGU_TUI_RAG_DEBUG` | `tui/mod.rs` | off | metrics line · planner recall hits as System bubbles |
| `TENGU_GPU_HINT` · `CUDA_VISIBLE_DEVICES` | `config/mod.rs::detect_gpu` | auto-detect | `none\|cpu\|off\|false` or `gpu\|cuda\|metal\|mps\|on\|true` for `runtime_profile = "auto"` |
| `NO_COLOR` | `inbound/eval.rs` | unset | plain eval table |
| `RUST_LOG` | `cli/mod.rs` (`tracing_subscriber::EnvFilter`) | `tengu=info` | log filter; one `metrics` line per LLM call at info |
| Claude CLI child env | `engines/claude_code.rs` | — | strips a parent Claude Code session's vars (`CLAUDECODE`, `CLAUDE_PID`, `CLAUDE_EFFORT`, `CLAUDE_CODE_*` but auth / provider) and `ANTHROPIC_API_KEY` |
| `LYREBIRD_RS_DIR` · `NETWORK` · `SANDBOX` | `Makefile` | `../lyrebird-rs` · the config's `[egress] network` · none | Tor image source · compose network · sandbox config to mount |
| `TENGU_FEATURES` · `TENGU_WEBHOOK_PORT` | `docker-compose.yml` | `openrouter,telegram` · 7080 | image features · host port |
| Tests only | `tests/*.rs`, `outbound/solana/rpc.rs` tests | — | `TENGU_MATRIX_LOCAL_BASE_URL` (local engine-matrix leg), `TENGU_CONFORMANCE_ONLY` / `_VERBOSE`, `TENGU_REGEN_CODE_MAP`, `TENGU_REGEN_SOURCE_EVAL` (the source eval set), `TENGU_CAPTURE_FIXTURES` |

## Feature flags

| Flag | Default | Purpose |
|---|---|---|
| `openrouter` | on | marker only — no code is gated on it; OpenRouter and `local` engines are always built |
| `telegram` | on | Telegram bot channel |
| `claude_code` | off | Claude Code CLI backend |
| `postgres_memory` | off | Postgres + pgvector agentic memory, `tengu agentic-memory-server` |
| `webhooks` | off | `tengu webhooks`, webhook routes under `tengu run` |

## Validation summary

| Errors (load fails) | Warnings (load continues) |
|---|---|
| unknown top-level key, unknown `[agents.<id>]` key, unknown key in a `deny_unknown_fields` section (§ Root sections) | a `local` agent on the default `context_window` |
| no agent; more than one `default`; engine / lens / flow / profile values outside their lists | a routable agent without `workspace` whose tools write (`write_file`, `manage_skill`, `skill_distill`, `apply_improver_proposal`) |
| `workspace_tools` outside `WORKSPACE_TOOLS` | `[telegram] tool_approvals` / `approve_only` set (not implemented) |
| `[egress]`, `[solana]`, hardening, `[xmarket]`, `[risk]` / `[paper]`, `[rate_limits]`, `[runtime]`, `[recorder]`, `[backtest]`, `[strategy_ranking]`, `[sources]`, `[feeds]`, `[decision_loops]` rules (sections above) | |

## Reset

```bash
tengu prune                              # <TENGU_HOME>/state/flows, memory/, logs/ (keeps config, secrets, skills)
tengu prune --sandbox <name>             # + each workspace's memory/, .tengu-tasks/, .tengu-attachments/, storage/, scaffold project dirs
tengu prune --sandbox <name> --hard      # empties each workspace root (every child), keeps the root
```

| Fact | Detail |
|---|---|
| Never pruned | `<TENGU_HOME>/state` beyond `state/flows` — the xmarket state dirs (`ledger.db`, `runtime.db`, `history/`, `market.db`, `backtests/`), the `[sources]` state dir (`sources.db`) and `solana-writes.db`; the sandbox config, secrets, managed skills |
| `--hard` | needs `--sandbox` (without it: global state only, with a note); the workspaces are the agents' `workspace` dirs — an xmarket workspace loses its `.tengu/observations.db`; does not clear the Postgres `agentic_memory` store — `make clean` does (global, destructive) |
| Logs | every prune deletes `<TENGU_HOME>/logs/` (`tengu.log`, `egress.jsonl`, `decisions.jsonl`, `risk.jsonl` — `ledger.db` keeps the canonical verdicts) |
| Manual | `rm -rf ~/.tengu` wipes everything, the paper ledgers and `market.db` included |

## Related
- `docs/architecture-2026-04-27.md` (canonical) · `docs/code-map.md` §3
- `docs/engine-backends.md` · `docs/skills.md` · `docs/tools.md` · `docs/webhooks-2026-05-11.md`
- `docs/runtime-2026-09-30.md` · `docs/xmarket-risk-paper-2026-09-30.md` · `docs/xlab-2026-10-01.md` · `docs/source-evidence-2026-10-08.md`
