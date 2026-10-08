# Tools — where they live, how to add one, how agents get them

## Pick the lightest option

| Need | Use | Rust? | Lives in |
|---|---|---|---|
| Call an HTTP API | Skill that teaches `http_request` | no | `skills/<name>/SKILL.md` |
| Reuse an existing tool server | `[[mcp_servers]]` in the sandbox config | no | `sandboxes/<name>/config.toml` |
| New native capability | Rust tool in the catalog | yes | `src/adapters/outbound/tools/<name>/` |

## Built-in tools (`src/adapters/outbound/tools/`)

| Dir | Tools | Gate |
|---|---|---|
| `workspace/` | `read_file`, `list_directory`, `write_file`, `run_command` | always |
| `memory/` | `memory_ingest`, `memory_search` | `[memory] enabled` |
| `memory/` | `persistent_store` | opt-in |
| `http/` | `http_request` | always |
| `crypto/` | `sign_and_send_transaction`, `sign_message`, `get_wallet_address` (Privy: `wallets`, then `PRIVY_*` through `env_reads`, every request through `[egress]` + `net_hosts`, audited), `abi_encode`, `hex_to_uint256` | always |
| `skill_resource/`, `view_skill/` | `skill_resource`, `view_skill` | always |
| `cache/` | `shared_cache` | opt-in |
| `agentic_memory/` | `agentic_memory` | opt-in, feature `postgres_memory` |
| `skill_lifecycle/` | `skill_distill`, `apply_improver_proposal` (+ implicit `compress_and_store`) | opt-in |
| `manage_skill/` | `manage_skill` | opt-in |
| `solana/` | observe/plan: `sol_price`, `dlmm_pools`, `dlmm_pool`, `dlmm_positions`, `jup_perps`, `solana_wallet`, `solana_tx`, `lp_snapshot`, `lp_swap_plan`, `hedge_decide`, `lp_decide`; writes: `solana_close_token_accounts`, `jupiter_swap`, `dlmm_open_position`, `dlmm_close_position`, `jup_perps_order` — `mode = simulate` (default) \| `send` (args / keys / hosts / send rules: `docs/typed-observations-2026-09-24.md`) | opt-in, one row per name |
| `hyperliquid/` | `hl_ctx` — typed, cached market context (`mkt_ctx/1`, `mkt_instrument/1`, `hl_sweep/1`); `hl_book` — L2 book, depth, slippage per notional (`hl_book/1`); args / keys / weights: `docs/typed-observations-2026-09-24.md` § Hyperliquid tools | opt-in, one row per name |
| `xm/` | `risk_status` — the `[risk]` paper account's risk state (`risk_state/1`): halt + kill switch, equity, P&L, loss headroom, exposure, order rate; `paper_positions` — the account at fresh marks (`paper_positions/1`); exec tools `paper_order`, `paper_close` — orders through the `[risk]` gate inside the tool, filled against a live HL book (`paper_fill/1`, `paper_close/1`) — and `xm_exits`, the exit rules (deadline, max hold, `[risk.exits]` stop-loss / take-profit) closing due positions through the same gate (`xm_exits/1`, run by a `kind = "tool"` feed), and `xm_weekend_fade`, the weekend-fade rule W (`[xmarket.weekend_fade]`: one step of the window per call — capped fades through the gate, shadow fades through the shadow gate, shadow exits; `xm_weekend/1`, run by a feed every 60 s); every verdict a `ledger.db` row + a `logs/risk.jsonl` line. Need `[xmarket]` + `[risk]` + `[paper]`; `docs/xmarket-risk-paper-2026-09-30.md` | opt-in, one row per name; exec tools only on a private agent (no `description`, not `default`, no webhook `agent`) |
| `xlab/` | `market_history` — one instrument's stored history in a window from the warehouse `<state dir>/market.db` (`mkt_history/1`, ttl 0): bar count, last close, return / volatility / max drawdown in bps, average volume, mean funding APR, gaps, a ≤ 48-row bar table, what else is stored — the sandbox's `[backtest.splits]` applied as a backtest applies them (a note each); `fetch = true` first backfills the missing part (HL bars + funding for `hyperliquid:` ids, GeckoTerminal pool bars for `solana:` / `robinhood:` ids with `pool`). `backtest` — a `[backtest.strategies]` name or an inline strategy spec (`spec`, the Architect's level-2 capability) over `[from, to)` with an optional `split`, run on `market.db` by the backtest use case (rules arms `research` + `capped`; no Jev gate, no network): `backtest/1:<run id>` (ttl 0), a run dir `<state dir>/backtests/<run id>/`; a bad spec is refused with every problem named; a split runs its in-sample half only until `holdout: true` (a holdout read, appended to `<state dir>/backtests/holdout-reads.jsonl`, `holdout read #n for this spec`); more than 50 000 candidates is refused; `run_id` (+ `view` periods / instruments / trades / notes, `arm`, `limit`) reads a stored run's rows by run id instead — never a path. Both need `[xmarket]` (else `state_dir_missing`), `backtest` also `[backtest]` (else `backtest_config_missing`); read-only. `docs/typed-observations-2026-09-24.md` § Market history, § Backtest, `docs/xlab-2026-10-01.md` § 8 | opt-in, one row per name; any agent (not in `XM_TOOLS`: no workspace rows) |
| `sources/` | `source_evidence` — the as-of evidence packet of the sandbox's source store `<TENGU_HOME>/state/<sources.state>/sources.db` (`source_asof/1:<source\|all>:<event\|entity\|all>:<at ms>`, ttl 0): current typed facts of the approved sources (SEC EDGAR filings, EU TED notices) visible at `at` (`captured` = read by then, `knowable` = public by then), corrections, withdrawn / unparsed items, confidence per event (a syndicated copy counts once), conflicts, rule issues, per-source freshness, citations; strict args `at` (never after now), `mode`, `source`, `entity`, `event_key`, `from` / `to` (publication window), `limit` (1–50, default 10; text ≤ 6 000 bytes, the rest `omitted`); every source free text inside one `source-text` fence. Read-only: no fetch argument (the operator fetches with `tengu sources fetch`); no `sources.db` yet ⇒ an empty packet, nothing created; no `[sources]` ⇒ `sources_state_missing`. `docs/source-evidence-2026-10-08.md` § 8 | opt-in; any agent (`sandboxes/soe`: the only tool of `soe_reader`) |

The list is `catalog()` in `tools/mod.rs` — one `ToolEntry` row per always-on group, one per opt-in name. That row drives in-process registration, the MCP bridge (Claude Code subagents), and the tool list the model sees. `tengu tool list` (hidden) prints every catalog name as JSON: 46 without `postgres_memory` (2026-10-08), 47 with it (`agentic_memory`); `compress_and_store` is not a catalog row (appended to plan steps).

## Add a Rust tool

1. `src/adapters/outbound/tools/<name>/mod.rs`: `impl Tool` (from `ports::tool`), a `ToolPlugin`, `tool_defs()`. First line of `execute` = `ctx.scope.check_*(..)` or `// scope: pure-compute` (`tests/scope_lint.rs`).
2. `pub(crate) mod <name>;` + one `ToolEntry` in `catalog()`.
3. Opt-in only: add the name to `src/domain/tools.rs::WORKSPACE_TOOLS` (`catalog_tests` fail otherwise).
4. `cargo test --bin tengu -- catalog schema_lint && cargo test --test scope_lint && cargo test --test bridge_conformance`.
5. Every engine — no exceptions (operator rule 2026-09-30): the tool must work the same under `engine = "openrouter"` and `"local"` (in-process) and `"claude_code"` (through `tengu mcp-bridge`). Keep the input schema in the subset all three accept (§ Tool schema subset — the lint covers every catalog row automatically), keep results within a local model's context window, and add:
   - a bridge conformance case — one `case("<tool>", json!({..}))` row in `tests/bridge_conformance.rs::cases()` with its scope TOML, mock replies (fixtures under `tests/fixtures/<area>/`) and expected outcome; `bridge_conformance` fails for a catalog tool without one (`docs/mcp-bridge.md` § Testing);
   - its engine-matrix smoke: the tool in a tool set of `tests/engine_matrix.rs` (`Set::tools`, the scripted `goal`, what the answer must hold; `every_catalog_tool_has_a_live_leg` fails CI for a catalog tool in no set) and in the fixture agents' `tools` + scopes — `tests/fixtures/engine_matrix/*.toml` for a `[risk]` sandbox, `tests/fixtures/engine_matrix/open/*.toml` otherwise (memory, shell, `[[mcp_servers]]`) — then the live legs on every engine (`docs/engine-backends.md` § Engine matrix). An exec tool (private agents only) goes in the xm set: its legs run `tengu tool turn` on the fixtures' private `xm_*` agents instead of `run-agent`.

   Open parity gaps: `docs/mcp-bridge.md` § Parity rule.

## Tool schema subset

`cargo test --bin tengu schema_lint` (`tools/schema_lint.rs`) checks every definition an engine can receive — every catalog row (opt-ins, memory, `agentic_memory` under `postgres_memory`), `compress_and_store`, `[[mcp_servers]]` tools (fixture server) and shell-skill tools (fixture SKILL.md) — and lists every violation at once.

Real `[[mcp_servers]]` tools are linted at runtime, on every discovery path (`mcp_client::linted_tool`: the executor's `McpPlugin`, the bridge, `enumerate_tools` for the planner registry and the Claude Code bridge list): a violator is dropped with a warn naming the server, the tool and each rule, so one bad external schema cannot break a Gemini or Ollama turn.

| Rule | Rejected live (2026-09-30) | Otherwise why |
|---|---|---|
| Name `[a-zA-Z0-9_-]`, first a letter or `_`, ≤ 64, unique per agent | `.`: OpenAI, Claude · leading digit: Gemini · duplicate: Claude, Gemini | 64 = OpenAI-style limit (Claude, Gemini: 128) |
| Root `type: "object"` with `properties`; no top-level `oneOf` / `anyOf` / `allOf` / `not` / `enum` | non-object root: OpenAI · top-level `anyOf`: OpenAI, Claude | — |
| No `oneOf` / `anyOf` / `allOf` / `not` / `const` / `$ref` / `$defs` at any depth | — | Gemini's schema and Ollama's typed tool structs have none; `const` = one-value `enum` |
| Every nested schema: one `type` (string, number, integer, boolean, array, object) or an `enum`; arrays have one `items` | — | Ollama parses `type` into typed structs; element types must be declared |
| `required` ⊆ `properties`, every level | — | a contract bug |
| `enum`: non-empty list of strings; `format`: `date-time` on strings only | — | Gemini: string enums, formats `enum` / `date-time` only |
| Tool description ≤ 1024 chars, field description ≤ 512 | — | OpenAI's documented limit; local context windows |

Live check 2026-09-30 (probe kept out of the repo): all 38 definitions accepted by `google/gemini-2.5-flash-lite`, `anthropic/claude-haiku-4.5`, `openai/gpt-4o-mini` (OpenRouter) and parsed by Ollama 0.24; the Claude CLI serves 78-char bridged names (`mcp__tengu-tools__…`).

## Add a typed (cached) tool

Use when a decision loop or the cache should consume the result (`docs/typed-observations-2026-09-24.md`).

| Step | Where |
|---|---|
| Result struct `impl Observed`: `SCHEMA = "<name>/1"`, `subject()` = full ids joined by `:`, `headline()` (full ids, fits the 200-char line 1), `features()` ≤ 32 scalars, `status()` / `errors()` when fields can fail (`Field<T>`, never 0) | `src/domain/...` (pure) |
| `execute`: `CachePolicy::new(SCHEMA, subject, ttl_ms, args)` → `application::observe::observe(store, name, &policy, now, fetch)` → `Ok(ToolOutput::observed(obs, now))` | the tool |
| Store: `SqliteObservationStore::open(ctx.workspace)` once in the plugin, `None` on failure (read live) | plugin, like `tools/solana/mod.rs` |
| Input schema: optional `max_age_secs` (0 = live). Tests: `assert_features_ok`, line 1 ≤ 200 with max-length ids | defs + unit tests |

## Give it to an agent

```toml
[agents.researcher]
engine = "openrouter"
model = "anthropic/claude-sonnet-4-6"
description = "..."                       # makes it routable by the planner
tools = ["http_request", "read_file", "shared_cache"]   # allow-list; "shared_cache" also opts in
skill_packages = ["my-skill"]

[agents.researcher.scopes.http_request]  # per-tool permissions (deny-by-default per field)
net_hosts = ["api.example.com"]
env_reads = ["EXAMPLE_API_KEY"]

[default_scopes.http_request]            # fallback for agents with no own entry
net_hosts = ["*"]
```

| Field | Effect |
|---|---|
| `tools` | Allow-list on every surface: chat (TUI, Telegram, webhooks, eval, `tengu tool turn`), plan steps (`tengu run-agent`), `tengu tool call`, feeds, decision loops, the Claude Code bridge — `[[mcp_servers]]` tools included (`bootstrap::tools::agent_base_tools`). Empty = every always-on tool plus configured opt-ins. Opt-in names listed here are switched on. A skill gets only these — list every tool its `SKILL.md` calls (e.g. storage-test's `telegram-rag-ingest`: `http_request` → `write_file` → `persistent_store`). |
| `workspace_tools` | Older way to switch on opt-in tools; merged with `tools`. |
| `skill_packages` | Skills the agent loads (doc skills into the prompt; shell skills as tools on every surface, the bridge included). An agent that runs no shell (`[risk]` / signer sandbox) loads no shell skill |
| `scopes.<tool>` | `fs_roots`, `net_hosts`, `env_reads`, `shell_bins`, `wallets`. Per-agent entry replaces `default_scopes` wholesale. A plan step (`run-agent`, its bridge) adds its workspace to every configured scope's `fs_roots` — except a deny-all one (every field empty): an explicit deny stays a deny. `shell_bins` (`run_command`, shell skills) gates the first command word only — leading `NAME=value` skipped (`DEK=… node` → `node`, `domain::scope::shell_command_binary`): a guard rail, not a sandbox (`;`, pipes, `$( )`, `node -e` run unchecked). Example: jev-exec's `[agents.architect.scopes] run_command`. |
| `write_file` (every sandbox) | Resolves the path first (symlinks, `..`), then refuses `.tengu/`, `.claude/`, `.git/`, `skills/` at any depth and `CLAUDE.md` / `CLAUDE.local.md` / `AGENTS.md` / `.mcp.json` files, case-insensitive (`tools/args.rs::validate_write_path`, `domain::scope::protected_write`); in a hardened sandbox (`[risk]` / signer: `AgentConfig::hardened`) also the system-prompt files `MEMORY.md` / `USER.md` / `IDENTITY.md` / `PROFILE.md` / `CONTEXT.md` (`protected_write_in` — elsewhere an agent may update its profile); `manage_skill` / `apply_improver_proposal` resource paths and `agentic_memory` wiki titles refuse the same names |
| Solana tools | need `fs_roots` = the workspace (observation store), `net_hosts` per tool, `env_reads = ["SOLANA_RPC_URL"]` (else the public RPC is used silently). Working example: `sandboxes/lping/config.toml` |
| Hyperliquid tools | need `fs_roots` = the workspace (observation store), `net_hosts = ["api.hyperliquid.xyz"]`, `env_reads = ["HL_API_URL"]` (testnet override; unset = mainnet). Request weight budgets against `[rate_limits.hyperliquid]`. Example: `config.example.toml` |
| xlab tools (`market_history`, `backtest`) | need `[xmarket]` (the warehouse and the run dirs live in its state dir, outside every fs root) and `fs_roots` = the workspace (observation store); a `market_history` `fetch` also needs `net_hosts = ["api.hyperliquid.xyz", "api.geckoterminal.com"]` (each request checked, `[egress] allow_hosts` the ceiling) and reads `HL_API_URL` / `GECKO_API_URL` only through `env_reads` (unset = the public hosts); budgets `[rate_limits.hyperliquid]`, `[rate_limits.geckoterminal]` (per process; HL's 1200 / min is per IP — xlab keeps 600, burst 200, for the xmarket runs on the same host). `backtest` needs no host and no env, plus `[backtest]` (costs, universes, strategies); its run dirs are read with `backtest` `run_id` (by id), never with `read_file`. Examples: `sandboxes/xlab/config.toml`, `tests/fixtures/engine_matrix/openrouter.toml` |
| `source_evidence` | needs `[sources]` (its `sources.db` lives in the sources state dir, outside every fs root — the load refuses it inside one) and `fs_roots` = the workspace (observation store); no host, no env. Example: `sandboxes/soe/config.toml` |
| Privy crypto tools | a configured scope needs `wallets = ["default"]` (to sign; empty = Privy signing off), `net_hosts = ["api.privy.io"]` (+ the `EVM_RPC_URL` host for receipts), `env_reads = ["PRIVY_APP_ID", "PRIVY_APP_SECRET", "PRIVY_WALLET_ID"]` (+ `CHAIN_ID`, `EVM_RPC_URL`); `[egress] allow_hosts` applies. No scope = the permissive fallback. Example: `tests/fixtures/engine_matrix/open/openrouter.toml` |
| Solana write tools, `mode = "send"` | `[solana] signer_key_file` + `wallets = ["<full pubkey>"]` in that ONE agent's own scope for the tool (never `[default_scopes]`; the agent has no `description`); signing-sandbox rules in `src/config/solana.rs` + `src/config/hardening.rs`. Without both they only simulate |
| `[risk]` sandbox (xmarket) | hardened like a signer (`src/config/hardening.rs`: no shell scope, no `[[mcp_servers]]`, `claude_code` only with built-ins off, `<TENGU_HOME>/state` / kill-switch file / config file outside every fs root); `[default_scopes.sign_and_send_transaction]` + `[default_scopes.sign_message]` required without `wallets` (Privy signing off); exec tools (`domain/tools.rs::XM_EXEC_TOOLS`) only on a private agent — rules `src/config/risk.rs`, doc `docs/xmarket-risk-paper-2026-09-30.md`; a plan step's `compose` may only narrow its base agent's `tools` / `skill_packages` (`bootstrap::tools::compose_agent`, a signer sandbox too) |
| `compress_and_store` | Added to every subagent automatically — never list it. |

## MCP servers

```toml
[[mcp_servers]]
name = "github"                  # tools appear as "github__<tool>"
transport = "stdio"              # or "http" + url = "..." + auth = { type = "bearer", token = "$TOKEN" }
command = ["npx", "-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "$GITHUB_TOKEN" }
```

List them in an agent's `tools` like any other tool (`tools = ["github__create_issue"]`); an empty `tools` gets every one. A server tool outside a non-empty `tools` runs on no surface (in-process or bridged). A server tool whose schema leaves § Tool schema subset is dropped at discovery with a warn (`mcp: tool schema outside the subset …`, naming server, tool and rule).

| Agent kind | Sees `[[mcp_servers]]` tools? | How |
|---|---|---|
| Plan-step subagent, `engine = "openrouter"` | yes | `run-agent` executor connects, advertises them |
| Plan-step subagent, `engine = "claude_code"` | yes | engine names the servers to the tengu bridge (`TENGU_BRIDGE_MCP_SERVERS`, names only: the bridge takes them from its loaded config, so no `${VAR}`-expanded value sits in the temp `--mcp-config`), which proxies them under the egress policy — as `mcp__tengu-tools__<server>__<tool>` |
| In-process OpenRouter agent (TUI, Telegram) | yes | same executor |
| In-process Claude Code agent (TUI, Telegram) | yes | servers listed once at agent setup and added to the bridge list; bridge proxies them |
| Webhook agent, `engine = "claude_code"` | yes | webhook turn hands its executor's tool list + servers to the bridge (`bootstrap::tools::bridge_inputs`) |
| Eval / evolve agent, `engine = "claude_code"` | yes | same (`bridge_inputs`); its bridge loads the expanded eval config (`eval.rs::bridge_config_file`); stubbed rows refused (`docs/mcp-bridge.md` § Known Limitations) |

Names use `__` because model APIs reject `.` in tool names.
