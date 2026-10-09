# xmarket — gap details (2026-09-29)

Reference notes behind [`xmarket-tracker-2026-09-29.md`](xmarket-tracker-2026-09-29.md): one entry per tracker item, in tracker order. Look an item up by id before starting it — not meant to be read end to end.

| Read | Rule |
|---|---|
| Source | 8 slice researchers + a completeness critic, then a two-lens review, 2026-09-29; `file:line` refs are as of commit `9bebd10` |
| Rules | Every task follows tracker § 0 (rules R1–R13, definition of done); the operator's decisions are in the PRD addendum; how to execute: `xmarket-build-plan-2026-09-30.md` |
| Precedence | Tracker § 4 Conventions win over any entry; entries carry a **Tracker note** where they diverge. Headings, size and kind follow the tracker; the bullets are the original research |
| Known divergences | Postgres event store → SQLite; store paths (`state/xm-paper.db`, `state/xmarket.db`, `state/history/<sandbox>/`, `.tengu/series.db`, `.tengu/loops.db`, `.tengu/feeds.db`) → `<TENGU_HOME>/state/xmarket/…`; keys `hl_ctx/1`, `xm_mkt/1`, `xm_focus/1`, `hl_book/1:<coin>`, `rh_quote/1:<contract>` and ids `hl:<coin>` → conventions 1–2; loop names other than the four in convention 8; architect `researcher` / unhardened Claude Code → one `xm_architect` (OpenRouter or hardened `claude_code`, conventions 11–12); any "never `claude_code`" or "no `claude_code` agent" rule → hardened `claude_code` allowed (conventions 12, 20); `[news.sources]`, `[news.hosts]`, `[budget]`, `[budgets.<name>]` → `[feeds.<n>]`, `[spend]`, `[rate_limits.<name>]`; §16 Rust enums in `info-extract` / `jev-research-submit-tool` → runtime-validated strings; `--liveness` → `--live`; HIP-3 dex defaults naming `flx`, `km`, `cash` (0 listed markets) |
| Status (2026-10-08) | an entry's meta-line status and **Have** bullets are the 2026-09-29 research, not updated; the tracker's ✅ / 🟡 column (with commits) is current — E0 + wave W1 done (#20), `info-edgar`'s adapter built in Phase 7, the M7 replay pieces pulled forward as xlab |
| Meta line | milestone · size · kind · status · layer · PRD refs |
| After | dependencies with their milestone; one in a later milestone means this item ships its early subset first |
| Merged ids | an absorbed item's notes follow its kept item as **Absorbed** bullets |

## E0 — engine parity foundation

### `x-tool-schema-lint` — Test: every catalog tool schema stays in the subset OpenRouter's providers, local OpenAI-compatible servers and Claude accept (name `^[a-zA-Z0-9_-]{1,64}$`, object root, no top-level `$ref` / `oneOf`, bounded description length); CI fails otherwise

E0 · S · rust · missing · tests · PRD addendum

- **Have:** No test checks tool input schemas against provider rules (`rg -i 'schema_lint|validate_schema' src tests` → 0 hits). The first failure of this class was fixed ad hoc on 2026-09-23: `[[mcp_servers]]` tools were renamed `{server}__{tool}` because model APIs reject `.`.
- **Need:** A test over the catalog's advertised definitions (`advertised_defs`): name matches `^[a-zA-Z0-9_-]{1,64}$`; input schema root is `type: object` with `properties`; no top-level `oneOf` / `anyOf` / `allOf` / `$ref`; `required` ⊆ `properties`; tool description ≤ 1,024 chars and field descriptions ≤ 512; enums are strings; no `format` values local grammar-constrained servers reject. It fails `cargo test` for any catalog tool (and the `[[mcp_servers]]` fixture) outside the subset. Document the subset in `docs/tools.md`.
- **No-Rust path:** None — a test.
- **Evidence:** rg over src + tests (2026-09-30); SESSION_HANDOFF 2026-09-23 `{server}__{tool}` rename.

### `x-bridge-parity` — `tengu mcp-bridge` runs every catalog tool exactly as in-process: loads the sandbox config (`ClaudeCodeEngine` forwards `TENGU_CONFIG` + the agent name), uses that agent's `AgentConfig` (not the default `main`), the process `SecretRegistry` + `SanitizedToolExecutor`, the agent's `no_shell`, and the MCP request id as `ToolCtx.call_id`

E0 · M · rust · missing · inbound (mcp_bridge) + outbound (engines/claude_code, bridge_env) + bootstrap · PRD §26 §30 §32

- **Have:** `build_bridge_executor` (`src/adapters/inbound/mcp_bridge.rs:412-560`) builds tools from `Config::default()` and the default `main` agent (:427-432), a fresh empty `SecretRegistry` (:446) while the CLI builds a populated one (`src/adapters/inbound/cli/mod.rs:319-361`), no `SanitizedToolExecutor`, and `no_shell = false` (:537-543). The env contract (`src/adapters/outbound/bridge_env.rs`) carries scopes and `[[mcp_servers]]` only; `ClaudeCodeEngine` does not forward `TENGU_CONFIG`.
- **Need:** Operator rule 2026-09-30 (convention 20): every catalog tool behaves the same through the bridge as in-process. (1) `ClaudeCodeEngine` writes `TENGU_CONFIG` (the resolved sandbox file) and `TENGU_BRIDGE_AGENT` (agent name) into the bridge env (`bridge_env.rs`). (2) The bridge loads that config, takes `[agents.<name>]` (falls back to today's default only when both are absent, with a warn), and builds `PluginCtx` from it — so `[risk]`, `[paper]`, `[solana]`, `[feeds]`, `[xmarket]` and `workspace_tools` match. (3) Register the inherited secrets (`TENGU_SECRETS_LOADED` names → env values) in the bridge's `SecretRegistry` and wrap the executor in `SanitizedToolExecutor`. (4) `no_shell` from the agent (`AgentConfig::no_shell_fallback`). (5) MCP `tools/call` request id → `ToolCtx.call_id` (with `risk-exec-idempotency-ids`). Tests: bridge built from a fixture sandbox sees its `[risk]` section; a secret value in tool output comes back redacted; `no_shell` removes the fallback shell.
- **No-Rust path:** None: the bridge wiring is code.
- **Evidence:** Read `src/adapters/inbound/mcp_bridge.rs`, `src/adapters/outbound/bridge_env.rs`, `src/adapters/outbound/engines/claude_code.rs:592-601` (2026-09-30); `rg -n TENGU_CONFIG src/adapters/outbound/engines/claude_code.rs` → no hits.

### `x-claude-code-hardening` — `--strict-mcp-config` in `engines/claude_code.rs` (no user plugin MCP servers); load rule: in a `[risk]` or signing sandbox every `claude_code` agent needs `builtin_tools_profile = "none"`; replaces the Solana signer's blanket `claude_code` refusal (`src/config/solana.rs`) with that rule

E0 · M · rust · missing · outbound (engines/claude_code) + config · PRD §26 §28
After: `x-bridge-parity` (E0)

- **Have:** The engine passes `--mcp-config` and `--allowedTools` (`engines/claude_code.rs:592-601`) but not `--strict-mcp-config`, so `claude -p` also loads the user's global plugin MCP servers (SESSION_HANDOFF open item). `builtin_tools_profile = "none"` exists (used by `sandboxes/jev-exec`). A configured Solana signer makes `Config::load` refuse every `claude_code` agent (`src/config/solana.rs`).
- **Need:** (1) Always pass `--strict-mcp-config` so a Claude Code agent sees only the tengu bridge (plus the sandbox's own `[[mcp_servers]]`, proxied by the bridge). (2) Load rule: in a `[risk]` sandbox or with a signer configured, every `claude_code` agent must set `builtin_tools_profile = "none"` — no Bash / Edit / Write that could touch `ledger.db`, the kill-switch file or the key. (3) Replace the Solana signer's blanket `claude_code` refusal with rule (2); key-outside-fs-roots and wallet-grant rules stay. (4) The bridge runs with `no_shell` in those sandboxes (`x-bridge-parity`). Tests: config load accepts a hardened `claude_code` agent in a `[risk]` sandbox and refuses one with built-ins on; the CLI args include `--strict-mcp-config`.
- **No-Rust path:** None: CLI flags and load rules are code.
- **Evidence:** `claude --help` lists `--strict-mcp-config` (2026-09-30); `rg -n strict-mcp src` → no hits.

### `x-bridge-conformance-test` — Conformance harness: each catalog tool runs once in-process and once through a real `tengu mcp-bridge` subprocess on the same fixture config; text + store rows must match; CI fails for a catalog row without a case (convention 20)

E0 · M · rust · missing · tests · PRD §30 §32
After: `x-bridge-parity` (E0)

- **Have:** `tests/mcp_bridge_external.rs` proves the bridge proxies `[[mcp_servers]]` (fixture `tests/fixtures/fake_mcp_server.sh`); no test compares a catalog tool's bridge result with its in-process result.
- **Need:** Enforces convention 20 like `tests/scope_lint.rs` enforces scope checks: a table of conformance cases, one per catalog tool name, each with a fixture sandbox config, args and a replay HTTP transport (no network). The harness calls the tool in-process (`build_tool_executor`) and through a real `tengu mcp-bridge` subprocess over stdio, and asserts equal text (after redaction) and equal store rows. A second check fails when any `catalog()` tool name has no case, so a new tool cannot land without one. Runs under `cargo test` within the 30 s cap (cases in parallel, shared binary).
- **No-Rust path:** None: it is a test.
- **Evidence:** `ls tests/` → layering_lint.rs, scope_lint.rs, code_map.rs, run_agent_ipc.rs, mcp_bridge_external.rs; none cover catalog-tool parity.

### `x-local-model-fit` — Tool results fit a local model's `limits.context_window`: compact `render_text` for typed tools, bounded `data`, per-engine result caps; documented settings for Ollama `gemma4:latest`

E0 · S · rust · missing · application (chat/tool_loop) + domain (observation render) + config · PRD addendum

- **Have:** `engine = "local"` exists (`src/adapters/outbound/engines/local.rs`); `limits.context_window` defaults to 1,000,000 (`src/config/mod.rs`, wrong for local models per the CLAUDE.md gotcha); `max_tool_result_chars` defaults to 300,000; typed observations render `data` up to 16,000 chars (`render_text`).
- **Need:** When an agent's engine is `local`, cap each tool result at a share of `limits.context_window` (tokens → chars) and render typed observations compactly (headline + features; `data` replaced by a pointer to the store key). Config validation warns when a local agent keeps the 1,000,000 default. Document working settings for Ollama `gemma4:latest` (context window, `max_tool_rounds`, result cap) in `docs/engine-backends.md` § Local. Tests: a 16k-context agent never receives a tool result above its budget; line 1 of typed rows stays intact.
- **No-Rust path:** Partly: the numbers are TOML (`limits.*`); the per-engine cap and compact rendering are code.
- **Evidence:** Read `src/config/mod.rs` (default_context_window) and `docs/typed-observations-2026-09-24.md` (render_text rules), 2026-09-30.

### `x-engine-matrix-smoke` — Live smoke harness (`#[ignore]` tests + `tengu doctor --engines`): a scripted turn that calls each tool of a set and reads its result, on OpenRouter (`google/gemini-2.5-flash-lite`, `anthropic/claude-haiku-4.5`), Ollama `gemma4:latest` and the Claude CLI (subscription); one fixture sandbox per engine

E0 · M · rust · missing · tests + inbound (cli/doctor) + sandbox fixtures · PRD addendum
After: `x-bridge-parity` (E0), `x-claude-code-hardening` (E0), `x-local-model-fit` (E0)

- **Have:** Engine live checks exist only in pieces: `#[ignore]` live tests for Solana and Jev; the local engine was smoke-tested once with `run-agent` on Ollama `gemma4:latest` (SESSION_HANDOFF 2026-09-23). Nothing runs one tool set across all three engines.
- **Need:** `#[ignore]` integration tests plus `tengu doctor --engines --sandbox <s>`: for each engine — OpenRouter (`google/gemini-2.5-flash-lite`, `anthropic/claude-haiku-4.5`), local (Ollama `gemma4:latest` at `http://127.0.0.1:11434/v1`), `claude_code` (Claude CLI on the subscription, built-ins off, `--strict-mcp-config`) — run a scripted one-turn prompt through `tengu run-agent` in a fixture sandbox whose agent holds the tool set under test; assert every tool was called, returned without error and its result was read (activity / audit log). Replay-backed network where possible; one turn per tool set to bound cost.
- **No-Rust path:** Partly: the fixture sandboxes are TOML; the harness and doctor check are code.
- **Evidence:** SESSION_HANDOFF (2026-09-23 local engine row); `ollama list` shows `gemma4:latest`; `claude --version` 2.1.285 (2026-09-30).

### `x-engine-parity-audit` — Run every existing catalog tool (workspace, http, memory, cache, skills, crypto, Solana, agentic memory, …) through the lint, conformance and smoke above and fix every failure; the gap list and fixes are recorded here

E0 · M · rust · missing · outbound/tools + inbound (mcp_bridge) + docs · PRD addendum
After: `x-bridge-conformance-test` (E0), `x-bridge-parity` (E0), `x-engine-matrix-smoke` (E0), `x-local-model-fit` (E0), `x-tool-schema-lint` (E0)

- **Have:** The catalog has 28 `ToolEntry` rows (workspace, http, memory, cache, skills, crypto, Solana reads and writes, agentic_memory, manage_skill, view_skill, skill_resource, skill_lifecycle, …); none has been checked across all three engines. Known violation: shell-skill tools (`SkillPlugin`) are never registered in the bridge (`src/adapters/inbound/mcp_bridge.rs:499`), so they do not work under `claude_code`.
- **Need:** Run the schema lint, the conformance harness and the live matrix over every existing catalog tool, `[[mcp_servers]]` proxy tools and shell-skill tools; fix every failure — tool schema, result size, config or store access through the bridge, or the engine side (e.g. build a `SkillRegistry` in the bridge so shell skills are bridged). No tool may stay excluded (operator rule). Record the gap list and each fix in the tracker.
- **No-Rust path:** None for the fixes; the audit itself is the three harnesses above.
- **Evidence:** `rg -c 'ToolEntry {' src/adapters/outbound/tools/mod.rs` → 28; `src/adapters/inbound/mcp_bridge.rs:499` (SkillPlugin never registered), 2026-09-30.


## M0 — thin paper slice + weekend sandbox

### `ops-sandbox-config` — `sandboxes/xmarket/config.toml` — one owner, sections staged per milestone (agents, egress, scopes, feeds, secrets list)

M0 · S · toml · missing · sandbox | config · PRD §1 §26 §27 §35 S1
After: `hl-book-tool` (M0), `hl-ctx-tool` (M0), `info-store` (M4), `jev-xmarket-loops-toml` (M0), `rh-dex-quote-v3` (M2), `rh-quote` (M2), `risk-gate-domain` (M0), `risk-gate-enforcement` (M0)

- **Tracker note:** Slow-path agent = one `xm_architect` on OpenRouter or hardened `claude_code` (convention 11); loop set per convention 8 (M0 one loop with `escalate = false`, M2 `market_anomaly` stub); no shell and no `[[mcp_servers]]` in a `[risk]` sandbox, `claude_code` only with builtins off + `--strict-mcp-config` (convention 12); `allow_hosts` ceiling (convention 17). The M0 stage names the EDGAR poll row with `target` = the M0 loop.
- **Have:** Nothing for xmarket. `ls sandboxes` shows jev-exec, lping, storage-test, tor-check, unlimited; `rg -n -i 'xmarket|\bxm\b' src sandboxes` finds nothing. Patterns to copy: sandboxes/lping/config.toml (network open 25-26, webhook→loop 47-49, per-tool default_scopes 61-143, non-routable loop executor 231-240) and sandboxes/jev-exec/config.toml (Jev over http_request 105-140).
- **Need:** Draft (<...> = tools from other slices):
  runtime_profile = 'auto'
  [egress]
  network = 'open'   # venues, RPCs, paid APIs, latency; Tor split = ops-egress-split-routing (P2)
  allow_hosts = ['api.hyperliquid.xyz', 'rpc.mainnet.chain.robinhood.com', 'robinhoodchain.blockscout.com', 'api.x.com', 'api.telegram.org', '<info hosts>']
  [memory]
  enabled = true
  within_session_output_top_k = 3
  [scaffold]
  root = '~/xmarket-workspace'
  directories = ['research', 'events', 'reports']
  [orchestrator]
  agent = 'xm'
  engine = 'rag'
  max_attempts_per_step = 2
  max_replans = 1
  [agents.xm]   # planner + operator chat; never claude_code
  default = true
  engine = 'openrouter'
  model = 'anthropic/claude-sonnet-4.6'
  workspace = '~/xmarket-workspace'
  tools = ['read_file', 'list_directory']
  [agents.researcher]   # architect, slow path §26-27; routable
  engine = 'openrouter'
  model = 'anthropic/claude-opus-5.5'
  workspace = '~/xmarket-workspace'
  description = 'Researches unknown entities, tokens, relationships and conflicting reports; returns structured context (entities, assets, venues, relationship strength direct|strong|possible|speculative, sources). Never trades.'
  tools = ['http_request', 'read_file', 'write_file', 'list_directory', 'agentic_memory']
  limits = { max_tool_rounds = 30, step_timeout_secs = 600 }
  [agents.xm_executor]   # loop tool owner, NO description (not routable)
  engine = 'openrouter'
  model = 'anthropic/claude-haiku-4.5'   # schema-required, unused by loops
  workspace = '~/xmarket-workspace'
  tools = ['http_request', '<hl_*, rh_*, ref_*, universe_*, paper_*, risk_* typed tools>']
  [agents.xm_executor.scopes.http_request]   # alert action only; token never granted to LLM agents
  net_hosts = ['api.telegram.org']
  env_reads = ['TELEGRAM_BOT_TOKEN']
  [default_scopes.http_request]
  net_hosts = ['<info hosts>']
  fs_roots = ['~/xmarket-workspace']
  [default_scopes.agentic_memory]
  env_reads = ['TENGU_MEMORY_DATABASE_URL']
  [webhooks]
  enabled = true
  bind = '127.0.0.1'
  port = 7082
  [webhooks.endpoints.market]
  loop = 'xm_market'
  secret_env = 'TENGU_XM_WEBHOOK_SECRET'
  [decision_loops.xm_market]   # actions from the jev slice; paper only
  goal = '<jev slice>'
  agent = 'xm_executor'
  model = '~typesafe/jev-latest'   # pin a version for evaluation windows
  dry_run = true
  act_at = 0.8
  max_steps = 6
  [decision_loops.xm_market.actions.ignore]
  description = 'Nothing to do for this event'
  # Added by this slice later: [audit] store = 'sqlite' · [recorder] enabled = true, retention_days = 30 · [budget] daily_usd = 40 · [alerts] chat_id_env = 'XM_ALERT_CHAT_ID' · [liveness] max_feed_age_secs = 90
  
  Secrets/env (vault or .env):
  - OPENROUTER_API_KEY: the dedicated daily-limited key (engines, embeddings, Jev).
  - TELEGRAM_BOT_TOKEN, TENGU_TELEGRAM_ALLOWED_USERS, XM_ALERT_CHAT_ID (new).
  - TENGU_MEMORY_DATABASE_URL, TENGU_XM_WEBHOOK_SECRET, TENGU_MASTER_PASSWORD.
  - The X API bearer token and news/reference vendor keys (info/rt slices).
  - Optionally a private RH Chain RPC URL (its host goes in net_hosts; never rendered).
  No signer keys: paper only.
- **No-Rust path:** This is itself the no-Rust path (TOML, doctrine #2).
- **Evidence:** ls sandboxes; rg -n -i 'xmarket|\bxm\b' src sandboxes → none; OpenRouter slug probe (anthropic/claude-sonnet-4-6 and -4.6 both resolve)
- **Absorbed `rt-xmarket-runtime-toml`** — xmarket sandbox runtime wiring (TOML): feed agent + scopes, budgets, feeds, detectors, market_first / info_first loop inputs. Runtime sections in sandboxes/xmarket/config.toml: [egress] network (see open question); [runtime]; [agents.xmarket_feeds] with no description and scopes http_poll / ws_feed / http_stream (net_hosts api.hyperliquid.xyz, api.x.com, news hosts; env_reads for auth vars); [budgets.hyperliquid] per_minute = 1000; [feeds.hl_universe] (kind tool, every_secs 900); [feeds.hl_ctx_<dex>] (every_secs 15, weight 20, budget hyperliquid); [detectors.*] (OI delta, funding jump, volume ratio, mark-vs-oracle premium); [decision_loops.market_first] input = ['market.anomaly'], lane = '/subject'; [decision_loops.info_first] input = ['info.event']. Tool names and schemas come from the hl / info slices.
- **Absorbed `hl-sandbox-config`** — sandboxes/xmarket/config.toml for this slice: HL/CEX scopes, observer agent, market_watch loop (TOML only). (1) Scopes: [default_scopes.hl_universe], [default_scopes.hl_ctx], [default_scopes.hl_book], [default_scopes.hl_funding] and [default_scopes.hl_candles], each with net_hosts = [api.hyperliquid.xyz] and fs_roots = [~/xmarket-workspace]; CEX scopes with their own hosts. (2) Agent market_observer with no description, holding the hl_* tools. (3) [decision_loops.market_watch] with a static starter set: world eth = mkt_ctx/1:hyperliquid:ETH, tsla = mkt_ctx/1:hyperliquid:xyz:TSLA, tsla_book = hl_book/1:xyz:TSLA. Actions: refresh_ctx (read_only, hl_ctx), inspect_book (read_only, hl_book), paper_enter (dry_run, requires tsla <= 10 s and tsla_book <= 5 s). (4) [egress] network from hl-tor-probe. The dynamic asset set needs jev-dynamic-world.
- **Absorbed `rh-sandbox-toml`** — sandboxes/xmarket/config.toml, Robinhood / reference part: agent, scopes, contract book, calendar, rh_watch loop. Egress: [egress] network = 'open'. Tor is unverified for these hosts; lping sets the precedent.
  
  Agent: loop owner xm_rh with no description; tools = ['rh_assets','rh_quote','rh_dex_quote','market_session','rh_basis', ...].
  
  Scopes: one [default_scopes.<tool>] per tool.
  - net_hosts drawn from: api.robinhood.com, rpc.mainnet.chain.robinhood.com, reference-data-directory.vercel.app, api.rh.lighter.xyz, li.quest, data.alpaca.markets, www.nasdaqtrader.com, www.sec.gov, api.openfigi.com, api.frankfurter.dev.
  - env_reads = ['ROBINHOOD_RPC_URL'] plus provider keys.
  - fs_roots = ['~/xmarket-workspace'].
  
  Also [evm.chains.robinhood] (contract book) and [market_calendar.us_equities].
  
  Loop [decision_loops.rh_watch]:
  - dry_run = true.
  - world = { q = 'rh_quote/1:0x322F0929c4625eD5bAd873c95208D54E1c003b2d', dex = 'rh_dex_quote/1:0x322F0929c4625eD5bAd873c95208D54E1c003b2d:0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168', basis = 'rh_basis/1:TSLA' }.
  - Actions: refresh_quote, dex_quote, compute_basis, paper_enter (requires basis <= 10 s), hold, escalate.
  
  Update config.example.toml.
- **Absorbed `kg-sandbox-config`** — sandboxes/xmarket: catalog agents, per-tool scopes, knobs, config validation. sandboxes/xmarket/config.toml:
  - `[xmarket] catalog_db`, `user_agent` (SEC/Wikidata contact), `[xmarket.lifecycle]` knobs, `[xmarket.venues.hyperliquid] trade_dexes = [...]` allow-list, `[xmarket.calendars.*]`
  - `[agents.xm_catalog]` (no description): xm_universe_sync, xm_lifecycle_eval, xm_find_instruments, xm_related_assets, xm_confirm_reaction, xm_resolve_entity
  - architect/researcher: read + propose tools only
  - `[default_scopes.xm_universe_sync]` net_hosts = api.hyperliquid.xyz, api.robinhood.com, www.sec.gov, data.sec.gov, www.nasdaqtrader.com, api.openfigi.com, api.coingecko.com; env_reads = OPENFIGI_API_KEY, COINGECKO_DEMO_API_KEY; fs_roots = workspace
  - `[default_scopes.xm_resolve_entity]` adds api.gleif.org, www.wikidata.org, query.wikidata.org
  - `[egress] network = "open"` until Tor is verified per host
  Validation in src/config/xmarket.rs: write-capable xm tools only on non-routable agents; DB outside fs roots.
- **Absorbed `risk-xmarket-sandbox-exec`** — xmarket sandbox paper path: [risk], [paper], private executor agent, xm_paper loop, risk skill. sandboxes/xmarket/config.toml:
  - [risk] and [paper] blocks with operator limits.
  - [agents.xm_executor]: no description, openrouter, workspace ~/xmarket-workspace, tools = xm_* compute + paper_* + risk_status + hl/rh read tools.
  - Scopes: compute and paper tools fs_roots = ["~/xmarket-workspace"] with no net; sign_and_send_transaction = {} and sign_message = {} on every agent; no shell_bins anywhere.
  - [decision_loops.xm_paper] with world = { risk = "risk_state/1:<account>", cand = "xm_tradable/1:<account>", cmp = "xm_compare/1:<a>:<b>" }.
  - Actions: hold; inspect (xm_compare, read_only); paper_enter (paper_order; slots instrument = FromObservation over cand, side = ["buy","sell"], notional_usd = [100, 250, 500]; caps notional_usd = 500; requires = { risk = 10, cmp = 5 }); reduce; close (paper_close).
  - dry_run = false, since paper IS the simulation. Never read_only on exec actions.
  skills/xm-risk/SKILL.md for the architect and escalation agent: the rules, the meaning of each risk_rule code, and 'you never trade: return structured context'.

### `x-shared-workspace-and-state-layout` — Enforce one xmarket workspace + the `<TENGU_HOME>/state/xmarket/` layout; paths outside every fs root

M0 · S · rust · missing · config | docs · PRD §30 §32 §36
After: `ops-sandbox-config` (M0)

- **Have:** A loop reads `world` from its own agent's <workspace>/.tengu/observations.db. An agent without `workspace` falls back to the process cwd (src/bootstrap/decision.rs:57-61, 76-83). Typed tools write rows into the store of whichever agent runs them (src/adapters/outbound/observations.rs:59-73). Nothing checks that feed, loop and architect agents share a workspace. Across the reports, new state lands in about ten files under two roots: <TENGU_HOME>/state/{xmarket.db, xm-paper.db, audit/<sandbox>.db, history/<sandbox>/, spend.db, runtime.db}, <workspace>/.tengu/{feeds.db, loops.db, series.db}, plus Postgres xm_* tables.
- **Need:** (1) Config validation in src/config/xmarket.rs, run when [feeds], [risk] or [xmarket] is present: every agent named by [feeds.*].agent, [decision_loops.*].agent, the escalation agent and the owners of xm_* and paper_* tools must set the same explicit `workspace`; otherwise a load error that names the agents. (2) One install-wide layout, documented and backed by path constants: <TENGU_HOME>/state/xmarket/{catalog.db, events.db, ledger.db, audit.db, spend.db, runtime.db} plus history/<YYYYMMDD>.db. The workspace keeps only observations.db (the hot cache). Every path is validated to lie outside all fs roots and workspaces (reuse the src/config/solana.rs path helpers). `tengu prune` keeps sparing state/ (src/adapters/outbound/prune.rs:47-56).
- **No-Rust path:** Partial — setting the same `workspace` on every agent in TOML works today; only the check that catches a missing or mismatched one needs Rust (S)
- **Evidence:** read src/bootstrap/decision.rs:57-83, src/adapters/outbound/observations.rs:59-73, src/adapters/outbound/prune.rs:47-56; store paths as proposed in the kg, risk, ops, rt, jev and info reports

### `ops-openrouter-budget-key` — Dedicated OpenRouter key with a daily credit limit — hard cap for LLM and Jev calls, which share it (split keys if escalations ever starve Jev)

M0 · S · account · missing · infra · PRD §27 §35 S3

- **Tracker note:** Jev and LLM calls share this key's daily limit; once escalations are on (M5), a storm can stop Jev — split keys if that happens.
- **Have:** One OPENROUTER_API_KEY is shared by the chat engines, embeddings and Jev (src/adapters/outbound/decisions.rs:27-31). tengu has no spend cap of its own.
- **Need:** Create a key in the dashboard or via the Management API with limit = <USD/day> and limit_reset = 'daily' (resets at 00:00 UTC). Install it only for the xmarket deployment (`tengu secret set OPENROUTER_API_KEY` or .env). Suggested start: $40/day, with a warning at 80% via GET https://openrouter.ai/api/v1/key usage_daily / limit_remaining. X API: prepaid credits with auto-recharge off, plus a per-cycle spending limit in the Developer Console.
- **No-Rust path:** This is itself the no-Rust path. Trade-offs: covers OpenRouter traffic only (not the Claude Code subscription, X or news vendors); one cap shared by all loops; when the cap is hit calls return errors, which are audited only after ops-audit-atomic-write.
- **Evidence:** WebFetch openrouter.ai/docs/guides/overview/auth/management-api-keys (limit, limit_reset daily, usage_daily, limit_remaining); WebFetch docs.x.com/x-api/getting-started/pricing (prepaid credits, auto-recharge, spending limit)

### `rt-daemon` — `tengu run --sandbox <s>`: one process for feeds + loops + webhook router (feature-gated); graceful shutdown; single-runner lease

M0 · M · rust · partial · inbound | bootstrap | config · PRD §3 §13 §20 §35 S2 §35 S3

- **Tracker note:** The webhook router lives in the `webhooks`-gated module (`src/adapters/inbound/mod.rs`); mount it behind that feature or make `axum` / `hmac` non-optional, so `x-m0-e2e-test` runs under default features.
- **Have:** `tengu webhooks` (src/adapters/inbound/webhooks.rs:82-179) is the only long-running trigger host. It builds one DecisionLoop per referenced loop at boot (:115-139) and serves via `axum::serve` without graceful shutdown (:175-177). `tengu decide` is one-shot (src/adapters/inbound/cli/decide.rs:17-49). CLI `Commands` (src/adapters/inbound/cli/mod.rs:37-162) has no run/watch/daemon. The escalator and the one-shot turn factory live in the inbound adapter (webhooks.rs:332-388, 642-813), which bootstrap may not import.
- **Need:** inbound: src/adapters/inbound/run.rs `run_runtime(config, secrets)` + `Commands::Run { sandbox }` in cli/mod.rs, with file+stderr logging like webhooks (cli/mod.rs:363-420). bootstrap: src/bootstrap/runtime.rs builds loops once (bootstrap::decision::build_decision_loop), feeds, stores and the bus. Move OrchestratorEscalator / run_one_shot / WebhookChatServiceFactory to src/bootstrap/escalation.rs. Add `webhooks::router(state)` so `tengu run` mounts /webhooks/:name when [webhooks] enabled, giving one process ownership of loop state. Shutdown: tokio::signal::ctrl_c + unix SIGTERM -> tokio::sync::watch cancel -> stop feeds, `axum::serve(..).with_graceful_shutdown`, drain in-flight events for up to [runtime] shutdown_grace_secs (20), flush writers. Single-runner lease `runtime:<sandbox>` in <TENGU_HOME>/state/runtime.db (ACQUIRE_SQL pattern, writes_store.rs:27-36), renewed every 10 s; a second instance exits non-zero. config: src/config/runtime.rs `[runtime]` (deny_unknown_fields) with shutdown_grace_secs, max_decisions_in_flight, heartbeat_secs.
- **No-Rust path:** Partial: run `tengu webhooks` under launchd/systemd (Restart=always) plus external tick senders (rt-interim-cron-ticks). Trade-off: no feeds/bus/backpressure/dedup, one spawned task per POST (webhooks.rs:260), history lost on every restart.
- **Evidence:** rg 'Daemon|daemon|Commands::(Watch|Run|Stream|Feed|Serve|Daemon)|fn run_(watch|stream|feed|daemon|serve)' src -> only Commands::RunAgent; rg 'ctrl_c|with_graceful_shutdown|CancellationToken|SIGTERM|signal::unix' src -> only a telegram oneshot (telegram.rs:88); read webhooks.rs, cli/mod.rs, cli/decide.rs, bootstrap/decision.rs.

### `rt-backoff-budget` — Shared backoff per `ErrorClass` (jitter, Retry-After) + the one request limiter, `[rate_limits.<name>]` (token buckets, weights)

M0 · S · rust · partial · domain | outbound | config · PRD §20 §28 §35 S2

- **Tracker note:** Config table is `[rate_limits.<name>]` (not `[budgets.<name>]`, one letter from the spend cap) — convention 6. Shared by feeds, `hl-info-client` and `info-fetch`.
- **Have:** ErrorClass incl. RateLimited / QuotaExhausted + ReadError.retry_after_ms (src/domain/observation.rs:71-118). Solana RPC maps HTTP/JSON-RPC errors to classes and retries once within 1 s, honouring Retry-After (src/adapters/outbound/solana/rpc.rs:10-16, 57-58, 130-175). The orchestrator has a step backoff list (src/application/orchestrator/retry.rs:16-58). JevClient has no retry: one failed call aborts the whole event (src/adapters/outbound/decisions.rs:58-81; decision_loop/mod.rs:202 `?`). No token bucket or rate limiter anywhere.
- **Need:** domain: src/domain/backoff.rs `next_delay(class, attempt, retry_after_ms, policy, rand01) -> Retry(ms) | Park(ms) | Stop`: RateLimited -> max(Retry-After, exp); QuotaExhausted -> park quota_reset_secs; AuthRequired/Fatal -> stop + health down; Transient/Timeout -> full-jitter exp 1 s..60 s. Plus `TokenBucket { per_minute, burst }`. outbound: move HTTP-status/body -> ErrorClass out of solana/rpc.rs into src/adapters/outbound/http_class.rs so feeds share it. config: `[budgets.<name>] per_minute = 1000` + `[feeds.<n>] budget = 'hyperliquid', weight = 20` (HL: 1200 weight/min/IP, metaAndAssetCtxs = 20). Jev: one retry on 429/5xx + a circuit breaker in dispatch.
- **No-Rust path:** None for enforcement; operators can only choose conservative every_secs by hand.
- **Evidence:** rg -i 'backoff|jitter|retry.after|retry_after|RateLimited|QuotaExhausted|rate_limited|quota_exhausted' src -> domain/observation.rs, solana/rpc.rs, orchestrator/retry.rs only; rg -i 'token.?bucket|governor|ratelimiter|rate_limiter|per_minute|requests_per|leaky' src -> only an unrelated test struct (secrets.rs:422).

### `rt-scheduler` — `[feeds.<n>]` scheduler: `kind = "tick"`, `"tool"`, `"poll"`; M0 feeds name `target = "<loop>"` (direct `handle_event`, one in flight per loop) until `rt-bus-dispatch` adds topics

M0 · M · rust · missing · config | ports | application · PRD §19 §20 §35 S1 §35 S2
After: `rt-backoff-budget` (M0), `rt-bus-dispatch` (M2), `rt-daemon` (M0)

- **Tracker note:** M0: tool and poll feeds name `target = "<loop>"` (direct `DecisionLoop::handle_event`, one in flight per loop); `rt-bus-dispatch` (M2) replaces that with `topic` + `input`. Also replaces `rt-interim-cron-ticks` (host-timer stopgap), dropped because this lands in M0; its notes are kept below in case M0 slips.
- **Have:** No interval or cron code. The only loops are UI loops: TUI audit tail every 300 ms (src/adapters/inbound/tui/mod.rs:47-61) and telegram typing sleeps (telegram.rs:1345). The plan's `trigger (tick | webhook | gRPC)` (docs/decision-loop-plan-2026-09-24.md:33) and open question 1 (default 'polling', :120) were never built. Typed tools already cache through observe() (src/application/observe.rs:16-52), and a loop agent executor can be built (src/bootstrap/decision.rs:62-74).
- **Need:** config: src/config/feeds.rs `[feeds.<n>]` (deny_unknown_fields). kind = 'tick' {every_secs, jitter_pct, target = '<loop>', event = {..}}; kind = 'tool' {agent, tool, args, every_secs, jitter_pct, budget, weight, emit = 'none'|'event', topic}. Validation: tool is in the agent's tools, target loop exists, every_secs >= 1. application: src/application/runtime/poller.rs (sleep_until next tick +/- jitter, skip missed ticks, one in-flight run per feed; reads Observation.status / errors[].class / retry_after_ms for rt-backoff-budget). ports: src/ports/clock.rs `Clock { now_ms, sleep_until }` for deterministic tests. An S1 universe refresh (every_secs = 900) and S2 market polling (every_secs = 15) are then just feed rows.
- **No-Rust path:** cron/launchd/systemd timer POSTing an HMAC-signed `{}` to a `loop` endpoint (rt-interim-cron-ticks). Cron is 60 s granular, each tick costs at least 1 Jev call just to choose `fetch`, there is no budget/backoff, and the queue is unbounded.
- **Evidence:** rg 'tokio::time::interval|time::interval\(|interval_at|MissedTickBehavior|\bcron\b|Cron|scheduler|Scheduler|schedule_|poll_interval|interval_secs|every_secs|tick_secs|\bticker\b|Ticker|poller|Poller' src -> only an observations.rs:29 comment and a scanner.rs:135 regex; rg 'render_audit|decisions.jsonl|from_millis\(300\)|sleep\(' src/adapters/inbound -> UI loops only.
- **Absorbed `jev-loop-ticker`** — Periodic ticks for loops (position management, monitor re-checks). config `[decision_loops.<n>] every_secs = 30`, optional `tick_event = {...}`. inbound/webhooks.rs (loop host): build every [decision_loops.*] block, and spawn one tokio interval per ticking loop that calls handle_event({tick: n, ts_ms}, "tick-<loop>-<n>"). Skip a tick while the previous one still holds the loop mutex. position_manager then reads `world = { book = "xm_paper_book/1:paper-main" }` with a FromObservation slot over /data/positions/*.
- **Absorbed `rt-interim-cron-ticks`** — Interim no-Rust ticks: launchd/systemd timer -> HMAC-signed POST to `tengu webhooks` loop endpoints. deploy/xmarket/ (infra only): a launchd plist and a systemd .service + .timer (OnUnitActiveSec=15s, AccuracySec=1s). Their inline command signs with `printf '%s' $BODY | openssl dgst -sha256 -hmac $SECRET` and runs `curl -X POST http://127.0.0.1:<port>/webhooks/tick_market` with header X-Tengu-Signature: sha256=<hex>. sandboxes/xmarket/config.toml gets `[webhooks] enabled = true` + `[webhooks.endpoints.tick_market] loop = 'market_first', secret_env = 'XMARKET_TICK_SECRET'`. Add a docs section with the limits. Retire once rt-scheduler lands.

### `hl-info-client` — Hyperliquid `POST /info` client on the shared limiter; HL error mapping (`500 null` ⇒ not applicable, 403 ⇒ geo / WAF)

M0 · M · rust · missing · outbound · PRD §19 §20 §35 S1 §35 S2

- **Tracker note:** Uses the shared `rt-backoff-budget` limiter (`[rate_limits.hyperliquid]`, per-request weights); no own bucket (convention 14).
- **Have:** Nothing HL-specific. Reusable: scoped/egress-checked post_json/request_json + audit + ErrorClass in src/adapters/outbound/solana/http_json.rs:36-135 and the classifier in src/adapters/outbound/solana/rpc.rs:124,300 (maps every 5xx to Transient).
- **Need:** outbound: src/adapters/outbound/hyperliquid/{mod.rs,info.rs}. HlInfo::post(body) returns InfoReply {Json | Null}; it uses ctx.http and scope.check_net_host(api.hyperliquid.xyz). Base URL https://api.hyperliquid.xyz; testnet https://api.hyperliquid-testnet.xyz via scoped env HL_API_URL. In-process weight bucket, 1200/min: weight 2 for l2Book, allMids, exchangeStatus, clearinghouseState, orderStatus, spotClearinghouseState; 60 for userRole; 20 for everything else; +1 per 60 candles; +1 per 20 items for recentTrades, fundingHistory and userFills. Error mapping: 200 null => Absent; 500 null (unknown dex/coin) => NotApplicable, not Transient; 422 'Failed to deserialize the JSON body into the target type' => Fatal; 429 => RateLimited; 403 => AuthRequired with message 'blocked (geo/WAF/Tor exit?)'; other 5xx => Transient; timeout => Timeout. Decimal-string helpers: f64 for features, raw strings kept in data. Move request_json/post_json/fetch_json to src/adapters/outbound/http_json.rs (solana re-exports them) so the hyperliquid and cex families share one gate. Fixtures in tests/fixtures/hyperliquid/*.json, captured from this session's probes (JSON only).
- **No-Rust path:** Partial. http_request already POSTs JSON through egress + scope + audit (Content-Type defaults to application/json, request.rs:280-283), so an LLM agent can use HL through a skill (hl-skill). Lost: weight budgeting, typed decimals, and 500 null => NotApplicable (without it an unknown coin looks like an outage).
- **Evidence:** rg -n -i 'hyperliquid|hip-3|hip3|api\.hyperliquid|hyperevm' over the repo (excl. Cargo.lock): only docs/xmarket-prd-2026-09-29.md. rg -n -i '\bhl_[a-z]+|hl::|"hl"' src skills sandboxes docs: none. Read src/adapters/outbound/solana/http_json.rs and rpc.rs:296-320. Probes: meta dex=nope => 500 null; l2Book xyz:NOPE => 200 null; type=bogus => 422.

### `hl-market-schema` — Cross-venue schemas `mkt_instrument/1` + `mkt_ctx/1`, keyed by instrument id

M0 · M · rust · missing · domain · PRD §19 §20 §23 §25 §35 S1 §35 S2

- **Have:** Nothing. The 21 'const SCHEMA' impls are all Solana or test schemas (acct/1, price_oracle/1, dlmm_*/1, jup_perps/1, lp_*/1, solana_*/1, write/1). Envelope rules are in src/domain/observation.rs:17-24,153-194.
- **Need:** domain: src/domain/market.rs. (1) MarketInstrument: Observed, schema mkt_instrument/1, subject <venue>:<symbol>. Fields: venue, symbol, kind (perp | spot | outcome), dex, asset_id, category (stocks | etf | indices | commodities | fx | rates | preipo | crypto; normalises HL's stock/stocks and FX/fx), display_name, keywords, underlying {listing, ticker, ratio, fx_converted}, quote_ccy (USDC | USDT | USDH | USDE | USD), sz_decimals, max_leverage, margin_mode (normal | no_cross | strict_isolated), only_isolated, status (listed | delisted), oi_cap_usd, deployer_fee_scale, growth_mode. (2) MarketCtx: Observed, schema mkt_ctx/1, one Field<f64> per price. At most 32 features: mark, oracle, index, mid, bid, ask, last, spread_bps, impact_bid, impact_ask, impact_spread_bps, basis_bps, premium_bps, funding_1h, funding_apr_pct, funding_interval_h, next_funding_s, oi_base, oi_usd, oi_cap_used_pct, at_oi_cap, vol_24h_usd, change_24h_pct, max_leverage, only_isolated, delisted, session, category, growth_mode, taker_fee_bps, oracle_eq_mark. Status: delisted or 200-null => absent; ctx without a book (null premium/midPx/impactPxs) => partial; read failure => error. Venue ids: hyperliquid, binance-spot, binance-usdm, bybit-spot, bybit-linear, okx-spot, okx-swap, coinbase, coinbase-intx; the rh slice adds its own. Example keys: mkt_ctx/1:hyperliquid:xyz:TSLA, mkt_ctx/1:hyperliquid:@151, mkt_ctx/1:binance-usdm:TSLAUSDT. Tests use assert_features_ok.
- **No-Rust path:** None. Typed status/Field semantics and the features contract are Rust types; TOML reducers only project untyped JSON.
- **Evidence:** rg -n 'mkt_|market_ctx|MarketCtx|struct Instrument|instrument/1|venue' src: only venue_permission in src/domain/lp/snapshot.rs. rg -n 'const SCHEMA: &' src: 21 hits, none venue-generic.

### `hl-ctx-tool` — `hl_ctx`: mark / oracle / mid / impact / basis / funding / OI / volume; each sweep also reads `perpDexs` + `perpsAtOpenInterestCap` into `mkt_instrument/1` (fee scale, growth mode, OI cap, status) — M0 subset of `kg-sync-hyperliquid`

M0 · M · rust · missing · domain + outbound (tools) · PRD §12 §14 §20 §23 §25 §35 S2
After: `hl-info-client` (M0), `hl-market-schema` (M0)

- **Tracker note:** Also writes `mkt_instrument/1` rows each sweep from `perpDexs` + `perpsAtOpenInterestCap` (fee scale, growth mode, OI cap, status) — the M0 subset of `kg-sync-hyperliquid`, so fee and OI-cap checks have inputs before M1.
- **Have:** Nothing. observe() (src/application/observe.rs:16-52) and the store are ready.
- **Need:** domain: src/domain/hl/ctx.rs. Perp AssetCtx decoder: funding, openInterest, prevDayPx, dayNtlVlm, premium, oraclePx, markPx, midPx, impactPxs, dayBaseVlm, all decimal strings. Spot ctx decoder: markPx, midPx, prevDayPx, dayNtlVlm, circulatingSupply. Maps into MarketCtx with basis_bps = (mark - oracle) / oracle, impact_spread_bps, premium_bps, funding_1h (HL is hourly), funding_apr_pct (x 8760), oi_usd = oi x mark, oi_cap_used_pct, vol_24h_usd, change_24h_pct = mark / prevDayPx - 1, and oracle_eq_mark. Tool hl_ctx (tools/hyperliquid/ctx.rs). Args: coins (at most 64 full names, e.g. ETH, xyz:TSLA, @151) or dex, plus max_age_secs. Makes one metaAndAssetCtxs call per needed dex (spotMetaAndAssetCtxs for @ coins), writes a mkt_ctx/1:hyperliquid:<coin> row for every returned coin (TTL 5 s), and returns the requested ones. Listing flags, OI caps and category come from fresh mkt_instrument/1 rows. Budget: the 5 active dexes (default, xyz, para, mkts, io) cost 100 weight per full sweep.
- **No-Rust path:** A jev-exec stopgap works: tool http_request, POST https://api.hyperliquid.xyz/info with body {type: metaAndAssetCtxs, dex: {dex}}, slots dex = ['', xyz, para, mkts, io], reduce ctx = /1/*/{markPx,oraclePx,funding,openInterest}. Lost: coin names (they are index-aligned in /0/universe), every item after the 20th, numbers (HL sends strings), store rows for world/requires, and the basis/APR/oi_usd arithmetic that §25 assigns to code.
- **Evidence:** rg -n -i 'open_?interest|funding_?rate|fundingRate' src: only src/domain/lp/perps.rs:706 (Jupiter). Probes: metaAndAssetCtxs for all 11 dexes. Read src/application/decision_loop/reduce.rs:17-35,96-120.

### `hl-book-tool` — `hl_book`: executable bid / ask, depth, imbalance, VWAP slippage for a notional

M0 · M · rust · missing · domain + outbound (tools) · PRD §20 §21 §25 §31 §35 S2 §35 S7
After: `hl-info-client` (M0), `hl-market-schema` (M0)

- **Tracker note:** Key uses the convention-1 id: `hl_book/1:hyperliquid:xyz:TSLA`, not `hl_book/1:xyz:TSLA` (a mismatch is a silent `missing`).
- **Have:** Nothing. An rg for order book finds only the DLMM bid_ask strategy at src/domain/lp/dlmm_ix.rs:53.
- **Need:** domain: src/domain/hl/book.rs. l2Book decoder: at most 20 levels per side, each {px, sz, n}; nSigFigs 2-5 or null; mantissa. Features: bid, ask, mid, spread_bps, depth_usd_10bps and depth_usd_50bps per side, imbalance_10bps, vwap_buy_bps and vwap_sell_bps (slippage vs mid) for notional_usd, and book_empty (delisted/halted). Optional recentTrades (weight 20+) adds last and last_age_s. Tool hl_book. Args: coin*, notional_usd (up to 3 values), include_trades (default false), max_age_secs. Key hl_book/1:<coin>, TTL 2 s; the levels are stored and the notional-specific numbers are computed from them. Weight 2. risk-paper-engine uses it for §31 fills.
- **No-Rust path:** http_request l2Book + reduce /levels/0/0/px gives the top of book only. Depth, VWAP and slippage are arithmetic, which §25 assigns to code.
- **Evidence:** rg -n -i 'l2book|order_?book|orderbook|best_bid' src: none. Probes: l2Book xyz:TSLA (20/20 levels); l2Book flx:TSLA (delisted, levels [[],[]]).

### `info-fetch` — Egress-gated feed fetcher on the shared limiter: headers + User-Agent, conditional GET (`[feeds] kind = "poll"`)

M0 · M · rust · partial · outbound · PRD §15 §35 S3

- **Tracker note:** Uses the shared `rt-backoff-budget` limiter via `[rate_limits.<name>]` (sec.gov 8 req/s); no own bucket, no `[news.hosts]` (convention 14).
- **Have:** src/adapters/outbound/solana/http_json.rs:31-53 request_json handles JSON only (Accept json, no custom headers/UA, no ETag). tools/http/request.rs:485-504 hands the raw body to the LLM. egress tool_client at egress.rs:333-340 has a total timeout and no redirects. Nothing in src sets a User-Agent, and SEC returns 403 without one (probe).
- **Need:** src/adapters/outbound/news/fetch.rs: fetch(http, scope, &FetchSpec{url, method GET|POST, body, headers ($ENV via scope.check_env_read), user_agent, accept, if_none_match, if_modified_since, max_bytes, timeout}) -> FetchOutcome{status, not_modified, etag, last_modified, content_type, body}. Per-host token bucket shared in-process (*.sec.gov 8 req/s, api.gdeltproject.org 1 per 5 s, default 2 req/s) configured in [news.hosts]. Egress check_url + net_hosts gate + audit (host+path), as in http_json.rs:1-13. ErrorClass mapping (429 rate_limited + Retry-After, 401/403 auth_required, 5xx transient); 304 handling; optional reqwest gzip feature. Unit tests against a local mock server.
- **No-Rust path:** http_request with a headers arg already sends a SEC UA (probe: 200). That is enough for skill- or architect-driven one-off reads, but it has no conditional GET, no rate limiter, and the whole body lands in the LLM context, so it cannot do continuous polling.
- **Evidence:** rg -i 'if-none-match|etag|if-modified-since|last_modified' → 4 hits, all similar::ChangeTag; rg -i 'rate_limiter|token_bucket|governor|RateLimiter' → 0; rg -i 'user_agent|user-agent' src → 0. Read http_json.rs, request.rs, egress.rs.
- **Absorbed `rt-http-poll`** — `kind = poll` generic HTTP JSON / RSS / Atom poller with conditional GET, cursors and declarative item mapping. outbound: src/adapters/outbound/feeds/http_poll.rs — GET/POST via egress `tool_client` + `check_url` (egress.rs:297-340). If-None-Match / If-Modified-Since from the cursor; 304 = no items. JSON -> `items` pointer; RSS/Atom via quick-xml (direct-dependency line, no new crate) -> {id, title, link, published, summary}. Per-poll audit (tool = 'feed_poll', host, status, n_items, ms). Scope = the feed agent's scopes.http_poll (check_net_host scope.rs:64; check_env_read :84 for auth headers). Mapping: emit = 'event' (topic info.raw) or obs = {schema, subject = '/id', headline = '/title', features = {..}} rows. config: [feeds.<n>] kind = 'poll', url, method, body, headers_env, format = 'json'|'rss'|'atom', items, id, every_secs, budget.

### `info-parsers` — Atom parser for EDGAR `getcurrent` (RSS 2.0, JSON mapping, `t.me/s`, HTML → text move to `info-parsers-ext`, M2)

M0 · S · rust · missing · outbound · PRD §15 §35 S3

- **Tracker note:** M0 ships only the Atom parser: EDGAR `getcurrent` summaries already carry item codes and accession numbers. RSS 2.0, `json_map`, `tg_preview` and `html_text` moved to `info-parsers-ext` (M2).
- **Have:** Nothing. Cargo.toml:12-67 has no XML/feed/HTML crate; quick-xml 0.31.0 is only a transitive dependency via calamine 0.26.1 in Cargo.lock. Decision-loop reducers parse JSON only (reduce.rs:96-114) and cut strings at 2,000 chars (reduce.rs:19). read_file extracts PDF only (read_file.rs:66-72).
- **Need:** src/adapters/outbound/news/parse/{mod,rss_atom,json_map,tg_preview,html_text}.rs → FeedEntry{source_item_id, url, title, summary, body, published_at_ms, author, categories, vendor_tickers, repost_of}. rss_atom uses quick-xml 0.31 as a direct dependency (same version, already compiled). json_map reuses the reducer path grammar reduce::select (reduce.rs:34), driven by TOML {items,id,title,url,published,published_format,body,tickers}. tg_preview parses data-post, tgme_widget_message_text, <time datetime> and forwarded-from. html_text uses html2text 0.17.1 (new dependency) for EDGAR exhibits and bodies. Goldens go in tests/fixtures/news/ from today's probes (PRN RSS, EDGAR Atom, SEC/CFTC/Fed/ECB feeds, t.me/s HTML, Binance/OKX/Bybit/Upbit JSON).
- **No-Rust path:** Letting the LLM parse raw XML/HTML from http_request is too expensive: the PRN feed (40 KB) is about 10k tokens per poll and a t.me/s page (133 KB) about 35k, i.e. $1-5/day per feed at one poll a minute on flash-lite, with non-deterministic ids. An RSS-to-JSON MCP server via [[mcp_servers]] only converts to JSON (no store, no dedup) and adds an out-of-repo process.
- **Evidence:** rg -i 'rss|atom_syndication|feed_rs|quick_xml|quick-xml' src Cargo.toml → 0; rg -i 'html2text|scraper::|dom_smoothie|strip_tags|html_to_text' → 0. Cargo.lock: quick-xml 0.31.0 via calamine, no HTML crate. crates.io: quick-xml 0.42.0, feed-rs 3.0.0, html2text 0.17.1, dom_smoothie 0.18.2.

### `info-edgar` — EDGAR adapter: `$SEC_USER_AGENT`, ≤ 10 req/s, accession ids, 8-K item codes, CIK → ticker; M0 feed scoped to the allow-listed CIK (Tesla `0001318605`); Ex-99.1 text in M4

M0 · S · rust · missing · outbound · PRD §15 §16 §35 S3
After: `info-fetch` (M0), `info-parsers` (M0)

Status (2026-10-08): 🟡 — the adapter is built (`src/adapters/outbound/backfill/sec.rs`, decoders `src/domain/sec.rs`, `tengu history events`); open: the M0 feed and Ex-99.1 text (tracker row).

- **Tracker note:** The M0 feed is scoped to the allow-listed CIK (Tesla `0001318605`). The Ex-99.1 exhibit fetch moved to `info-pipeline` (M4).
- **Have:** Nothing: rg 'edgar|sec.gov' finds nothing in the repo.
- **Need:** src/adapters/outbound/news/sources/edgar.rs. Parse the getcurrent Atom per form (8-K, 6-K, SC 13D, SC 13G, 425, S-1, F-1); the summary already carries 'Item 1.01 …' codes and AccNo (probe) → EdgarFiling{accession, cik, company, form, items[], filed_at, index_url}. Map items to §16 hints (2.02 earnings, 1.01 agreement, 2.01 acquisition, 5.02 officers, 7.01/8.01 Reg FD/other, 3.01 delisting). Submissions JSON for watched CIKs; Ex-99.1 fetch + html_text; refresh company_tickers.json (10,431 rows) into the kg registry. UA from $SEC_USER_AGENT; host limiter ≤ 10 req/s shared by www/data/efts.sec.gov; daily-index backfill for replay.
- **No-Rust path:** Title-only EDGAR (company, form) works through the generic Atom parser and a TOML row, but loses item codes and exhibit text.
- **Evidence:** rg -i 'edgar|sec.gov' . → 0. Probes: getcurrent Atom, submissions, efts, daily-index, company_tickers.json.

### `jev-event-key` — Event key ⇒ session id, dedupe window, audit key (M0: EDGAR accession)

M0 · S · rust · missing · config | application | inbound · PRD §18 §27 §32

- **Tracker note:** Moved to M0: the event key there is the EDGAR accession; no dependency on `info-store` (convention 15).
- **Have:** The webhook mints a random session per POST, `webhook-<endpoint>-<uuid>` (src/adapters/inbound/webhooks.rs:242-244), and `tengu decide` mints `decide-<loop>-<uuid>` (src/adapters/inbound/cli/decide.rs:40). Every POST spawns an unbounded task (webhooks.rs:259-264). There is no dedupe or single-flight, so duplicate deliveries (Helius documents duplicates, docs/lping-2026-09-24.md:22) run and escalate twice.
- **Need:** config: `[decision_loops.<n>] event_key = "/event_id"` (pointer into the raw event) and `dedupe_secs = 600`. application/decision_loop/mod.rs: handle_event derives event_key and keeps an in-flight set plus a recent map (key -> first_seen_ms). A duplicate returns `StepOutcome::Duplicate` with no Jev call. When event_key is set, session id = `<loop>-<event_key>`, so escalation, research rows, agentic_memory and audit share it. inbound/webhooks.rs and cli/decide.rs pass it through. The audit line gains `event_key`.
- **No-Rust path:** Sender-side dedupe in the feed runner, with the canonical id embedded in the body. Trade-off: session ids stay random, so audit, escalations and memory are not joinable by event, and retries or two senders still double-run.
- **Evidence:** rg -n -i "dedupe|dedup|idempot|single.?flight|seen_events|in_flight" src/application/decision_loop/ src/adapters/inbound/webhooks.rs src/bootstrap/decision.rs -> none; read webhooks.rs:242-264, decide.rs:38-40

### `jev-event-templating` — `{event:/pointer}` in args and world keys + `FromEvent` slots

M0 · M · rust · missing · config | application · PRD §22 §23 §35 S5
After: `info-store` (M4)

- **Have:** Slots = Static | FromHistory | FromObservation only (src/config/decision_loop.rs:125-161). render_args substitutes only `{slot}` values (src/application/decision_loop/slots.rs:106-131). World aliases are literal keys read verbatim (src/config/decision_loop.rs:76-82; src/application/decision_loop/world.rs:63-64), and validation only checks for ':' (src/config/decision_loop.rs:183-189). This is an open item in docs/SESSION_HANDOFF.md:74.
- **Need:** config/decision_loop.rs: add `SlotConfig::FromEvent { event = "/entities/*", value = "id", top }` and validate `{event:<pointer>}` refs in `args` and `world` keys (same path grammar as reduce::select). application/decision_loop/slots.rs: `render_args(template, values, event)` resolves `{event:/event_id}`; a whole-string ref keeps its JSON type and ids are never cut. application/decision_loop/world.rs: `World::read(store, cfg, event, now)` renders templated keys per event; an unresolved path makes that alias `missing`, never a partial key. Tests: templated world key, FromEvent labels `entity_1..n` mapped to full ids, missing path makes the action illegal. Enables world `{ ctx = "xm_ctx/1:{event:/event_id}", research = "xm_research/1:{event:/event_id}" }` and args `{ event_id = "{event:/event_id}" }`.
- **No-Rust path:** Partial only. (1) jev-exec pattern: the architect or feeder pre-renders a full event and the loop uses static slots (sandboxes/jev-exec/config.toml:74-79). (2) One fixed 'focus' row per loop written by a feeder. Trade-off: tools never get the event id, so there are no per-event reads or writes, and a focus row races under concurrency. Rust required.
- **Evidence:** rg -n "FromEvent|from_event|\{event\.|event\." src/application/decision_loop/ src/config/decision_loop.rs -> only doc comments; rg -n "template|interpolat" same paths -> only slots.rs:106-110 render_args; read world.rs:50-86, slots.rs:33-131
- **Absorbed `kg-slot-from-event`** — FromEvent slot source: event fields → Jev slot candidates. Add `SlotConfig::FromEvent { event: "<path>", value: Option<String>, top }` in src/config/decision_loop.rs, with validation (path non-empty; a list path needs `value`). slots::candidates gains the reduced event: a scalar path gives 1 candidate; a list path gives top-N labelled `<slot>_n` with full values kept in code. DecisionLoop::step passes the event. TOML example: `slots = { mention = { event = "/entities/*", value = "mention", top = 5 }, event_id = { event = "/event_id" } }`. Tests in slots.rs. One implementation shared with the jev slice.
- **Absorbed `kg-world-event-keys`** — World keys templated from event fields ({event./path}). World values may contain `{event./path}`. World::read(store, cfg, event, now_ms) renders them per event; a missing path makes the entry `missing`. The rendered key must still be `<schema>:<subject>`. requires / FromObservation keep referencing aliases. Enables `world = { focus = "xm_focus/1:{event./event_id}", hl_sync = "xm_sync/1:hyperliquid" }`, so Jev sees the per-event impact set (features ≤ 32) and FromObservation slots pick instruments from `/data/instruments/*`.

### `risk-config-schema` — `[risk]` + `[paper]` sections: every limit required, no defaults, fail closed; `Config::load` rejects unknown top-level keys

M0 · S · rust · missing · config · PRD §28 §29 §31 §35 S7

- **Tracker note:** Also make `Config::load` reject unknown top-level keys (`deny_unknown_fields` on `Config` after auditing every config, or a raw-table key check): section-level `deny_unknown_fields` cannot catch a misspelled table name (convention 6).
- **Have:** Only per-loop numeric slot caps (src/config/decision_loop.rs:111-114, re-checked at src/application/decision_loop/mod.rs:278-290), dry_run (decision_loop.rs:62-64) and per-tool knobs on the Solana tools (tools/solana/lp.rs:606-648). `Config` has no risk section and no deny_unknown_fields at top level (src/config/mod.rs:122-124), so a misspelled `[risk]` would be silently ignored.
- **Need:** config: src/config/risk.rs `RiskConfig` (deny_unknown_fields, every limit required): account, mode = paper|live, venues, min_lifecycle (paper_tradable), instruments_allow / instruments_deny (full ids such as hyperliquid:xyz:TSLA; the allow-list is the interim §29 gate), max_order_notional_usd, max_position_notional_usd {default, per instrument}, max_asset_exposure_usd (net per underlying across venues), max_venue_exposure_usd, max_gross_exposure_usd, max_net_exposure_usd, max_leverage, daily_loss_limit_usd, total_loss_limit_usd, min_edge_bps, max_slippage_bps, min_depth_usd, require_hedge_for = [convergence], max_data_age_ms {book, ctx, reference, quote}, max_skew_ms, max_orders_per_min, max_open_orders, kill_switch_file. `PaperConfig`: initial_cash_usd, latency_ms, latency_jitter_ms, fee_tier (0-6), staking_discount_pct, order_types. Add `#[serde(default)] pub risk: Option<RiskConfig>` and `paper` to Config (src/config/mod.rs:124). Call validation next to solana::validation_errors (mod.rs:1106). Inject into AgentConfig runtime fields the way signer_key_file is (mod.rs:1023-1028) so tools read it through ctx.agent_config. Tests: parse, every-missing-field error, unknown key error. Update config.example.toml.
- **No-Rust path:** The limits themselves are TOML (doctrine #2). Only the schema and validation (about 150 lines) is Rust. A missing or misspelled section must make the exec tools fail closed, and that needs code.
- **Evidence:** rg -n -i 'paper|kill_switch|killswitch|risk_gate|RiskGate|risk_engine|exposure_limit|max_leverage|daily_loss|loss_limit' src docs sandboxes: only hits are the Jupiter perps decoder max_leverage (src/domain/lp/perps.rs:163) and unrelated 'paper' strings. Read src/config/mod.rs:122-199 and :1068-1119.

### `risk-calc-costs` — Pure costs: L2 depth walk, HL tick / lot rounding, fee schedules (HIP-3 scale), funding carry, gas, edge after costs

M0 · M · rust · missing · domain · PRD §25 §28 §31 §21

- **Have:** Venue-specific pieces only: Jupiter perps fees and price impact (src/domain/lp/perps.rs:633-641, :691-692), DLMM fee rate (domain/lp/gates.rs), Solana CU price clamp (src/domain/solana_write.rs:42-56). No CLOB depth walk and no generic fee model.
- **Need:** domain: src/domain/xm/cost.rs.
  - `walk(book_side, qty|notional) -> Walk {filled_qty, vwap, worst_px, levels_used, slippage_bps_vs_mid, slippage_bps_vs_touch, unfilled}` and `depth_within(book, bps)` in USD.
  - HL rounding: 5 significant figures and at most MAX_DECIMALS - szDecimals decimals (perps 6, spot 8), integer prices always valid, sizes to szDecimals.
  - FeeSchedule: HL perps tier 0 taker 0.045% / maker 0.015%, tiers 1-6, spot tier 0 0.070% / 0.040%, staking discount, HIP-3 deployerFeeScale and growth mode taken from the market row. Knob-driven; re-verify against the first live fill.
  - funding_carry_bps(rate_1h, hours, side).
  - gas_usd(gas_units, gas_price_wei, eth_usd): Robinhood Chain bundles the L1 data fee into gas.
  - edge_after_costs_bps for a round trip.
  Tests: the xyz:TSLA 20-level book fixture with hand-computed VWAPs; the HL doc rounding examples (1234.5 valid, 1234.56 not; 0.001234 valid, 0.0012345 not).
- **No-Rust path:** None. §25: normal software calculates costs; Jev must not do the arithmetic.
- **Evidence:** rg -n -i 'funding_rate|basis_bps|annualis|slippage_bps|fee_bps|taker|maker' src: only Jupiter perps / Solana tool defs (src/domain/lp/perps.rs:635-731, tools/solana/defs.rs:154,210,219).
- **Absorbed `hl-fee-model`** — Pure venue fee model for paper fills (HL perps/spot, HIP-3 deployer scale + growth mode, CEX knobs). domain: src/domain/hl/fees.rs. Base tier: perps taker 4.5 bps / maker 1.5 bps; spot 7 / 4 bps. HIP-3 all-in fee = protocol fee (x0.1 in growth mode) + deployer share. deployerFeeScale ranges 0-3 (at most 1 in growth mode); above 1 the protocol fee is raised to match it. Examples: xyz in growth mode at scale 1.0 = 0.9 bps taker; not in growth mode at scale 1.0 = 9 bps. Adds hourly funding accrual. CEX fees are required TOML knobs with no defaults, like hedge_decide. Outputs: taker_fee_bps in mkt_ctx and a cost(side, notional, book) fn for risk-paper-engine. The formula is derived from the fee docs; confirm it against a testnet fill in P2.

### `risk-calc-tools` — Store-only compute tools `xm_cost`, `xm_compare` (the row the gate re-reads for `min_edge_bps`; M0: HL book vs HL oracle)

M0 · M · rust · missing · outbound/tools + domain/tools · PRD §25 §23 §24 §12 §35 S6
After: `hl-book-tool` (M0), `hl-ctx-tool` (M0), `rh-dex-quote-v3` (M2), `rh-quote` (M2), `risk-calc-costs` (M0)

- **Have:** Pattern: hedge_decide / lp_decide read only store rows, use no network, require knobs and return typed output (src/adapters/outbound/tools/solana/lp.rs:7-9, :606-648, :822-972). Loops read rows through world / requires (src/application/decision_loop/world.rs, mod.rs:141).
- **Need:** adapters/outbound/tools/xm:
  - `xm_cost` (instrument*, side*, notional_usd*, fees* knobs) -> xm_cost/1:<instrument>:<side>:<notional>. Features: vwap, slippage_bps, fee_bps, total_cost_bps, depth_ok, levels_used, book_age_ms.
  - `xm_compare` (a*, b* = instrument or reference key, notional_usd*, knobs*) -> xm_compare/1:<a>:<b>. Features: basis_bps, exec_spread_bps in both directions, cost_rt_bps, funding_8h_bps_a / _b, carry_bps_24h, edge_after_costs_bps, depth_usd_a / _b, age_a_ms, age_b_ms, skew_ms, market_open_a / _b; basis_z_1h and ret_5m_a / _b once risk-calc-market-stats lands. This is the row the gate re-reads for min_edge_bps.
  - `xm_stats` (instrument*, windows) -> xm_stats/1:<instrument> (P1, needs the series store).
  ttl 2-5 s so world / requires can use the rows. First line ctx.scope.check_fs_write(ctx.workspace); no net hosts. Opt-in names and catalog rows.
  Tests: MemStore fixtures give exact features; a stale or missing leg gives partial status with no numbers.
- **No-Rust path:** None for the arithmetic (§25). The loop wiring that consumes the rows is TOML.
- **Evidence:** rg -n -i 'xm_|basis|spread_bps|edge_after' src: no hits. Read tools/solana/lp.rs and application/decision_loop/world.rs.
- **Absorbed `rh-basis`** — Cross-market convergence calculator rh_basis (store-only, multiplier-correct, session-gated). Pure logic in src/domain/xmarket/convergence.rs.
  
  Inputs:
  - rh_quote/1:<contract>
  - rh_dex_quote/1:<contract>:<quote> (plus lighter_rh_book/1:<id>)
  - the HL/HIP-3 row from the hl slice
  - optional eq_quote/1:<SYM>, market_session/1:*, us_halt/1:<SYM>
  
  Computation:
  - Normalise every price to token-equivalent units: share price x uiMultiplier. Lighter's perp multiplier is 1.0, while AAPL/USDG spot is 1.000566080061092436.
  - Executable basis both ways at size (buy RH / sell HL, sell RH / buy HL), net of pool fee, gas, and HL fees plus funding (cost model from the risk slice).
  - Reference-vs-venue and oracle-vs-executable spreads in bps.
  - convergence_enforceable = mint window open AND underlying session open AND not halted AND oracle not paused.
  
  Tool rh_basis, store-only, in src/adapters/outbound/tools/rh/basis.rs:
  - Key rh_basis/1:<SYMBOL> (e.g. rh_basis/1:TSLA), TTL 0 (never cached).
  - A missing or stale input returns blocked with stale_input.
  - Features: basis_buy_rh_bps, basis_sell_rh_bps, net_edge_bps, size_usd, ref_vs_rh_bps, ref_vs_hl_bps, oracle_vs_exec_bps, hl_funding_bps_8h, session, enforceable, max_input_age_s.

### `risk-paper-ledger-domain` — Pure ledger math: positions, cash, average-cost P&L, mark-to-market, exposure, leverage, funding

M0 · M · rust · missing · domain · PRD §25 §31 §28

- **Have:** Only Jupiter-perps-specific math: unrealized P&L that is None without an oracle, never 0 (src/domain/lp/perps.rs:1206), liquidation price (perps.rs:617), fee bps (perps.rs:691-692). The LP hedge controller's exposure lives in src/domain/lp/hedge.rs. There is no venue-neutral position or P&L model.
- **Need:** domain: src/domain/xm/ledger.rs.
  - Position {instrument, underlying, venue, qty (signed), avg_px, realized_pnl, fees_paid, funding_paid, opened_ms}.
  - apply_fill (average cost; a flip closes, then opens).
  - mark(position, Field<mark_px>) -> Field<unrealized>: a missing mark is an Error, never 0.
  - equity = cash + sum(unrealized) - accrued.
  - Net and gross exposure per underlying and per venue at mark; leverage = gross / equity.
  - accrue_funding(position, rate_1h, oracle_px, hour_ms) with the Hyperliquid convention: payment = size x oracle x rate, positive rate means longs pay.
  - Maintenance-margin estimate from the HL margin tables (P1).
  - Observed impl PaperPositions -> paper_positions/1:<account>. Features: n_positions, equity_usd, cash_usd, upnl_usd, rpnl_usd, funding_usd, fees_usd, gross_exposure_usd, net_exposure_usd, leverage, daily_pnl_usd, marks_stale, halted. Per-position rows with full ids go in data.
  Tests: hand-computed vectors (open, increase, partial close, flip, funding across hours). Any stale mark makes the status partial.
- **No-Rust path:** none
- **Evidence:** rg -n -i 'realized_pnl|unrealized|mark_to_market|avg_px|avg_entry|equity_usd|gross_exposure|net_exposure' src: only Jupiter perps (src/domain/lp/perps.rs:768, :1206) and its tool tests.

### `risk-gate-domain` — Pure policy: every §28 rule as a `Check`, fail closed on missing data

M0 · M · rust · missing · domain · PRD §28 §29 §13 §30
After: `risk-config-schema` (M0), `risk-paper-ledger-domain` (M0)

- **Have:** Pre-send checks exist only inside the Solana write tools (Check at src/domain/solana_write.rs:134-150; a failed check refuses at tools/solana/write_common.rs:188-196). They are venue-specific: oracle gate, per-order notional cap in jup_perps_order. There is no account-level exposure, leverage or loss state. Loop caps are per-slot bounds only (application/decision_loop/slots.rs:40-44).
- **Need:** domain: src/domain/xm/{mod,risk}.rs.
  - OrderIntent {account, instrument (full venue:symbol id), underlying, side, qty, notional_usd, reduce_only, strategy, hedge_instrument?, opportunity_key?}.
  - RiskContext {positions, cash, equity, day_start_equity, open_orders, orders_last_min, lifecycle: Field<Lifecycle>, per-leg market {book_age_ms, ctx_age_ms, skew_ms, market_status, depth_usd_within(max_slippage), est_slippage_bps, oi_cap_headroom}, opportunity: Field<edge_after_costs_bps>, halted}.
  - `evaluate(&OrderIntent, &RiskContext, &RiskLimits, now_ms) -> RiskVerdict {allow, rule, checks: Vec<Check>, headroom}`.
  - Rules: venue and instrument permitted; lifecycle >= min_lifecycle (§29); order, position, asset, venue, gross and net notional after the fill; leverage after the fill; daily and total loss (trips the halt); kill switch; min edge after costs (entries only); max slippage and min depth; hedge availability for require_hedge_for strategies; data freshness and cross-venue skew; order rate and open-order limits.
  - Any Field::Absent or Field::Error input denies with `missing:<field>`. Reduce-only exits may pass while halted or stale (configurable).
  - Tests: table-driven boundary vectors (allow at the limit, deny at limit + epsilon), a 'never allow with a missing input' property, reduce-only-under-halt vectors.
- **No-Rust path:** None. §28 requires policy that is independent of Jev and the architect. A SKILL.md or prompt rule is advisory only; only runtime enforcement protects.
- **Evidence:** Same rg as risk-config-schema. `rg -n 'struct Check' src`: only src/domain/solana_write.rs:136. `rg -n -i '\bhalt(ed)?\b|trading_halt|circuit.?breaker' src`: no hits.

### `risk-paper-ledger-store` — `ledger.db`: one account per sandbox, idempotent `client_order_id`, gate + fill + write in one transaction

M0 · M · rust · missing · ports + outbound + bootstrap · PRD §31 §32 §35 S7
After: `risk-paper-ledger-domain` (M0)

- **Tracker note:** Path is `<TENGU_HOME>/state/xmarket/ledger.db` (convention 3); approvals live in `catalog.db`, not here (convention 10).
- **Have:** Pattern only: install-wide SQLite state (src/adapters/outbound/solana/writes_store.rs:1-58, port src/ports/solana_writes.rs:11-35). The observation store is the wrong home: one row per key (observations.rs:23-27), 7-day purge (:21). The agent-writable shared_cache lives in the workspace.
- **Need:** ports: src/ports/paper.rs `PaperLedger` with open_account, snapshot(account), place(intent, verdict, fills) in one transaction, get_by_client_id, record_funding, day_start_equity, risk_state get/set, approvals, append_risk_decision.
  outbound: src/adapters/outbound/paper_store.rs `SqlitePaperLedger` at <TENGU_HOME>/state/xm-paper.db (outside every workspace and fs root), WAL + busy_timeout. Tables: accounts, orders (UNIQUE(account, client_order_id), so a retry returns the stored result), fills, positions, cash, funding (PK account + instrument + hour_ms, so accrual is idempotent), risk_decisions, risk_state, approvals. BEGIN IMMEDIATE wraps gate + fill + write. Account id = [risk] account, defaulting to the sandbox name. `tengu prune` already spares state/ except state/flows (prune.rs:47-56); keep it that way.
  bootstrap: opened once per plugin like SolanaPlugin::tools (tools/solana/mod.rs:94-117). If unavailable, exec tools refuse.
  Tests (tempdir): idempotent replay; a two-connection race where the second order sees the first's exposure; persistence across restart.
- **No-Rust path:** None. shared_cache and persistent_store live in the agent-writable workspace (tamperable, no multi-statement transactions). The ledger must sit outside fs roots.
- **Evidence:** rg -n -i 'ledger|paper_order|client_order_id|cloid|idempoten|client_order' src: only Solana ATA CreateIdempotent (src/domain/solana_tx.rs:435) and 'ledger' in comments of src/domain/lp/wallet.rs:1053. Read observations.rs, writes_store.rs, prune.rs:44-93.

### `risk-paper-fill-engine` — Market / IOC orders, L2 depth-walk fills, partial / failed fills, injected latency, HL rejection codes (ALO in P1; AMM / RFQ path in `rh-paper-fill`)

M0 · L · rust · missing · domain + outbound/tools · PRD §31 §20 §25 §35 S7
After: `hl-book-tool` (M0), `hl-ctx-tool` (M0), `risk-calc-costs` (M0), `risk-paper-ledger-domain` (M0)

- **Tracker note:** M0: market / IOC only; ALO (`BadAloPx`) is P1; the AMM / RFQ quote path moved to `rh-paper-fill` (M6).
- **Have:** Keyless simulation exists only for Solana transactions (tools/solana/write_common.rs:239-266, pipeline src/adapters/outbound/solana/send.rs); it simulates a chain tx, not an order against a book. No order or fill model.
- **Need:** domain (pure): src/domain/xm/paper.rs.
  - PaperOrder {client_order_id, instrument, side, qty|notional_usd, kind: market|limit, tif: ioc (gtc/alo in P1), limit_px?, reduce_only, max_slippage_bps}.
  - `simulate_fill(order, &L2Book{bids, asks, venue_ts_ms}, book_age_ms, &VenueRules{sz_decimals, max_decimals: 6 perp / 8 spot, min_notional_usd: 10, oracle_band}, MarketStatus, position, &FeeSchedule) -> FillResult {status: filled|partial|rejected, fills[{px, qty, level}], avg_px, slippage_bps vs mid, fee_usd, reason}`.
  - A market order is an IOC bounded by max_slippage_bps from mid. It walks levels to the limit and cancels the remainder.
  - Copies Hyperliquid's documented rejections: Tick, MinTradeNtl ($10), ReduceOnly, BadAloPx, IocCancel, MarketOrderNoLiquidity, Oracle (price too far), OI-cap increase.
  - Failed fills on a stale book (> max_data_age_ms.book) or a halted/closed market. Exhausted visible depth gives a partial fill: never assume hidden liquidity (l2Book shows 20 levels per side, probe).
  - A quote path for AMM/RFQ venues: Quote {amount_in, amount_out, gas_units, quote_ts}.
  outbound/tools (latency injection): sleep latency_ms +- jitter, then read the book live (max_age_secs = 0) through the hl slice's typed book tool and fill against THAT book, so the market moves during the latency.
  Tests: book fixtures captured from the probes (tests/fixtures/xm/l2_xyz_TSLA.json), hand-computed VWAP / slippage / rounding vectors, one vector per rejection code.
- **No-Rust path:** none
- **Evidence:** rg -n -i 'order_book|orderbook|l2_?book|depth_walk|vwap|fills? table' src: no hits. HL l2Book probe: 20 levels per side, {px, sz, n} strings plus a venue time.

### `risk-exec-idempotency-ids` — `ToolCtx.call_id` + restart-safe loop ids (`{loop}:{session}:{t}`)

M0 · S · rust · missing · ports + application · PRD §30 §31 §32

- **Have:** Loop tool calls get id = format!("{}-{t}", self.name) (src/application/decision_loop/mod.rs:328-332). `t` restarts with the process because history is in-process (mod.rs:26-28, LoopState at :70-79), so ids repeat across restarts. ToolCtx carries no call id (src/ports/tool.rs:107-123). Arg templates only substitute slot values (src/application/decision_loop/slots.rs:106-131).
- **Need:** ports: add ToolCtx.call_id: Option<&str>, set in PluginToolExecutor::execute_typed from call.id (application/tools/registry.rs:144-171), including the MCP bridge.
  application: loop ids become `{loop}:{session_id}:{t}`.
  Exec tools use client_order_id = the arg if given, else call_id, and refuse when neither exists (never a random id: a retry must deduplicate).
  Tests: two restarts give distinct ids; a retry with the same id returns the stored fill.
- **No-Rust path:** Partial: make client_order_id a required arg. But loop args are static templates filled with slot values only, so a TOML author cannot mint a unique id per call; the small Rust change is needed.
- **Evidence:** Read src/application/decision_loop/mod.rs:26-28, :70-79, :328-332; src/ports/tool.rs:107-123. rg -n -i 'idempoten|client_order|dedup' src: no order-level idempotency.

### `risk-gate-enforcement` — Gate inside every exec tool, in-process and through the bridge alike + `[risk]` load rules: no shell anywhere, `claude_code` only when hardened (convention 12), no `[[mcp_servers]]`

M0 · M · rust · missing · outbound + config (+ ports) · PRD §28 §30 §26 §36
After: `hl-book-tool` (M0), `hl-ctx-tool` (M0), `risk-config-schema` (M0), `risk-gate-domain` (M0), `risk-paper-ledger-store` (M0)

- **Tracker note:** The `[risk]` load rules apply sandbox-wide (convention 12): force `no_shell_fallback` on every agent (`fold_default_scopes`: signer OR `[risk]`) and in the bridge, reject configured `shell_bins` and any `[[mcp_servers]]`, and allow `claude_code` agents only with `builtin_tools_profile = "none"` + `--strict-mcp-config` (`x-claude-code-hardening`). The gate lives inside the exec tool, so it holds through the bridge too (convention 20). Add a test like `signing_sandbox_fallback_runs_no_shell`.
- **Have:** Precedent: the Solana signer grant is checked inside the tool (tools/solana/write_common.rs:163-186, :268-293). Every surface reaches tools through PluginToolExecutor::execute_typed (src/application/tools/registry.rs:144-171): decision loops (src/bootstrap/decision.rs:62-74), run-agent children (bootstrap/tools.rs:339-423) and the MCP bridge (src/adapters/inbound/mcp_bridge.rs:506-546). Load rules exist only for signing (src/config/solana.rs:79-223). Two open bypasses: (1) tools without a configured scope get permissive_scope, which grants the Privy `default` wallet (src/bootstrap/tools.rs:262-272), and sign_and_send_transaction is an always-on catalog row (adapters/outbound/tools/mod.rs:137-142, check at tools/crypto/sign_tx.rs:68-70); (2) run_command scopes check only the first token (sandboxes/jev-exec/config.toml:52), so `tengu …; sqlite3 …` chains run.
- **Need:** adapters/outbound: src/adapters/outbound/tools/xm/exec_common.rs `run_exec(ctx, shared, intent)`:
  - (1) No [risk] section: refuse with `risk_config_missing`.
  - (2) Assemble RiskContext: market rows through ObservationStore::get_many (book, ctx and the opportunity key such as xm_compare/1:<a>:<b>, read fresh), lifecycle, kill-switch file.
  - (3) BEGIN IMMEDIATE on the ledger, re-read positions and orders inside the transaction, call domain::xm::risk::evaluate, then fill and write in the same transaction. This prevents check-then-act races between loops and processes.
  - (4) Put the verdict in the result: features risk = allow|deny and risk_rule.
  Optional port src/ports/risk.rs (RiskGate) implemented by the ledger adapter. A ToolExecutor decorator is rejected: it would have to be threaded through the ~11 call sites that hold the concrete PluginToolExecutor, plus the bridge's own builder, and it still could not make check + fill atomic.
  config: src/config/risk.rs::validation_errors (reusing the path helpers at config/solana.rs:46-69). Rules:
  - XM_EXEC_TOOLS only on an agent with no description, not default, not a webhook agent, and engine != claude_code.
  - No scope with shell_bins in a [risk] sandbox.
  - No [[mcp_servers]] for the exec agent.
  - <TENGU_HOME>/state, kill_switch_file and the sandbox config file outside every fs_roots / workspace.
  - sign_and_send_transaction and sign_message must have an explicit empty scope on every agent.
  - An exec action may not set read_only = true (generalises config/solana.rs:204-223).
  Tests: config-rule tests like config/solana.rs:252-372. Tool test: a denied intent writes no order or fill and returns status = error with `refused`.
- **No-Rust path:** None. The TOML side (private executor agent, empty crypto scopes, no shell) can be written today, but nothing enforces it without the load rules, and the gate itself must run in code.
- **Evidence:** rg -n 'build_tool_executor|build_subprocess_tool_executor|register_catalog|PluginToolExecutor' src: builders at bootstrap/tools.rs:95 and mcp_bridge.rs:412; callers in webhooks.rs:744, eval.rs:1370/1723, tui/mod.rs:584/664/809, telegram.rs:1221/1496/1926, run_agent.rs:281, decision.rs:62. Read bootstrap/tools.rs:95-272, config/solana.rs, sandboxes/jev-exec/config.toml:52, docs/decision-loop-plan-2026-09-24.md:28.

### `risk-paper-tools` — `paper_order`, `paper_close`, `paper_positions` (typed results)

M0 · M · rust · missing · outbound/tools + domain/tools · PRD §22 §30 §31 §35 S7
After: `risk-exec-idempotency-ids` (M0), `risk-gate-enforcement` (M0), `risk-paper-fill-engine` (M0), `risk-paper-ledger-store` (M0)

- **Have:** Pattern: the Solana tool family with opt-in catalog rows (src/adapters/outbound/tools/mod.rs:155-248, names in src/domain/tools.rs:14-70) and write results as ttl-0 observations (src/domain/solana_write.rs:193-217).
- **Need:** adapters/outbound: src/adapters/outbound/tools/xm/{mod,defs,paper}.rs with `XmPlugin`, which opens the observation store and the ledger once (like SolanaPlugin, tools/solana/mod.rs:88-131). The first line of every execute is ctx.scope.check_fs_write(ctx.workspace). Tools:
  - `paper_order` (instrument*, side*, notional_usd*, kind* market|limit, limit_px, tif (ioc), reduce_only, max_slippage_bps*, strategy, hedge_instrument, opportunity (observation key), client_order_id) -> paper_fill/1:<account>:<client_order_id>. ttl 0; features status, risk, risk_rule, filled_notional_usd, avg_px, slippage_bps, fee_usd, latency_ms, book_age_ms, levels_used, position_qty_after, equity_usd_after.
  - `paper_close` (instrument | all = true): reduce-only market IOC through the same gate.
  - `paper_positions` (account defaults to [risk] account) -> paper_positions/1:<account>, ttl 2 s, marks from fresh rows, lazy funding accrual.
  - `paper_cancel` (P1, resting orders).
  domain/tools.rs: the names plus XM_EXEC_TOOLS = [paper_order, paper_close, paper_cancel], added to WORKSPACE_TOOLS, one catalog row each.
  Tests: cargo test --bin tengu catalog, tests/scope_lint.rs, tool tests with MemStore + a tempdir ledger.
- **No-Rust path:** None. http_request + a skill cannot fill against a book or keep a ledger, and an external MCP server would sit outside the gate (and outside the Rust-only repo).
- **Evidence:** rg -n -i 'paper_order|paper_positions|paper_close' src: no hits. Read src/adapters/outbound/tools/mod.rs and src/domain/tools.rs.

### `x-exit-rules` — Exit rules for every open position: take-profit / stop-loss (bps) + max holding time, checked by a `kind = "tick"` feed that calls `paper_close` (live entries also carry exchange-side TP / SL, `risk-hl-exchange`) — keeps the $100 budget turning over

M0 · M · rust · missing · domain + config + outbound/tools · PRD §25 §28 §30 §35 S7
After: `hl-ctx-tool` (M0), `risk-paper-tools` (M0), `rt-scheduler` (M0)

- **Have:** Added 2026-09-30 from the $100 budget decision: with a fixed budget and no exits, the process fills its exposure cap after a few entries and every later entry is refused. Nothing closes positions today; `paper_close` arrives with `risk-paper-tools`.
- **Need:** Pure `domain/xm/exits.rs`: `exit_due(position, mark, now_ms, &ExitRules) -> Option<ExitReason>` over take-profit bps, stop-loss bps and max holding time, all required knobs in `[risk.exits]` (no defaults). A `[feeds.xm_exits] kind = "tick"` row (every 15 s) runs a store-only check over `paper_positions/1:<account>` + fresh `mkt_ctx` marks and calls `paper_close` (reduce-only IOC through the same gate) for each due position; stale marks never trigger a close on their own, and the reason is written to the audit. Live (M3b): every entry also places exchange-side TP / SL trigger orders, so the venue enforces the stop while the process is down; the tick loop only handles max holding time and edge-gone exits. Tests: table-driven TP / SL / max-hold boundaries; a stale mark closes nothing.
- **No-Rust path:** Partly: the tick feed and the knobs are TOML; the exit check must be code, to stay deterministic and inside the risk gate (§25, §28).
- **Evidence:** rg -n 'stop_loss|take_profit|max_hold|exit_rule' src → no hits (2026-09-30).

### `risk-kill-switch` — Kill switch + daily / total loss trip; `tengu risk status / halt / resume` (TTY only); `risk_status` tool

M0 · M · rust · missing · domain + outbound + inbound + outbound/tools · PRD §28 §29 §32 §35 S7
After: `risk-gate-domain` (M0), `risk-paper-ledger-domain` (M0), `risk-paper-ledger-store` (M0)

- **Tracker note:** `risk_state` lives in `<TENGU_HOME>/state/xmarket/ledger.db` (convention 3). The `xm_tradable/1` rows moved to `x-risk-relation-gate` (M6). `tengu risk resume` must refuse without a TTY (`is_terminal()`): the existing prompt pattern accepts a piped `y`.
- **Have:** Nothing at account level. The only pause logic is the LP storm pause for one strategy (src/domain/lp/gates.rs:205 StormPaused). No CLI subcommand for risk (src/adapters/inbound/cli/mod.rs).
- **Need:** domain: src/domain/xm/risk_state.rs, RiskState {halted, reason: daily_loss|total_loss|operator|file, since_ms, day_start_equity, day_utc}. A daily-loss halt clears at 00:00 UTC; total-loss and operator halts clear manually only.
  outbound: risk_state table in xm-paper.db.
  inbound: src/adapters/inbound/cli/risk.rs with `tengu risk status|halt|resume --sandbox xmarket`. The command is operator-only: it refuses without a TTY or when TENGU_AGENT_IPC / TENGU_AGENT_NAME is set, and resume asks the operator to type the account name. The presence of kill_switch_file means halted and is checked on every gate call.
  tools: `risk_status` returns risk_state/1:<account> (ttl 2 s; features halted, reason, equity_usd, daily_pnl_usd, total_pnl_usd, loss_headroom_usd, gross_exposure_usd, net_exposure_usd, leverage, orders_last_min). It also puts xm_tradable/1:<account>: instruments at >= min_lifecycle with remaining headroom, which loops use as a FromObservation slot so Jev can only pick permitted instruments.
  Tests: pure transition vectors; CLI refuses without a TTY.
- **No-Rust path:** Partial: once the gate reads kill_switch_file, `touch <file>` is a no-code operator lever. Loss-limit tripping, the status row and the CLI need Rust.
- **Evidence:** rg -n -i 'kill_switch|killswitch|\bhalt(ed)?\b|circuit.?breaker' src: no hits. Read src/domain/lp/gates.rs:200-240 and src/adapters/inbound/cli/mod.rs (Commands has no risk).

### `risk-audit-verdicts` — `risk_decisions` table joinable with the decision audit by `call_id`

M0 · S · rust · missing · outbound + application + domain · PRD §32 §31 §28
After: `ops-audit-record-v2` (M5), `risk-exec-idempotency-ids` (M0), `risk-gate-enforcement` (M0)

- **Tracker note:** `risk_decisions` lives in `<TENGU_HOME>/state/xmarket/ledger.db` (convention 3).
- **Have:** decisions.jsonl stores answers, outcome, args, reduced output and obs meta (src/application/decision_loop/mod.rs:425-464). A tool refusal appears only as ok = false in history. `tengu prune` deletes <TENGU_HOME>/logs (src/adapters/outbound/prune.rs:56). StepOutcome has no risk variant (src/domain/decision.rs:119-133).
- **Need:** outbound: canonical risk_decisions table in xm-paper.db (never pruned), mirrored to <TENGU_HOME>/logs/risk.jsonl. Record: {ts, account, call_id, session_id, loop?, tool, intent, verdict, rule, checks[], context digest (row keys, ages and values used), fill ref}. Joined to decisions.jsonl by call_id.
  application (optional, S): StepOutcome::Refused {action, rule} when a typed exec result carries features.risk = deny, so §35 S8 can count refusals without parsing output.
  Tests: a denied order writes exactly one verdict row and no order or fill rows.
- **No-Rust path:** none
- **Evidence:** Read src/application/decision_loop/mod.rs:425-464, src/domain/decision.rs:119-133, src/adapters/outbound/prune.rs:47-56. rg -n -i 'risk.jsonl|risk_decision|verdict' src: no hits.

### `ops-audit-atomic-write` — Decision audit: one write per line, a line for failed Jev calls, ms timestamps

M0 · S · rust · partial · application | domain · PRD §31 §32

- **Have:** DecisionLoop::audit (src/application/decision_loop/mod.rs:424-464) opens the file with append (455-458) and calls `writeln!(f, "{line}")` (459) on an unbuffered File. serde_json's Display streams one write_str per token (serde_json-1.0.149 src/value/mod.rs:222-234), so each token is its own syscall. Concurrent loops or processes can therefore interleave partial lines, and the TUI feed silently skips lines it cannot parse (src/adapters/inbound/tui/mod.rs:80-82). `self.engine.decide(..).await?` (mod.rs:202) returns before the audit, so a Jev timeout, 402 or 5xx leaves no line; webhooks.rs:262 only logs a warning. `ts` is unix seconds (mod.rs:435).
- **Need:** application/decision_loop/mod.rs: serialize the whole line to one String with its newline and call a single f.write_all (the pattern in src/adapters/outbound/egress.rs:484-488). When decide() errors, write {result: {outcome: 'error', reason}} and then propagate. Add ts_ms, latency_ms, sandbox (config.sandbox_name, src/config/mod.rs:204) and act_at. domain/decision.rs: add StepOutcome::Error { reason }. render_audit renders error lines. Test: 4 tasks x 500 lines into one file, every line parses.
- **No-Rust path:** none — writer behaviour is code
- **Evidence:** rg -n 'writeln!|write_all|OpenOptions' src/application/decision_loop/mod.rs src/adapters/outbound/egress.rs; read serde_json Display impl in ~/.cargo registry; rustc scratch demo (scratchpad/wfmt.rs): writeln! over a token-streaming Display = 10 write calls vs write_all(format!(..)) = 1; rg -n 'decision loop event failed' src/adapters/inbound/webhooks.rs

### `jev-xmarket-loops-toml` — `[decision_loops.*]` + planner / executor agents; M0 loop `inspect_book` → `compare` → `paper_enter`, `escalate = false` (loop set per convention 8)

M0 · M · toml · missing · config | sandbox · PRD §22 §27 §30 §35 S5 §35 S7
After: `info-store` (M4), `jev-action-kinds` (M5), `jev-classify-questions` (M5), `jev-context-composer` (M5), `jev-event-templating` (M0), `jev-triage-state-tool` (M5), `kg-related-assets` (M5), `risk-paper-tools` (M0)

- **Tracker note:** Loop set per convention 8. M0: one loop `inspect_book` → `compare` (`xm_compare`, HL book vs HL oracle) → `paper_enter`, `escalate = false`. M2: `market_anomaly` stub. M5: escalation on.
- **Have:** No sandboxes/xmarket directory. Loop defaults: history 8, act_at 0.8, max_steps 4, dry_run true, escalate true, Jev timeout 20 s, world max age 30 s (src/config/decision_loop.rs:240-263). Templates: sandboxes/lping/config.toml:47-48,231-345 and sandboxes/jev-exec/config.toml:41-140.
- **Need:** §22 -> actions: ignore / reject / monitor = xm_mark + stop (read_only bookkeeping; reason and until_s [300,1800,7200] as static slots). investigate entity = kg resolve tool (FromEvent /entities/*). discover assets = kg impact tool. inspect market = xm_event_context. request confirmation = info confirmations tool. identify candidate = candidates from xm_event_context + xm_mark state candidate. escalate = escalate = true. paper-enter / reduce / close = paper_order (risk slice, non-read_only, dry_run governs). hold / done = terminal. Agents: xm_planner (openrouter, default, [orchestrator] agent); xm_executor (no description, owns every loop tool, workspace ~/xmarket-workspace). Loop `info_triage`: trigger endpoint xm_info (auth_header_env, info feed); event_key /event_id; event_reduce keeps structured fields only; classify = relevance, info_state, category; world {triage = xm_triage/1:event:{event:/event_id}, research = xm_research/1:{event:/event_id}, impact = xm_impact/1:{event:/event_id}}; actions ignore, monitor, investigate_entity, discover_assets, request_confirmation, inspect_market, identify_candidate, reject, escalate, done; act_at 0.8; max_steps 8. Loop `market_anomaly`: endpoint xm_anomaly from detectors; event_key /anomaly_id; world {mkt = xm_mkt/1:{event:/venue}:{event:/instrument}}; actions ignore, inspect_market, search_news, link_event, monitor, escalate, done. Both loops dry_run = true in S5. S7 adds `opportunity` (paper_enter: candidate FromHistory + size_usd [100,250,500,1000], caps size_usd 1000, requires ctx <= 15 s) and `position_manager` (every_secs 30, world book, hold / reduce / close).
- **No-Rust path:** This is the TOML itself (doctrine #2). While dry_run, write actions may name tools not built yet (src/config/mod.rs:1120-1131).
- **Evidence:** ls sandboxes/xmarket -> No such file or directory; rg -c -i "xmarket|xm_" src/ sandboxes/ -> none. Cross-slice ids (info-*, kg-*, hl-*, rh-*, risk-*, ops-*, rt-*) are named by convention; reconcile them with the other slices.

### `rt-health` — Feed / loop health rows, heartbeat, `tengu doctor --live` for the Docker healthcheck

M0 · S · rust · missing · application | inbound · PRD §20 §28 §35 S2
After: `rt-daemon` (M0)

- **Tracker note:** Moved to M0 (2026-09-30): the continuous process runs on the operator's VPS from M0, and Docker's healthcheck needs it. The flag is `tengu doctor --live` everywhere.
- **Have:** Health = `tengu doctor` (config + engine build + Tor reachability) used as the Docker healthcheck (docker-compose.yml:43-48, Dockerfile:61-62). Nothing observes a running feed or loop. The only design note is heartbeat row `stream/1:<name>` (docs/SESSION_HANDOFF.md:49).
- **Need:** application: the runtime writes typed rows `feed/1:<name>` (state connecting|live|backoff|stalled|down, last_item_age_s, reconnects, dropped, last_error_class) and `loop/1:<name>` (queue_depth, in_flight, last_decision_age_s). Loops and risk can gate on them via `requires = { hl_feed = 10 }` (data quality, §28). inbound: heartbeat JSON <TENGU_HOME>/state/run-<sandbox>.json every 5 s. `tengu doctor --sandbox <s> --live` exits non-zero when the heartbeat is older than 30 s or a `required = true` feed is down or stale. Compose healthcheck switch (ops).
- **No-Rust path:** None; grep tengu.log / egress.jsonl by hand.
- **Evidence:** rg -i 'healthz|/health|liveness|readiness|heartbeat|last_seen|watchdog' src -> only channel.rs:216 (skill readiness text); read docker-compose.yml, Dockerfile.
- **Absorbed `ops-doctor-liveness`** — Liveness healthcheck for the long-running xmarket daemon. `tengu doctor --sandbox xmarket --liveness`:
  - build every [decision_loops] entry (JevClient::from_env);
  - Postgres SELECT 1 when TENGU_MEMORY_DATABASE_URL is set;
  - heartbeat rows feed/1:<name> no older than [liveness] max_feed_age_secs;
  - last audit record no older than max_decision_gap_secs;
  - recorder disk free ≥ min_free_gb;
  - OpenRouter GET /api/v1/key limit_remaining > 0.
  Exit non-zero on any failure. Compose adds an autoheal sidecar, or the daemon exits by itself on fatal staleness.

### `x-weekend-fade-strategy` — Deterministic weekend-fade rule (W): at Sun 18:00 ET (last closed day before a trading day) s = ln(HL at 18:00 / HL at Fri 20:00 ET) per name; paper-fade every eligible name in a shadow ledger (no cap, depth-walk fills) and the 4 names with the largest absolute s, at least 50 bps, at $25 each in the capped ledger; exit Mon 09:00 ET; ET clock ticks via `rt-scheduler`

M0 · M · rust · missing · domain (xm/weekend_fade) + outbound/tools (xm) + config · PRD §21 §31 §35 S6 §35 S7
After: `hl-book-tool` (M0), `hl-ctx-tool` (M0), `kg-calendars` (M1), `ops-history-recorder` (M1), `risk-paper-fill-engine` (M0), `risk-paper-tools` (M0), `rt-scheduler` (M0), `x-exit-rules` (M0)

- **Have:** Only the feasibility scripts (scratch, not kept): in-sample +46 bps net per trade (30 weekends, 22 names); holdout +49.8 bps on 53 other names (`docs/xmarket-feasibility-2026-09-30.md`).
- **Need:** Pure `domain/xm/weekend_fade.rs`: signal s = ln(HL at Sun 18:00 ET / HL at Fri 20:00 ET) per name, selection = every eligible name (shadow) and the 4 largest absolute s of at least 50 bps (capped). Tool `xm_weekend_fade` (typed; prices from the recorder / `mkt_ctx` history, books from `hl_book`): places the capped fades through `paper_order` ($25 each, inside the `[risk]` gate) and records shadow fills for every eligible name through the fill engine at the recorded book (no cap). Exits at Mon 09:00 ET through the time exit of `x-exit-rules`. Schedule: `rt-scheduler` clock ticks at Sun 18:00 and Mon 09:00 `America/New_York` on the last closed day / next trading day (`kg-calendars`). Tests: a replay of the 2026-09-26 → 09-28 weekend reproduces the feasibility signals; the 2026-11-01 DST switch; a 3-day weekend.
- **No-Rust path:** None for the rule (deterministic, §25); schedule and knobs are TOML.
- **Evidence:** `docs/xmarket-feasibility-2026-09-30.md` (in-sample + holdout sections).

### `x-weekend-sandbox` — `sandboxes/xmarket-weekend/config.toml`: floor profile (no LLM, no X, Jev off), `hl_ctx` every 60 s + `hl_book` every 5 min (60 s around entry and exit) with recording, the W strategy, its own state dir; runbook with `caffeinate`; offline replay of the 2026-09-26 → 09-28 weekend + a 30-min live soak (build plan § weekend sandbox)

M0 · S · toml · missing · sandbox + docs · PRD §31 §35 S2 §35 S6
After: `ops-history-recorder` (M1), `rt-daemon` (M0), `rt-health` (M0), `x-weekend-fade-strategy` (M0)

- **Have:** A throwaway shell sampler records the 2026-10-02 → 10-05 weekend books into `~/.tengu/state/xmarket/research/weekend-2026-10-02/` (outside the repo). No tengu sandbox exists for it.
- **Need:** `sandboxes/xmarket-weekend/config.toml`: floor profile (no LLM, no X, Jev off); `[egress] network = "open"`; `[risk]` $100 caps and `[paper] initial_cash_usd = 100`; feeds `hl_ctx` every 60 s for listed xyz markets and `hl_book` every 5 min (60 s during Sun 17:00–19:00 and Mon 08:30–09:30 ET), recorder on; the W strategy ticks; state in `<TENGU_HOME>/state/xmarket-weekend/`. A runbook block in the file: start (`tengu run --sandbox xmarket-weekend`), keep awake (`caffeinate -i -s -w <pid>`), check and stop. Acceptance: an offline replay of the 2026-09-26 → 09-28 weekend plus a 30-minute live soak with `tengu doctor --live` green; if not green by Fri 18:00 ET, skip the weekend (the sampler covers the data).
- **No-Rust path:** This item is TOML + docs; it needs the Rust items it depends on.
- **Evidence:** `docs/xmarket-build-plan-2026-09-30.md` § weekend sandbox.

### `ops-deploy-compose` — Docker on the operator's Hetzner / Hostinger VPS: `tengu run` service with `restart: unless-stopped`, persistent `<TENGU_HOME>/state/xmarket` + workspace volumes, `webhooks` feature, healthcheck `tengu doctor --live`; operator chat on demand

M0 · M · infra · partial · infra · PRD §35 S2
After: `rt-daemon` (M0), `rt-health` (M0)

- **Tracker note:** Moved to M0 (2026-09-30): the continuous process runs on the operator's Hetzner or Hostinger VPS from the first milestone.
- **Have:** There is one tengu service with CMD telegram (Dockerfile:65) and default FEATURES openrouter,telegram (Dockerfile:14; docker-compose.yml:27). The only volumes are the config and /opt/tengu/data (docker-compose.yml:38-40). A sandbox workspace '~/…' resolves to /root/… inside the container and is lost when it is recreated (observations.db, research files). The webhook port is published only under NETWORK=open (docker-compose.tor.yml:23). cloud-init provisions a Hetzner cx22 (40 GB disk) running `make up` (deploy/cloud-init.yml:6-11).
- **Need:** docker-compose.xmarket.yml (infra, used with docker-compose.yml under NETWORK=open):
  - Service xm-daemon: image tengu-cluster, command `webhooks` today (the rt daemon command later), TENGU_CONFIG = mounted sandboxes/xmarket/config.toml; [webhooks] bind = '0.0.0.0' only if a port is published.
  - Service tengu: command `telegram`, for operator chat.
  - The existing postgres-memory profile.
  - Image FEATURES openrouter,telegram,webhooks,postgres_memory.
  - Named volume xm-workspace:/root/xmarket-workspace on both services, plus tengu-data. SQLite WAL across containers on one host is fine.
  - Healthcheck `tengu doctor --liveness` plus an autoheal sidecar (label autoheal=true).
  - json-file logging with max-size 50m.
  - A data volume of at least 100 GB for the recorder (cx22's 40 GB is too small).
  - chrony/NTP on the host, since timestamps drive lead-lag.
  - Nightly VACUUM INTO backup of state/audit and state/history to off-host storage.
  - Makefile: an EXTRA_COMPOSE hook or an up-xmarket target.
- **No-Rust path:** yes — compose and Makefile only; no Rust needed
- **Evidence:** read docker-compose.yml, docker-compose.tor.yml, Dockerfile, Makefile, deploy/cloud-init.yml; rg -n 'expand_tilde' src/bootstrap/decision.rs:60 (workspace ~ expansion)

### `x-m0-e2e-test` — Offline end-to-end test (HL ctx / book / `perpDexs` / OI-cap + EDGAR fixtures, scripted Jev) under `cargo test --bin tengu`, < 30 s

M0 · M · rust · missing · application | outbound | bootstrap (test only) · PRD §13 §30 §31 §32 §35 S7
After: `hl-book-tool` (M0), `hl-ctx-tool` (M0), `info-edgar` (M0), `jev-event-key` (M0), `jev-event-templating` (M0), `ops-audit-atomic-write` (M0), `risk-audit-verdicts` (M0), `risk-kill-switch` (M0), `risk-paper-tools` (M0), `rt-daemon` (M0), `rt-scheduler` (M0), `x-shared-workspace-and-state-layout` (M0)

- **Tracker note:** Fixtures also include `perpDexs` + `perpsAtOpenInterestCap` replies; the scripted run is `inspect_book` → `compare` → `paper_enter` and asserts the `xm_compare/1` row.
- **Have:** Only per-module unit tests. Decision-loop tests drive a scripted DecisionEngine inside src/application/decision_loop/mod.rs (tests from :537). tests/ holds code_map.rs, layering_lint.rs, scope_lint.rs, run_agent_ipc.rs and mcp_bridge_external.rs. No test crosses feed tick → loop → exec tool → ledger → audit.
- **Need:** One #[tokio::test] in the binary crate: runs under `cargo test --bin tengu` in under 30 s, no network. A replay HTTP transport serves JSON/XML fixtures captured from the 2026-09-29 probes: tests/fixtures/xmarket/hl_meta_ctx_xyz.json, hl_l2book_xyz_TSLA.json, edgar_getcurrent_8k.atom, company_tickers_exchange.json. A scripted DecisionEngine returns inspect_book, then paper_enter. Uses a tempdir TENGU_HOME and one shared workspace. Assert: (1) the HL feed tick writes mkt_ctx/1:hyperliquid:xyz:TSLA; (2) a new 8-K accession emits exactly one loop event, and a repeated poll emits none; (3) one risk_decisions row (allow) and one fill row share the call_id; (4) paper_positions/1:<account> VWAP and fee equal values hand-computed from the 20-level book; (5) every decisions.jsonl line parses and carries the accession-derived session id; (6) with the kill-switch file present, the next paper_enter returns risk = deny, rule = kill_switch, and writes no fill.
- **No-Rust path:** none — the repo is Rust-only, and the slice's guarantees (atomic gate + fill, dedup, audit join) can only be proven in code
- **Evidence:** ls tests/ (5 files, no end-to-end test); read the src/application/decision_loop/mod.rs test module (scripted engine, test escalator at :586); user rule: test runs capped at 30 s

### `rt-docs` — Runtime operator doc + code map / egress / decision-loop plan / webhooks doc / config example

M0 · S · docs · missing · n/a · PRD §13 §35 S2 §35 S3
After: `rt-daemon` (M0)

- **Have:** Trigger docs cover only webhooks and `tengu decide` (docs/webhooks-2026-05-11.md; docs/decision-loop-plan-2026-09-24.md:17, 33, 120). The egress doc has no ws/stream paths (docs/egress-2026-09-16.md:27-44).
- **Need:** Add docs/runtime-<date>.md (one screen: `tengu run`, tables for [runtime]/[feeds]/[detectors]/[budgets]/[stages], bus topics, drop policies, health, latency table). Update docs/egress-2026-09-16.md (ws_connect, stream_client, audit records), docs/decision-loop-plan-2026-09-24.md (triggers, lanes, resume), docs/code-map.md + .html (new files), docs/SESSION_HANDOFF.md, config.example.toml, and the CLAUDE.md + AGENTS.md gotchas.
- **No-Rust path:** n/a (documentation only).
- **Evidence:** read docs/webhooks-2026-05-11.md, docs/decision-loop-plan-2026-09-24.md, docs/egress-2026-09-16.md, docs/code-map.md.

### `hl-docs` — HL tools in typed-observations, code map, tools.md, egress hosts, CLAUDE / AGENTS gotchas

M0 · S · docs · missing · n/a · PRD §32 §35 S1
After: `hl-book-tool` (M0), `hl-ctx-tool` (M0), `kg-sync-hyperliquid` (M1)

- **Have:** docs/typed-observations-2026-09-24.md covers Solana only; docs/code-map.md:65-66.
- **Need:** (1) docs/typed-observations-2026-09-24.md: new § Hyperliquid + CEX tools (args, keys, TTLs, hosts, weights, error mapping). (2) docs/code-map.md + .html GRAPH rows (tests/code_map.rs enforces them). (3) docs/tools.md. (4) docs/egress-2026-09-16.md: hosts api.hyperliquid.xyz, fapi.binance.com, api.binance.com, data-api.binance.vision, api.bybit.com, www.okx.com, api.exchange.coinbase.com, api.coinbase.com, api.international.coinbase.com. (5) CLAUDE.md + AGENTS.md gotchas: dex:COIN names, '500 null', weight budget. (6) SESSION_HANDOFF.
- **No-Rust path:** Docs only.
- **Evidence:** rg -n -i hyperliquid docs: only xmarket-prd-2026-09-29.md. No xmarket tracker file yet.

### `x-info-docs` — Information-slice doc (M0 part: EDGAR adapter + tool rows)

M0 · S · docs · missing · n/a · PRD §15 §32
After: `info-edgar` (M0)

- **Have:** The information report has no docs gap. tests/code_map.rs fails when a new src file is missing from docs/code-map.md or from the html GENERATED block (tests/code_map.rs:1-12). The typed-observations and egress docs cover Solana schemas and hosts only.
- **Need:** Add docs/xmarket-information-<date>.md, one screen: source table with tier, org, cadence, host and User-Agent rule; [feeds] rows; dedup and extract knobs; observation keys news_item/1, news_feed/1, xm_event/1, xm_entity_news/1, why_moving/1; `tengu news` CLI. Update docs/code-map.{md,html}, docs/typed-observations-2026-09-24.md (news schemas), docs/egress-2026-09-16.md (www.sec.gov, data.sec.gov and efts.sec.gov with a mandatory User-Agent; exchange and regulator hosts), docs/tools.md and config.example.toml. The M0 part covers only the EDGAR adapter and tool rows.
- **No-Rust path:** n/a (docs)
- **Evidence:** read tests/code_map.rs:1-12; the info report lists 25 gaps and none is docs; rg -i 'edgar|sec.gov' over the repo (PRD excluded) → 0

### `risk-docs` — Risk + paper operator doc, observation keys, CLAUDE / AGENTS gotchas

M0 · S · docs · missing · n/a · PRD §28 §31 §32
After: `risk-gate-enforcement` (M0), `risk-paper-tools` (M0)

- **Have:** Pattern docs: docs/typed-observations-2026-09-24.md (keys, TTLs, write rules) and docs/decision-loop-plan-2026-09-24.md. The CLAUDE.md REQUIRED-updates table. tests/code_map.rs fails when a source file is missing from docs/code-map.md.
- **Need:** docs/xmarket-risk-paper-<date>.md, one screen: [risk] fields; gate rules and codes; where the gate is enforced; ledger schema; fill model and rejection codes; observation keys paper_fill/1, paper_positions/1, risk_state/1, xm_tradable/1, xm_cost/1, xm_compare/1, xm_stats/1, xm_opportunity/1; `tengu risk …` CLI. Also update docs/code-map.{md,html}, docs/tools.md, config.example.toml, and CLAUDE.md + AGENTS.md gotchas (exec tools only on a private agent; no shell in [risk] sandboxes; ledger outside fs roots).
- **No-Rust path:** n/a (docs)
- **Evidence:** Read CLAUDE.md REQUIRED updates, docs/code-map.md §6 (tests/code_map.rs).


## M1 — universe, lifecycle, recording

### `kg-domain-model` — Pure types: entities, instruments, ext ids, equivalence, edges, §7 strength

M1 · M · rust · missing · domain · PRD §4 §5 §7 §19 §35 S1

- **Have:** nothing. src/domain holds only Solana LP types (domain/lp/*) and the generic Observation envelope (domain/observation.rs:154).
- **Need:** src/domain/xmarket/{mod,ids,instrument,graph,normalize}.rs, pure with no IO. ids.rs: EntityId `<kind>:<slug>`, kind ∈ company|token|commodity|index|fx|rate|sector|fund; listing public|private is an attribute, so an IPO never changes the id (e.g. company:anthropic). InstrumentId `<venue>:<venue-native id verbatim>`: hl:xyz:TSLA, hl:BTC, hlspot:@107, rh:0x322F0929c4625eD5bAd873c95208D54E1c003b2d, ref:XNAS:TSLA. ExtId schemes: cik|lei|isin|figi|figi_share|wikidata|coingecko|hl_token|sic. instrument.rs: Instrument {venue, native_id, symbol, kind spot|perp|stock_token|etf|index|fx|commodity|rate|preipo_perp, underlying Option<EntityId>, ratio as a decimal string (RH currentMultiplier, HL `k` prefix = 1000), quote, calendar_id, reference_calendar_id, max_leverage, margin_mode, sz_decimals, fee_scale, oracle_ref, status listed|delisted|halted|oi_capped|untradable, sessions}. graph.rs: Edge {from, to, kind supplier|customer|competitor|investor|parent|subsidiary|sector_member|derivative|proxy, role (cloud_provider, chip_supplier…), strength strong|possible|speculative, status proposed|accepted|rejected|retired, provenance seed|sec_sic|wikidata|gleif|architect|operator|market_confirmed, evidence[{url, quote, at}], last_confirmed_ms}. ImpactStrength direct|strong|possible|speculative, with path strength = weakest link; `direct` only for the event's own entities (§7). normalize.rs: HL category table (stock→stocks, fx→FX), k-prefix ratios, symbol normalization. Unit tests use the probed rows (io:ANTH, xyz:TSLA, km:USOIL, kPEPE).
- **No-Rust path:** none. The store, the tools and the risk gate share these invariants (ratio math, strength ordering, id grammar); TOML cannot enforce them.
- **Evidence:** rg -n -i 'instrument|market_universe|universe|symbol_master|tradable|tradeable' src/ (excl. domain/lp, tools/solana) → 0; rg -n -i 'entity_id|entities|relationship|relation_edge|asset_graph|knowledge_graph|kg_|edge_kind|supplier|competitor' src/ → 0; rg -n -i '\bvenue|ticker|equivalen|canonical_id|asset_id|underlying' src/ → only Guard::VenuePermission (src/domain/lp/snapshot.rs:1264); read docs/code-map.md §7 domain file list.

### `kg-catalog-store` — `CatalogStore` port + `catalog.db` system of record (instruments, equivalence, edges, lifecycle, approvals)

M1 · L · rust · missing · ports + outbound + config + bootstrap · PRD §5 §12 §19 §29 §32 §33 §35 S1
After: `kg-domain-model` (M1)

- **Tracker note:** Path is `<TENGU_HOME>/state/xmarket/catalog.db` (convention 3); §29 lifecycle + approvals live only here (convention 10).
- **Have:** observations.db: latest row per key, 7-day purge (src/adapters/outbound/observations.rs:21,23-36,66-69). shared_cache: untyped namespace/key/json (src/adapters/outbound/tools/cache/mod.rs:22-29). agentic_memory: memory_events/sources/chunks/promotions only (src/adapters/outbound/tools/agentic_memory/mod.rs:535-578). Precedent: SqliteWriteStore (src/adapters/outbound/solana/writes_store.rs:1-58).
- **Need:** Port src/ports/catalog.rs::CatalogStore:
  - upsert_entity, ext_ids, aliases
  - upsert_instruments(batch, sync_run) in ONE transaction with a change log; set_status
  - equivalence ops; edges propose/review/retire
  - related(entity, max_hops ≤ 2, min_strength, statuses) via recursive CTE; search_names (FTS5)
  - lifecycle get + CAS transition + events; reactions / edge_stats / event_impacts upserts; sync_runs
  Adapter src/adapters/outbound/catalog_store.rs::SqliteCatalogStore: WAL, busy_timeout 5000, foreign_keys, schema_version + migrations, `VACUUM INTO` backup.
  Tables:
  - entities; entity_ext_ids (UNIQUE scheme,value); entity_names_fts (FTS5)
  - instruments; instrument_changes (append-only); equivalence
  - edges (UNIQUE from,to,kind); edge_events (append-only)
  - lifecycle; lifecycle_events (append-only, actor)
  - reactions; edge_stats; event_impacts; sync_runs
  Config src/config/xmarket.rs: `[xmarket] catalog_db` (default <TENGU_HOME>/state/xmarket.db), validated outside every fs root and workspace (as in src/config/solana.rs:110-146). Wiring src/bootstrap/xmarket.rs. Lifecycle reads are fail-closed: Err is never treated as 'absent'.
- **No-Rust path:** none. agentic_memory Postgres is optional (Cargo.toml:56,78), has no graph tables, and the plugin is moving out of Tengu (docs/agentic-memory-implementation-2026-05-13.md:5-10,23-30). observations.db drops rows after 7 days and keeps no history. shared_cache has no schema or queries. None is fit to be the store a risk gate reads.
- **Evidence:** read observations.rs, cache/mod.rs, agentic_memory/mod.rs:520-600, writes_store.rs; rg -n 'memory_claims|memory_links' → docs only (agentic-memory-implementation-2026-05-13.md:74-78, SESSION_HANDOFF.md:280); rg -n 'CREATE TABLE' src/ → observations, leases/pending_sends/fences, cache_entries, memory_* only.

### `kg-sync-hyperliquid` — HL sync: all 11 dexes via one `allPerpMetas` + spot + annotations, listing diffs, asset-id formula

M1 · M · rust · missing · outbound + application · PRD §4 §14 §19 §35 S1
After: `hl-info-client` (M0), `kg-catalog-store` (M1)

- **Tracker note:** Observed / tradable dexes: `default`, `xyz`, `para`, `mkts`, `io`. `flx`, `vntl`, `hyna`, `km`, `abcd`, `cash` have 0 listed markets (live probe 2026-09-29). An M0 subset (per-sweep `perpDexs` + `perpsAtOpenInterestCap` → `mkt_instrument/1`) ships inside `hl-ctx-tool`.
- **Have:** nothing HL-specific. post_json exists only inside the Solana family (src/adapters/outbound/solana/http_json.rs:36-45).
- **Need:** (1) Hoist fetch_json/post_json to src/adapters/outbound/http_json.rs (shared with Solana and the hl slice).
  (2) src/adapters/outbound/xmarket/hyperliquid.rs, per run:
  - allPerpMetas (1 call, 11 dexes)
  - perpDexs (deployer, oracleUpdater, feeRecipient, OI caps)
  - perpsAtOpenInterestCap per dex
  - perpConciseAnnotations (category, displayName, keywords)
  - perpAnnotation only for new/changed coins (weight 20 each; budget 1200/min/IP)
  - spotMeta (pairs `@N` → base/quote tokenId)
  (3) Map rows to Instrument: `hl:<name verbatim>` / `hlspot:<pair name>`. Kind from the normalized category. Status from isDelisted + OI cap. Carry maxLeverage, marginMode, szDecimals, deployerFeeScale, growthMode; ratio 1000 for the `k` prefix.
  (4) Mapping hints parsed from descriptions: `(Nasdaq|NYSE|NYSE Arca): TICKER` ⇒ same underlying; `Primary oracle pricing reference: NYSE:USO` ⇒ proxy.
  (5) Mass-delist guard: > 20% of a dex delisted in one run aborts the run as a source error.
  Entry points: tool `xm_universe_sync {venue = "hyperliquid"}` and `tengu xm sync --venue hyperliquid`. Writes `xm_sync/1:hyperliquid` (features: n_live, n_added, n_delisted, n_unannotated).
- **No-Rust path:** http_request + skill only for one-off lookups (e.g. one perpAnnotation). Bulk is not viable: allPerpMetas is 82 KB / 529 markets, loop reducers cut arrays at 20 items (src/application/decision_loop/reduce.rs:17), and an LLM copying rows is neither reproducible nor auditable.
- **Evidence:** rg -n -i 'hyperliquid|hip-3|hip3|perpDexs|robinhood|xyz:' across repo → only docs/xmarket-prd-2026-09-29.md; live probes perpDexs / allPerpMetas / meta / metaAndAssetCtxs / perpConciseAnnotations / perpAnnotation / spotMeta / perpsAtOpenInterestCap (see probes).
- **Absorbed `hl-universe-tool`** — hl_universe typed tool: every perp dex (incl. HIP-3), spot pairs, annotations, OI caps, listing diffs. domain: src/domain/hl/{mod.rs,universe.rs} with decoders for: (a) perpDexs. Index 0 is null (the default dex). Builder dex i gets asset offset 100000 + 10000*i, e.g. xyz:TSLA = 110001, xyz:AAPL = 110009, para:STX = 180010, io:ANTH = 200001. (b) allPerpMetas: all 11 metas in one weight-20 call. (c) spotMeta: pairs named @<index>, spot asset id = 10000 + index; token fullName and evmContract. (d) perpConciseAnnotations: category + keywords. (e) perpDexLimits.coinToOiCap and perpsAtOpenInterestCap. (f) perpAnnotation, fetched lazily for new coins only (weight 20 each, at most 20 per refresh), cached 24 h as hl_annotation/1:<coin>. The description parser extracts '(Nasdaq: TSLA)', '(KRX: 005930)' plus a KRW->USD flag, '(Cboe BZX: DRAM)', and ADS ratios ('one-tenth of 1 common share'). Output: HlUniverse at hl_universe/1:all (TTL 300 s; features n_dexes, n_perps, n_perps_listed, n_hip3_listed, n_spot_pairs, n_new_listed, n_newly_delisted, n_at_oi_cap), plus one mkt_instrument/1:hyperliquid:<coin> row per instrument. Tool: src/adapters/outbound/tools/hyperliquid/{mod.rs,defs.rs,universe.rs}, named hl_universe. Args: query (keyword, ticker or displayName), category, dex, include_delisted (default false), max_age_secs. Filtered answers are returned but never stored under the canonical key. Registration: HL_UNIVERSE in WORKSPACE_TOOLS + one ToolEntry. Scope: net_hosts = [api.hyperliquid.xyz], fs_roots = [workspace].

### `kg-sync-robinhood` — RH stock tokens from `rhj/assets` ∩ StockFactory (chain-only tokens stay DISCOVERED), Chainlink feed map

M1 · S · rust · missing · outbound + application · PRD §4 §8 §19 §35 S1
After: `kg-catalog-store` (M1), `kg-sync-us-reference` (M1), `rh-evm-rpc` (M1)

- **Have:** nothing (rg robinhood → 0 in src)
- **Need:** src/adapters/outbound/xmarket/robinhood.rs: GET https://api.robinhood.com/rhj/assets. Map each asset to an Instrument `rh:<contractAddress>`, deployments with chainId 4663 only; the address is canonical because the docs warn about counterfeit tokens.
  - symbol = tokenSymbol; name = tokenName
  - ISIN becomes an ext-id; link to company via kg-sync-us-reference ⇒ deterministic MAPPED
  - ratio = currentMultiplier as a decimal string, never f64. Non-empty pendingMultiplier ⇒ corporate-action hold flag
  - status from ASSET_STATUS_*; sessions market/extended/overnight × whole/fractional from TRADING_STATUS_*
  - calendar `rh_session` (tokenization window Mon 02:00–Sat 02:00 CET/CEST per docs)
  The parser must tolerate unknown fields (undocumented gRPC-gateway API). Entry: `xm_universe_sync {venue = "robinhood"}`, writes `xm_sync/1:robinhood`.
- **No-Rust path:** Partial: a TOML-mapped generic JSON source (kg-generic-venue-mapper, reusing the reduce.rs path grammar) could ingest this single 163 KB GET. Trade-off: multiplier / corporate-action / ISIN semantics still need code, so a ~200-line adapter is simpler for P0.
- **Evidence:** rg -n -i robinhood src/ → 0; live probe rhj/assets (195 assets, chain 4663, ISIN on all) and ?page_size → HTTP 400 GetAssetsRequest (no pagination).
- **Absorbed `rh-assets`** — Stock-token registry tool rh_assets (RH API ∩ StockFactory ∩ Chainlink feed map). Tool rh_assets, typed and opt-in.
  - Files: src/adapters/outbound/tools/rh/{mod,defs,assets}.rs plus RhPlugin/RhShared holding the observation store, like SolanaShared (tools/solana/mod.rs:59-78); catalog row (tools/mod.rs:78); name in domain/tools.rs:28.
  
  Sources:
  1. GET https://api.robinhood.com/rhj/assets: 195 ACTIVE rows with tokenSymbol, deployments[].contractAddress/chainId, isin, id (uid), tokenDecimals, currentMultiplier, pendingMultiplier (+EffectiveTime), tradingCapabilities {market,extended,overnight} x {whole,fractional}, and status.
  2. StockFactory 0x4783C67b63dE2B358Ac5951a7D41F47A38F3C046 Deployed logs: 204 so far, tracked by cursor row rh_cursor/1:stock_factory:4663.
  3. https://reference-data-directory.vercel.app/feeds-robinhood-mainnet.json: match 'Robinhood <SYM> / USD' to proxy, decimals and heartbeat.
  
  domain: src/domain/rh/registry.rs with RhAsset and a §29 stage:
  - chain-only = DISCOVERED;
  - API ACTIVE = IDENTIFIED;
  - plus feed and pool = OBSERVABLE.
  
  Observation keys:
  - rh_assets/1:4663, TTL 3600 s. Features: n_active, n_chain_only, n_with_feed, n_pending_multiplier.
  - rh_asset/1:<contract>, TTL 3600 s. Features: symbol, isin, multiplier, pending_multiplier, effective_at_s, has_feed, tradable_market, tradable_extended, tradable_overnight, stage.

### `kg-sync-us-reference` — Reference equity master: SEC CIK + tickers, Nasdaq symbol directory, OpenFIGI

M1 · M · rust · missing · outbound + application · PRD §4 §8 §19 §35 S1
After: `kg-catalog-store` (M1)

- **Have:** nothing (no CIK/ISIN/FIGI handling in src)
- **Need:** src/adapters/outbound/xmarket/reference.rs, all requests with the declared UA from `[xmarket] user_agent`:
  - SEC company_tickers_exchange.json daily (cik, name, ticker, exchange; ≤ 10 req/s)
  - data.sec.gov/submissions/CIK##########.json on demand (sic, sicDescription, formerNames → aliases + sector_member edges)
  - Nasdaq Trader nasdaqlisted.txt / otherlisted.txt daily (ETF flag, test issues excluded, exchange)
  - OpenFIGI /v3/mapping batches for new ISINs/tickers (keyless 25 req/min × 10 jobs; optional OPENFIGI_API_KEY)
  Output: entities company:<slug> with ext ids (cik, isin, figi_share); reference instruments `ref:<MIC>:<TICKER>` (tradable_by_us = false, calendar us_equity). Share classes: GOOGL/GOOG = one entity (CIK 1652044), two instruments. Entry: `xm_universe_sync {venue = "us_reference"}`.
- **No-Rust path:** none for bulk (10,428 SEC rows + 13k symdir lines). The architect may still do ad-hoc single lookups via http_request.
- **Evidence:** rg -n -i 'company_tickers|openfigi|figi|coingecko|cik\b|isin|cusip' src/ → only an egress unit-test host string (src/adapters/outbound/egress.rs:670); live probes SEC tickers + submissions, Nasdaq symdir, OpenFIGI mapping.
- **Absorbed `rh-id-map`** — Identifier map id_map: ticker, CIK, FIGI, ISIN, RH contract, Lighter market, HL symbol. Tool id_map with a refresh op. Files: src/adapters/outbound/tools/refdata/id_map.rs; pure join in src/domain/refdata.rs.
  
  Sources:
  - SEC https://www.sec.gov/files/company_tickers_exchange.json: 10,428 rows of cik, name, ticker, exchange. Needs a descriptive User-Agent; cache the file daily in the workspace.
  - OpenFIGI POST https://api.openfigi.com/v3/mapping: ISIN or TICKER to figi, compositeFIGI, shareClassFIGI. Keyless limit 25 req/60 s; optional $OPENFIGI_API_KEY.
  - RH /rhj/assets: isin, contract, uid.
  - lighter_rh_markets.
  - The HL/HIP-3 universe from the hl slice.
  
  Rows: id_map/1:<TICKER> (e.g. id_map/1:AAPL), TTL 86400 s. Data: {cik, name, exchange, isin, figi, composite_figi, share_class_figi, rh_contract, lighter_market_ids, hl_symbols}. Join on ISIN where present.
  
  The kg slice builds the entity graph on top of this.

### `kg-sync-crypto` — Crypto entity ids (CoinGecko) + HL crypto market mapping

M1 · S · rust · missing · outbound + application · PRD §4 §9 §19 §35 S1
After: `kg-catalog-store` (M1), `kg-seed-data` (M1), `kg-sync-hyperliquid` (M1)

- **Have:** nothing (CoinGecko appears only in agent prompts and an egress test)
- **Need:** src/adapters/outbound/xmarket/coingecko.rs: daily GET coins/list?include_platform=true (keyless cache 30 min; optional COINGECKO_DEMO_API_KEY, 100/min, 10k/mo, attribution). Produces entities token:<coingecko id> with platform contracts as ext ids.
  Mapping:
  - HL spot tokens by exact tokenId: CoinGecko platforms.hyperliquid, e.g. HYPE 0x0d01dc56dcaaca66ad901c959b4011ec
  - HL default-dex perps (BTC, ETH, SOL…; kPEPE = 1000 PEPE) through curated seeds for the top N, architect proposals for the rest
  - never map by symbol alone ('eth' matches ≥ 10 ids)
- **No-Rust path:** Partial: majors can be TOML seeds (done in kg-seed-data for P0). The long tail needs the sync.
- **Evidence:** rg -n -i coingecko src/ → only src/adapters/outbound/egress.rs:670 (test); live probe coins/list (21,701 ids, hyperliquid platform = HL tokenId).

### `hl-cex-universe-tool` — `cex_universe`: Binance / Bybit / OKX / Coinbase instruments incl. TradFi + pre-IPO perps

M1 · M · rust · missing · domain + outbound (tools) · PRD §4 §8 §10 §19 §35 S1
After: `hl-info-client` (M0), `hl-market-schema` (M0)

- **Have:** Nothing in src for binance, bybit or okx. Coinbase appears only as a jev-exec http_request URL (sandboxes/jev-exec/config.toml:124) and a test URL (src/application/decision_loop/mod.rs:1032).
- **Need:** domain src/domain/cex/{mod.rs,universe.rs} with decoders for: (1) Binance GET https://fapi.binance.com/fapi/v1/exchangeInfo: contractType PERPETUAL | TRADIFI_PERPETUAL; underlyingType COIN, EQUITY, KR_EQUITY, HK_EQUITY, CN_EQUITY, COMMODITY, INDEX, FX, PREMARKET. Also https://api.binance.com/api/v3/exchangeInfo. (2) Bybit https://api.bybit.com/v5/market/instruments-info?category=linear: symbolType stock, ETF, commodity, forex, innovation or empty; cursor paging. (3) OKX https://www.okx.com/api/v5/public/instruments?instType=SWAP: instCategory 1 crypto, 3 stocks, 4 commodities, 5 forex, 6 bonds. (4) Coinbase products. Tool cex_universe. Args: venue*, query. Output cex_universe/1:<venue> (TTL 3600 s) plus mkt_instrument/1:<venue>:<symbol> rows with category, quote_ccy and status. Flag ticker collisions: Bybit SPXUSDT and OKX SPX-USDT-SWAP are the SPX6900 memecoin (category crypto), not the S&P 500.
- **No-Rust path:** A skill + http_request works for the LLM. Lost: rows for kg matching, pagination, category normalisation.
- **Evidence:** rg -n -i 'binance|bybit|okx|coinbase|kraken' src: only the Coinbase test URL. Probes: Binance fapi exchangeInfo, Bybit instruments-info, OKX instruments.

### `kg-equivalence` — Cross-venue equivalence classes with ratios, relation `same` / `proxy`, quote currency, conflict flags

M1 · M · rust · missing · domain + application · PRD §4 §8 §19 §21 §35 S1
After: `kg-seed-data` (M1), `kg-sync-hyperliquid` (M1), `kg-sync-robinhood` (M1), `kg-sync-us-reference` (M1)

- **Have:** nothing
- **Need:** Pure src/domain/xmarket/equivalence.rs. Rules:
  - same ISIN/FIGI ⇒ same
  - HL description 'references 1 share of … (Nasdaq: TSLA)' ⇒ same as ref:XNAS:TSLA
  - RH ISIN ⇒ same
  - seeded commodity/index classes: hl:xyz:CL + hl:flx:OIL = commodity:wti_crude
  - `Primary oracle pricing reference: NYSE:USO` ⇒ proxy (hl:km:USOIL)
  Ratio per member (RH multiplier, HL k=1000, index points). Conflicts are flagged for review: two markets on one dex for one underlying (hl:xyz:SKHX vs hl:xyz:SKHY), or ISIN change.
  Applied by src/application/xmarket/equivalence.rs inside each sync transaction; projects `xm_instruments/1:<entity_id>`. Convergence strategies (§21) may only use `same` members.
- **No-Rust path:** Seed TOML equivalences (kg-seed-data) cover curated commodity/index/FX classes, but not the 300+ equity perps and tokens that churn weekly.
- **Evidence:** rg -n -i 'equivalen|canonical_id|underlying' src/ → no catalog code; probes show the same exposure under different ids (xyz:TSLA, flx:TSLA, km:TSLA, cash:TSLA, rh TSLA 0x322F0929c4625eD5bAd873c95208D54E1c003b2d) and proxies (km:USOIL).

### `kg-lifecycle` — §29 states, deterministic gates, demotion, audit, fail-closed reads; VALIDATED waits for M2 reference quotes (convention 10)

M1 · M · rust · missing · domain + application + outbound(tool) + config · PRD §28 §29 §32 §35 S1 §35 S7
After: `hl-info-client` (M0), `kg-catalog-store` (M1), `kg-domain-model` (M1), `ops-history-recorder` (M1), `rh-quote` (M2), `rh-ref-equities` (M2)

- **Tracker note:** Correction (critic + review): a terminal y/N prompt already exists for skill-evolve (`src/application/skills/lifecycle/approval_gate.rs`), but it reads one stdin line with no TTY check, so `tengu xm approve / suspend / resume` must add an `is_terminal()` refusal. Only Telegram `tool_approvals` / `approve_only` are unwired (`src/config/mod.rs:579-581`). Gate permission per convention 10: M1 = `instruments_allow` AND lifecycle ≥ MAPPED; VALIDATED needs reference quotes from `rh-quote` / `rh-ref-equities` (M2).
- **Have:** nothing. No approval gate exists anywhere: TelegramConfig.tool_approvals / approve_only are parsed but never read (src/config/mod.rs:579-581; docs/SESSION_HANDOFF.md:27).
- **Need:** Domain src/domain/xmarket/lifecycle.rs: states Discovered → Identified → Mapped → Observed → Validated → PaperTradable → LiveApproved (+ Suspended, Retired); pure evaluate(instrument, inputs, knobs, now) → transition + gate report.
  Gates:
  - DISCOVERED: seen by a sync or an unresolved mention.
  - IDENTIFIED: listed; kind, quote and leverage parsed.
  - MAPPED: one of (a) exact ext-id (ISIN/FIGI/CIK), (b) annotation pattern, (c) architect proposal + corroboration (return corr ≥ mapping_corr_min over ≥ mapping_min_bars during reference hours), (d) operator accept.
  - OBSERVED: fresh quotes for ≥ observed_min_hours with no gap > max_quote_gap_secs.
  - VALIDATED: oracle–reference divergence p95 ≤ divergence_p95_bps in open hours; spread at impact ≤ max_spread_bps; 24h notional ≥ min_notional_24h_usd; OI-cap headroom; ratio sanity.
  - PAPER_TRADABLE: VALIDATED + venue/kind allow-listed + no pending corporate action.
  - LIVE_APPROVED: operator only (kg-live-approval).
  Auto-demote on delist, halt, untradable, stale data, divergence breach, mapping conflict or pendingMultiplier. Any demotion clears LiveApproved.
  Knobs in `[xmarket.lifecycle]`: all required, no defaults (hedge-knob style), `deny_unknown_fields`.
  Application src/application/xmarket/lifecycle.rs: gather inputs → CAS transition → lifecycle_events row with actor.
  Tool `xm_lifecycle_eval {instrument_id | venue, commit = false}` projects `xm_lifecycle/1:<instrument_id>` and can never promote to LiveApproved.
  The risk gate reads CatalogStore::lifecycle(); unreadable ⇒ deny.
- **No-Rust path:** Partial: thresholds and allow-lists belong in TOML (doctrine #2). The states, transitions and audit must be enforced in code; an LLM or skill promoting states would break §29/§24.
- **Evidence:** rg -n -i 'DISCOVERED|IDENTIFIED|PAPER_TRADABLE|paper-tradable|LIVE_APPROVED|live-approved|lifecycle_state|asset_state|promote_asset' src/ → only unrelated DLMM 'discovered positions' (src/domain/lp/dlmm.rs:27); rg -n 'tool_approvals|approve_only' src/ → config/mod.rs:579-581 only.
- **Absorbed `risk-lifecycle-approvals`** — §29 lifecycle gate: PAPER-TRADABLE / LIVE-APPROVED approvals read by the gate. domain: `Lifecycle` enum discovered < identified < mapped < observed < validated < paper_tradable < live_approved, in src/domain/xm/universe.rs and shared with the catalog slice.
  outbound: approvals table in xm-paper.db (instrument full id, state, by, at_ms, reason). It is writable only by the operator CLI `tengu risk approve --instrument hyperliquid:xyz:TSLA --to paper_tradable|live_approved`, and by the automatic rule validated -> paper_tradable when mapping confidence is direct or strong ([risk] auto_paper = true). live_approved is operator-only. The gate reads max(catalog state, approval); a missing state denies. The approval ledger lives outside fs roots, so no agent can promote an instrument.
  Tests: ordering, missing state denies, live requires an operator approval.

### `kg-seed-data` — Curated seeds: non-listed entities, equivalence classes, relationship edges

M1 · S · toml · missing · sandbox · PRD §4 §5 §9 §10 §11 §35 S1
After: `kg-catalog-store` (M1)

- **Have:** nothing (sandboxes/*/ contain only config.toml and BENCH.md)
- **Need:** sandboxes/xmarket/seed/entities.toml:
  - commodities: commodity:wti_crude, commodity:brent_crude, commodity:gold, commodity:silver, commodity:copper, commodity:natgas
  - index:sp500, index:nasdaq100, index:russell2000; fx:eurusd, fx:usdjpy; rate:ust10y; sector:semiconductors
  - company:anthropic (listing private, lei 984500B6DEB8CEBC4Z70, wikidata Q116758847), company:openai, company:spacex
  - crypto majors token:bitcoin, token:ethereum, token:solana, token:hyperliquid
  equivalence.toml:
  - hl:xyz:CL + hl:flx:OIL same commodity:wti_crude; hl:xyz:BRENTOIL same commodity:brent_crude
  - hl:km:USOIL proxy via ref:ARCX:USO
  - hl:xyz:SP500 same index:sp500 (others after annotation check)
  - hl:io:ANTH preipo_perp → company:anthropic
  edges.toml, each with an evidence URL: company:anthropic ← investor company:amazon, company:alphabet (Wikidata P1951); semis supplier/customer seeds.
  Commodity/index/FX entity rows are what let an 'oil' event (§10) resolve in P0.
- **No-Rust path:** This IS the no-Rust path: curated data, reviewed through git. The loader is in kg-xm-cli; format validation runs at load time.
- **Evidence:** ls sandboxes/*/ → no seed dirs; perpAnnotation probes show commodity/index markets without ISINs (flx:OIL WTI 1 bbl, km:USOIL → NYSE:USO, cash:WTI unannotated), so curated classes are required.

### `kg-calendars` — One session evaluator: NYSE holidays / early closes, trade[XYZ] windows, RH mint window, 24/5, 24/7

M1 · S · rust · missing · domain + config · PRD §2 §19 §20 §21 §35 S1
After: `kg-domain-model` (M1)

- **Have:** nothing
- **Need:** Pure src/domain/xmarket/calendar.rs: session_state(calendar, ts) → open | pre | post | overnight | closed, with half-days. Calendar data in `[xmarket.calendars.us_equity]` TOML: tz America/New_York, core 09:30–16:00, holidays verified on nyse.com — 2026: 01-01, 01-19, 02-16, 04-03, 05-25, 06-19, 07-03, 09-07, 11-26, 12-25; 2027: 01-01, 01-18, 02-15, 03-26, 05-31, 06-18, 07-05, 09-06, 11-25, 12-24; early closes 13:00. Other calendars: `24x7` (HL perps) and `rh_session` (RH tradingCapabilities + tokenization window). Instruments carry calendar_id + reference_calendar_id. xm_find_instruments / xm_confirm_reaction expose reference_open so weekend HL moves against a closed reference are never read as under- or over-reaction.
- **No-Rust path:** Partial: holiday lists and hours are TOML data (doctrine #2); session evaluation is code.
- **Evidence:** rg -n -i 'holiday|trading_hours|market_hours|calendar_id|session_state|market_open|is_open\b' src/ → 0; fetched https://www.nyse.com/markets/hours-calendars (2026 + 2027 holidays).
- **Absorbed `hl-session-calendar`** — Market-session calendar giving a session feature (external | internal | unknown) for HIP-3 and TradFi perps. (1) domain src/domain/market_session.rs, pure, now_ms as input. Weekly windows in America/New_York with a hand-rolled US DST rule (2nd Sunday in March to 1st Sunday in November), fixed-offset Asia/Seoul and Asia/Tokyo, holiday dates, daily gaps. (2) config src/config/market_session.rs: [market_sessions.<name>] with windows, tz and holidays, plus [market_sessions.map]. Mapping: xyz:stocks -> us_equity_24x5; xyz:indices and xyz:commodities -> cme_23x5; xyz:fx -> fx_24x5; xyz:SMSN, xyz:SKHX, xyz:HYUNDAI, xyz:KR200 -> krx; xyz:JP225, xyz:KIOXIA, xyz:SOFTBANK -> jpx; para, mkts and io unmapped => unknown. (3) Seed data from trade[XYZ]: US stocks use the external price Sun 20:00 -> Fri 20:00 ET (pre-market 04:00-09:30, regular 09:30-16:00, post 16:00-20:00); US indices and commodities Sun 18:00 -> Fri 17:00 ET with a daily 17:00-18:00 gap; FX Sun 17:00 -> Fri 17:00 ET. When closed, the oracle is an internal EWMA (tau 30 min, at most ~9.5 %/day) and the mark is bounded to +-1/maxLeverage (discovery bounds with per-asset resets). (4) Features: session, session_change_in_s, bounds_pct = 1/max_leverage.
- **Absorbed `rh-market-hours`** — Deterministic session calendar market_session (US equities, RH mint window, 24/5 feeds, 24/7 venues). domain: src/domain/market_hours.rs, pure, with now_ms as input.
  - US equities sessions (ET):
    - overnight 20:00-04:00 (Sun-Thu nights)
    - pre-market 04:00-09:30
    - regular 09:30-16:00
    - post-market 16:00-20:00
  - Holidays and early closes (13:00 ET) come from config.
  - Hand-rolled DST: US 2nd Sun Mar to 1st Sun Nov; EU last Sun Mar to last Sun Oct. Tests cover the 2026-2028 transitions.
  - RH tokenization window: Mon 02:00 to Sat 02:00 Europe/Paris.
  - Venue table:
    - us_equities
    - rh_tokenization
    - chainlink_rh (24/5)
    - crypto (24/7)
    - hip3 (24/7; details from the hl slice)
  
  config: src/config/market_calendar.rs, deny_unknown_fields. [market_calendar.us_equities] holidays = ['2026-11-26', ...], early_closes = ['2026-11-27', ...], taken from the NYSE-published 2026-2028 list.
  
  tool: market_session (// scope: pure-compute).
  - Key market_session/1:<venue> (e.g. market_session/1:us_equities), TTL 60 s.
  - Features: session, is_open, is_holiday, early_close, secs_to_next_change, mint_window_open.
  - Read by rh_quote, rh_basis and risk gates.

### `kg-sync-schedule` — Syncs + lifecycle evaluation as `[feeds.*] kind = "tool"` rows (no host timers)

M1 · S · toml · missing · infra · PRD §4 §19 §35 S1 §35 S2
After: `kg-xm-cli` (M1)

- **Tracker note:** Implemented as `[feeds.*] kind = "tool"` rows on `rt-scheduler`, not host timers.
- **Have:** No scheduler or interval poller (rg); triggers are only webhooks and `tengu decide` (src/adapters/inbound/webhooks.rs:246-275; cli/decide.rs).
- **Need:** Interim: deploy/xmarket-sync.sh (infra) + launchd/cron entries calling `tengu xm sync --venue <v>`:
  - hyperliquid every 15 min
  - robinhood every 60 min
  - us_reference daily after the symdir refresh
  - coingecko daily
  - lifecycle eval every 5 min
  Staleness is visible via `xm_sync/1:<venue>` rows; trade-related loop actions use `requires = { hl_sync = 3600 }`. Later, move to the rt slice's in-process scheduler (rt-scheduler).
- **No-Rust path:** Yes: cron + CLI, no Rust beyond kg-xm-cli. Trade-off: no in-process backoff/jitter, and the store is reopened each run (cheap).
- **Evidence:** rg -n -i 'tokio::time::interval|interval\(|cron|scheduler|every_secs|poll_secs|tick_secs' src/ → none (only the tool_loop cancel poll, application/chat/tool_loop.rs:218); Cargo.toml has no cron/websocket/tungstenite/eventsource/tonic/grpc deps.

### `kg-xm-cli` — `tengu xm sync / seed / status / show / backup` (no LLM)

M1 · S · rust · missing · inbound + bootstrap · PRD §19 §29 §32 §35 S1
After: `kg-catalog-store` (M1), `kg-sync-hyperliquid` (M1)

- **Have:** The CLI has chat/status/doctor/telegram/webhooks/decide/eval/secret/prune/mcp-bridge/agentic-memory-server/skill/run-agent and nothing catalog-related (src/adapters/inbound/cli/mod.rs:37-162).
- **Need:** New `Commands::Xm` variant + src/adapters/inbound/cli/xm.rs:
  - `tengu xm sync --sandbox xmarket [--venue hyperliquid|robinhood|us_reference|coingecko|all]`
  - `tengu xm seed` (loads sandboxes/xmarket/seed/*.toml, idempotent, provenance seed)
  - `tengu xm status` (per venue: counts by lifecycle state, last sync, errors)
  - `tengu xm show <entity_id|instrument_id>` (full ids, class members + ratios, edges + provenance, lifecycle history)
  - `tengu xm backup` (VACUUM INTO <TENGU_HOME>/state/backups/xmarket-<ts>.db)
  JSON output on stdout; exit non-zero on sync failure.
- **No-Rust path:** Partial: once xm_universe_sync exists, a decision loop of read_only actions triggered by `tengu decide` can sync. Trade-off: one Jev call per run for a fixed sequence, and nondeterministic ordering.
- **Evidence:** read src/adapters/inbound/cli/mod.rs:37-162 (Commands enum); rg -n -i 'catalog|universe' src/adapters/inbound/ → 0.

### `rh-evm-rpc` — EVM JSON-RPC read transport (`eth_call`, `eth_getLogs`, blocks, Multicall3) + `[evm.chains]`; route the existing receipt poll through it

M1 · M · rust · missing · domain+outbound+config+bootstrap · PRD §1 §19 §20 §35 S1 §35 S2

- **Tracker note:** Correction (critic): alloy 1.7.3 accepts an injected reqwest client (`ProviderBuilder::connect_reqwest`, `Http::with_client`), so it can ride the egress client; what it loses is the per-call `check_url` / `net_hosts` check and the audit record, and WS has no SOCKS path. The recommendation (own transport) stands (convention 13). Also route the existing `wait_for_receipt` poll (`src/adapters/outbound/tools/crypto/helpers.rs:155-185`, no scope check or audit) through it.
- **Have:** The only EVM network call is wait_for_receipt (src/adapters/outbound/tools/crypto/helpers.rs:155-185): eth_getTransactionReceipt on $EVM_RPC_URL with no check_url or net_hosts check. alloy (Cargo.toml:43) is used only for U256 and dyn-abi (src/domain/solana.rs:12, crypto/helpers.rs:6). A reusable Solana transport exists: src/adapters/outbound/solana/rpc.rs:325-446 (RpcTransport and HttpTransport: gate, audit, Scrubber) and :458-557 (SolanaRpc::call with one retry).
- **Need:** outbound:
  - Move RpcTransport, HttpTransport, Scrubber and RpcError from solana/rpc.rs into src/adapters/outbound/jsonrpc.rs, with the audit label and URL env var as parameters. SolanaRpc keeps its API.
  - New src/adapters/outbound/evm/{mod,rpc,logs,multicall}.rs with EvmRpc::from_ctx(ctx, chain):
    - URL = $ROBINHOOD_RPC_URL when env_reads allows it, else https://rpc.mainnet.chain.robinhood.com; render the host only.
    - chain_id() asserted = 4663 on first use; block_number(); block(n) returning timestamp and head_age_s; call(to, data, block).
    - get_logs(filter) chunked at 10,000,000 blocks, bisecting on -32000 'exceeds limit of 10000'.
    - multicall3(calls) = aggregate3 on 0xcA11bde05977b3631167028862bE2a173976CA11.
    - EVM error classes: -32000 revert/limit, -32602 range, HTTP 429 = rate_limited.
  
  domain: src/domain/evm/{mod,abi}.rs with alloy sol! ABIs for ERC-20, ERC-8056 (uiMultiplier / newUIMultiplier / effectiveAt / oraclePaused), AggregatorV3, UniswapV3Factory, QuoterV2, V4Quoter, StateView, Multicall3 and the StockFactory Deployed event.
  
  config: src/config/evm.rs [evm.chains.robinhood], deny_unknown_fields.
  - Keys: chain_id, rpc_env, default_rpc, and a contract book (stock_factory, usdg, weth, v3_factory, quoter_v2, v4_pool_manager, v4_quoter, state_view, multicall3).
  - Folded into AgentConfig the same way as signer_key_file (src/config/mod.rs:373-377, 1024-1028).
  
  doctor: tengu doctor --sandbox xmarket checks eth_chainId = 0x1237 and eth_getCode != 0x for every address in the contract book.
  
  Rules:
  - Never use alloy::providers (it bypasses egress).
  - Tests use a fake transport replaying the 2026-09-29 probe replies.
- **No-Rust path:** Partial only.
  (a) http_request POST JSON-RPC plus abi_encode and hex_to_uint256 works for single flat calls by the architect, as text. abi_encode splits the signature on every comma (crypto/helpers.rs:207-212), so tuple args (QuoterV2, aggregate3) fail.
  (b) An external EVM MCP server via [[mcp_servers]] (e.g. JamesANZ/evm-mcp with CUSTOM_NETWORKS chainId 4663) needs no Rust. It returns untyped hex, has no cache, world or requires, and its stdio egress is only advisory.
  Good enough for exploration, not for the §25 deterministic layer.
- **Evidence:** - rg -n 'alloy::(providers|rpc|transports|pubsub|contract|sol_types)|ProviderBuilder|eth_call|eth_getLogs|eth_blockNumber|eth_getBlockByNumber|EVM_RPC_URL|EvmRpc|evm_rpc|evm_call' src tests: only crypto/helpers.rs:160.
  - rg -n 'alloy::' src: domain/lp/dlmm.rs:35, domain/solana.rs:12, tools/crypto/*.
  - rg -n -i 'chainlink|latestRoundData|multicall|aggregate3' src: none.
  - Read crypto/{mod,helpers,sign_tx}.rs, solana/rpc.rs, Cargo.toml, Cargo.lock.
  - Live: eth_chainId 0x1237; getLogs span and log caps verified.

### `ops-history-recorder` — `HistoryStore` + SQLite day files under `state/xmarket/history/` (snapshots, depth, funding, OI, events, universe)

M1 · L · rust · missing · ports | application | outbound | config | inbound · PRD §20 §33 §34 §35 S2 §35 S8
After: `hl-book-tool` (M0), `hl-ctx-tool` (M0), `info-store` (M4), `rh-dex-quote-v3` (M2), `rh-quote` (M2), `rt-scheduler` (M0)

- **Tracker note:** Path is `<TENGU_HOME>/state/xmarket/history/<YYYYMMDD>.db` (conventions 3, 5).
- **Have:** The observation store keeps one row per key (`key TEXT PRIMARY KEY`, src/adapters/outbound/observations.rs:23-27). Upserts overwrite (31-36), rows older than 7 days are purged on open (21, 66-69) and Error rows are never stored (163-165). observe() stores only usable rows with ttl > 0 (src/application/observe.rs:44). The only history-like data is the ≤5-minute price-sample ring inside a price_oracle row (src/domain/lp/market.rs:255, 386).
- **Need:** ports/history.rs: HistoryStore { append(&[Observation]), range(key, from_ms, to_ms), asof(keys, t_ms, max_age_ms) }. ports/observation.rs: ObservationStore::record(&Observation), a default no-op that observe() calls for every live result, including Error and ttl-0 rows. adapters/outbound/history_sqlite.rs: day files <TENGU_HOME>/state/history/<sandbox>/<YYYYMMDD>.db (WAL + busy_timeout as in observations.rs:64), table obs_history(key, schema, observed_at_ms, venue_ts_ms, slot, source, status, features JSON, data JSON NULL), index (key, observed_at_ms). A RecordingObservationStore decorator plus one open_observation_store(workspace, &RecorderConfig) constructor used by every tool plugin and by bootstrap/decision.rs:76. config/recorder.rs: [recorder] enabled, schemas = [...], keep_data = ['l2/1'], change_only = true, min_interval_secs = {}, retention_days = 30 (a sweeper deletes old day files). Conventions: schemas carry a venue_ts_ms feature when the venue stamps data; universe/catalog rows are recorded too, to avoid survivorship bias. CLI `tengu history range <key> --from --to`. Volume: Hyperliquid 529 markets (234 main + 295 HIP-3, probed) plus RH tokens and reference instruments ≈ 900. The HL REST budget of 1200 weight/min at weight 20 per ctx call allows one ctx call per dex every 15–30 s, i.e. 2.7–5.4M rows/day ≈ 0.9–1.9 GB/day in SQLite (~350 B/row) vs ~80–160 MB/day columnar. L2 top-10 for ≤20 hot instruments at 1 s ≈ 1.7M rows ≈ 2.2 GB/day with data, so the default records depth features only.
- **No-Rust path:** none for recording (store semantics are code). For querying, the sqlite3 or DuckDB CLI outside the repo can read the day files.
- **Evidence:** rg -n -i 'time_series|timeseries|history\.db|recorder|backtest|ohlc|candle|append_only' src → none relevant; rg -n -i 'trait HistoryStore|obs_history|asof|as_of' src → none; read observations.rs:1-73, observe.rs:16-51; HL probes (metaAndAssetCtxs 72,110 B / 234 markets; perpDexs 10 dexes / 295 markets); HL rate-limit doc
- **Absorbed `risk-series-store`** — Time-series sample store for rolling windows: <workspace>/.tengu/series.db. domain: src/domain/series.rs with Sample {t_ms, src_t_ms (venue time), fields: BTreeMap<String, f64>}, buckets, windows.
  ports: src/ports/series.rs `SeriesStore` with append, range(from, to), last_before(t), purge.
  outbound: src/adapters/outbound/series.rs `SqliteSeriesStore` at <workspace>/.tengu/series.db. Table samples(series TEXT, t_ms INTEGER, src_t_ms INTEGER, body TEXT, PRIMARY KEY(series, t_ms)), WAL. Retention per class: raw 1 s buckets for 48 h, 1 min rollups for 30 d.
  application: observe() (src/application/observe.rs:39-50) appends a sample after a live put when the Observed type implements an optional fn sample(&self) -> Option<Sample>. Stream writers (rt slice) append directly. A backfill API accepts HL candleSnapshot (1m works for HIP-3, probe).
  Series keys mirror observation keys, e.g. hl_ctx/1:xyz:TSLA with fields {mark, oracle, mid, oi, funding, day_ntl_vlm}.
  Tests: tempdir append / range / bucket / purge; observe() hook test with MemStore.

### `hl-tor-probe` — Probe every xmarket host over Arti exits and from the deployment host's own network (Kazakh connections cannot reach Coinbase, OKX and 1,100+ other platforms); per-host table so `network = "tor"` stays a one-line switch

M1 · S · research · missing · infra · PRD §20 §35 S1

- **Tracker note:** Probe from Arti exits AND from the deployment host's own network: Kazakh connections (the operator's location) cannot reach Coinbase, OKX and 1,100+ other platforms, and the research probes ran from a DE IP. The per-host table keeps `network = "tor"` a one-line switch (convention 17). (The CLAUDE.md / AGENTS.md egress gotcha was corrected 2026-09-30.)
- **Have:** Tor is the egress default and 'tengu doctor --tor' checks the exit (src/adapters/outbound/egress.rs:1-30). No market host has been probed. This session could not probe: 127.0.0.1:9050 closed, no tor/arti binary. lping and jev-exec run network = open because APIs block Tor exits (sandboxes/lping/config.toml:22-26, sandboxes/jev-exec/config.toml:22-24).
- **Need:** Run 'make tor'. Then, for several exits: curl --socks5-hostname 127.0.0.1:9050 -X POST https://api.hyperliquid.xyz/info with body {type: exchangeStatus}; websocat --socks5 127.0.0.1:9050 wss://api.hyperliquid.xyz/ws with an allDexsAssetCtxs subscription; the same for fapi.binance.com, api.bybit.com, www.okx.com, api.exchange.coinbase.com and api.international.coinbase.com. Record 200/403/451/429 and latency per exit in docs/egress-2026-09-16.md, then set [egress] network in sandboxes/xmarket/config.toml. Optional: a 'tengu doctor --tor --probe <url>' flag (src/adapters/inbound/cli/doctor.rs).
- **No-Rust path:** Yes: shell + docs. The doctor flag is optional Rust.
- **Evidence:** nc -z -w 2 127.0.0.1 9050: closed. 'which tor arti torsocks': not found. deploy/tor exists (Docker: arti.toml, compose.yml) but is not running.

### `hl-skill` — `skills/hyperliquid/SKILL.md`: info API over `http_request` for the architect

M1 · S · skill · missing · skill · PRD §4 §8 §19 §26 §35 S1

- **Have:** No market-data skill (skills/ has orchestrator, privy-agentic-wallets, …). http_request POST is ready (src/adapters/outbound/tools/http/request.rs:28-78,280-283).
- **Need:** skills/hyperliquid/SKILL.md with frontmatter (name, description). Content: (1) request bodies for perpDexs, allPerpMetas, metaAndAssetCtxs (dex), spotMetaAndAssetCtxs, perpConciseAnnotations, perpAnnotation, l2Book, candleSnapshot, fundingHistory, predictedFundings, perpsAtOpenInterestCap, outcomeMeta; (2) naming: perps dex:COIN, spot @N, outcome sides #N; (3) request weights; (4) pitfalls: STX = Stacks but para:STX = Seagate; SPX = SPX6900; SKHY = 1/10 share; '500 null' = unknown dex/coin; the dexes flx, vntl, hyna, km, abcd and cash are fully delisted. Agent scope: http_request = { net_hosts = [api.hyperliquid.xyz] }.
- **No-Rust path:** This is the no-Rust path (doctrine #2). It works today for LLM research, but not for Jev world/requires.
- **Evidence:** ls skills: no hyperliquid or market skill. rg -n -i hyperliquid skills: none.

### `rh-skill` — `skills/robinhood-chain/SKILL.md`: venue knowledge for the architect and planner

M1 · S · skill · missing · skill · PRD §8 §19 §20 §26

- **Have:** Nothing. The only related content is skills/privy-agentic-wallets, which has just an Arbitrum chain-id table.
- **Need:** Documentation skill with frontmatter name / description. It covers:
  - Canonical-address rule: API ∩ StockFactory. A same-name token at another address is not a Robinhood token.
  - The four price surfaces (reference, token-equivalent, oracle, executable), with the AAPL example.
  - Multiplier math.
  - Sessions: 24/7 on-chain, 24/5 oracle, mint window Mon 02:00 to Sat 02:00 Europe/Paris.
  - Venues: Uniswap v3/v4; Lighter RH, including the ANTHROPIC / OPENAI perps; RFQ via 0x / LI.FI; Rialto propAMM.
  - Eligibility: no US persons; CA / UK / CH restricted.
  - http_request recipes for /rhj/* and Lighter, including a User-Agent header for Blockscout and SEC.
  
  Also evals/prompts.yaml with 3 cases.
- **No-Rust path:** This is itself the no-Rust path (doctrine #2).
- **Evidence:** rg -n -i 'robinhood|stock.?token' skills: none.

### `kg-docs` — Catalog / graph / lifecycle doc + code map, typed-observations, egress, tools

M1 · S · docs · missing · n/a · PRD §19 §29 §32
After: `kg-catalog-store` (M1)

- **Have:** No catalog docs. The PRD links a tracker (docs/xmarket-tracker-2026-09-29.md) that does not exist yet.
- **Need:** docs/xmarket-catalog-2026-xx.md (one screen: ids, tables, lifecycle gate table, sources + limits). Update:
  - docs/code-map.{md,html} (new files; tests/code_map.rs enforces)
  - docs/typed-observations-2026-09-24.md (xm_* schemas, keys, TTLs)
  - docs/egress-2026-09-16.md (new hosts, UA requirement)
  - docs/tools.md, SESSION_HANDOFF.md
  - CLAUDE.md + AGENTS.md gotchas: catalog DB outside fs roots; LIVE_APPROVED only via CLI
- **No-Rust path:** n/a (docs)
- **Evidence:** ls docs | rg -i 'xmarket|tracker' → only xmarket-prd-2026-09-29.md; CLAUDE.md 'REQUIRED updates' table.

### `rh-docs` — Robinhood Chain operator doc + typed-observations / egress / code map

M1 · S · docs · missing · n/a · PRD §32 §35 S1
After: `kg-calendars` (M1), `kg-sync-robinhood` (M1), `rh-dex-quote-v3` (M2), `rh-evm-rpc` (M1), `rh-quote` (M2)

- **Have:** Nothing for RH; docs/typed-observations-2026-09-24.md covers Solana only.
- **Need:** New doc: one screen of tables covering hosts, env vars, keys, TTLs, observation keys, the contract book and the session table.
  
  Update:
  - docs/typed-observations (new schemas)
  - docs/egress-2026-09-16.md (new hosts)
  - docs/code-map.md and .html (new files)
  - docs/tools.md
  - config.example.toml ([evm.chains], [market_calendar], [market_data])
- **No-Rust path:** n/a (docs)
- **Evidence:** rg -n -i 'robinhood|stock.?token' over the repo (PRD excluded): no docs hits.


## M2 — continuous market observation

### `rt-bus-dispatch` — Event bus + per-loop bounded queues (topics, drop / coalesce, max age, in-flight cap, priority); both discovery paths concurrent

M2 · M · rust · missing · domain | application | config · PRD §3 §13 §14 §27 §35 S2
After: `rt-daemon` (M0)

- **Have:** Each webhook POST spawns `dl.handle_event` (webhooks.rs:257-264). handle_event holds the loop's tokio Mutex for the whole event incl. Jev and tool calls (src/application/decision_loop/mod.rs:67, 104-122; doc :27-28), so events wait in an unbounded FIFO with no age check, drop, coalescing or cross-loop cap. Worst case per event = Jev timeout 20 s x max_steps 4 + tool time (src/config/decision_loop.rs:59-75). The existing buses serve UI/telemetry only: OrchestratorEvent broadcast (src/application/orchestrator/events.rs:78-86), metrics broadcast (src/application/metrics.rs:22).
- **Need:** domain: src/domain/feed.rs `BusEvent { topic, subject, lane, dedup_key, correlation, trace, payload }`, `DropPolicy { Oldest, Newest, CoalesceBySubject }`. application: src/application/runtime/bus.rs (topic -> bounded mpsc per consumer, drop counters) and src/application/runtime/dispatch.rs (one worker per loop; skips events older than max_event_age_secs with audit `skipped_stale`; global tokio Semaphore [runtime] max_decisions_in_flight; per-loop decisions_per_min). config on [decision_loops.<n>]: input = ['market.anomaly'], queue = 32, drop = 'coalesce', max_event_age_secs = 30. Topics: market.raw, market.anomaly, info.raw, info.event, loop.<name>, escalation.result. Under `tengu run`, webhook endpoints with `loop` publish to loop.<name> instead of spawning.
- **No-Rust path:** None. The webhook path cannot bound or coalesce events; TOML can only lower max_steps / timeout_secs.
- **Evidence:** rg 'broadcast::channel|mpsc::channel|mpsc::unbounded_channel|watch::channel|EventBus|event_bus|FeedSource|feed_source|EventSink|Semaphore' src -> orchestrator/metrics buses and TUI/telegram/claude_code channels only; rg 'Semaphore' src -> 0; read decision_loop/mod.rs, webhooks.rs.
- **Absorbed `jev-loop-concurrency`** — Parallel events per loop with bounded queue, coalescing and priority. config: `concurrency = 4`, `queue_max = 200`, `coalesce = true` (keep the newest per event_key), `priority_path = "/priority"`. application: per-event_key LoopState (needs event-scoped history) behind a semaphore, plus a bounded priority queue; drops and coalesces get an audit line.
- **Absorbed `jev-loop-dispatch`** — In-process loop registry: feed / tick / hand-off / resume dispatch. ports/decision.rs: LoopDispatch {enqueue(loop, event, event_key?, session_id?, priority)}. bootstrap/decision.rs: LoopRegistry (every [decision_loops.*], one DecisionLoop each), used by the listener, the ticker, ArchitectEscalator (resume) and in-process feed runners (rt / info). config: action flag `handoff = "opportunity"` forwards the event with its event_key and returns outcome HandedOff.

### `rt-dedup-state` — Persistent ingest seen-set + feed cursors + webhook replay protection in `runtime.db`

M2 · M · rust · missing · domain | ports | outbound | config · PRD §17 §18 §35 S3
After: `rt-bus-dispatch` (M2)

- **Tracker note:** Moved to M2 (the M2 exit asserts restart dedup). State lives in `<TENGU_HOME>/state/xmarket/runtime.db`, not `<workspace>/.tengu/feeds.db` (convention 3).
- **Have:** No event-level dedup. The observation upsert is last-writer-wins per key (observations.rs:29-36), which dedups rows, not events. Webhooks have no replay protection or idempotency (docs/webhooks-2026-05-11.md:291), and Helius may deliver duplicates (docs/lping-2026-09-24.md:22).
- **Need:** domain: src/domain/feed.rs `dedup_key(item, rule)` — an id JSON pointer, else sha256 (sha2 in tree) of normalized text (lowercase, collapse whitespace, strip URL query/fragment, strip an `RT @x:` prefix). ports: src/ports/feed_state.rs `FeedStateStore { seen_or_insert(key, ttl_ms) -> bool, cursor(feed), set_cursor(feed, {since_id, etag, last_modified, last_ts}), purge }`. outbound: src/adapters/outbound/feed_state.rs — SQLite <workspace>/.tengu/feeds.db (WAL + busy_timeout like observations.rs:59-73) behind an in-memory LRU. config: `[feeds.<n>] id = '/id'`, `dedup_ttl_secs = 86400`; `[webhooks.endpoints.<n>] dedup = '/0/signature'` -> a duplicate POST answers 200 {status: duplicate}. Semantic / canonical-event merging (§17 near-duplicates, confirmations) stays in info/kg.
- **No-Rust path:** None. The listener has no idempotency and loops keep no memory of seen ids.
- **Evidence:** rg -i 'dedup|idempot|seen_ids|seen_set|replay|nonce|content_hash|event_id' src -> unrelated only (solana_tx.rs:435 ATA CreateIdempotent, lp/snapshot.rs:3315 sort dedup, secrets.rs AES nonce); read docs/webhooks-2026-05-11.md.

### `rt-ws-client` — WebSocket feeds through egress (SOCKS5h / CONNECT / direct): subscribe, heartbeat, reconnect + resubscribe

M2 · L · rust · missing · outbound | application | config · PRD §4 §20 §27 §35 S2
After: `hl-book-tool` (M0), `hl-ctx-tool` (M0), `rt-backoff-budget` (M0), `rt-daemon` (M0), `rt-obs-batch-writer` (M2)

- **Have:** No WS code in src. tokio-tungstenite 0.26.2 (rustls + webpki-roots) is compiled but unused, pulled by alloy full -> alloy-transport-ws 1.7.3 (Cargo.toml:43; Cargo.lock:803-816, 5885-5897). SOCKS5 exists only inside reqwest via hyper-util 0.1.20 (`client::legacy::connect::proxy::SocksV5`, remote DNS by default; Cargo.lock:3055, 4605-4650); no tokio-socks. Egress refuses non-http(s) schemes (`check_url`, egress.rs:297-311), applies the proxy only to reqwest builders (`apply_proxy`, :313-329), and audits with a synchronous append per record (:466-492).
- **Need:** egress (src/adapters/outbound/egress.rs): `check_ws_url` (wss; ws only when !https_only; allow/deny hosts) and `ws_connect(url, headers)`. Open network -> tokio_tungstenite::connect_async_tls_with_config. socks5h:// -> hyper_util SocksV5::new(proxy, HttpConnector) (remote DNS) -> `.into_inner()` TcpStream -> tokio_tungstenite::client_async_tls_with_config. http:// proxy -> hyper_util proxy::Tunnel. Connection-level audit (tool = 'ws', host, path, verdict, connect_ms, msgs, bytes, close), never per message. Cargo.toml direct deps, no new lockfile crates: tokio-tungstenite = { version = '0.26', default-features = false, features = ['connect', 'rustls-tls-webpki-roots'] }, hyper-util = { version = '0.1', features = ['client-legacy', 'tokio'] }. outbound: src/adapters/outbound/feeds/ws.rs — subscribe messages; app heartbeat (send {method: ping} every 30 s, stale after 60 s; HL closes connections idle 60 s); reconnect via rt-backoff-budget with a >= 2 s floor (HL: 30 new connections/min/IP); resubscribe; snapshot-on-subscribe dedup; subscription set reconciled every 5 s from `watch/1:<venue>:<coin>` rows (cap max_subscriptions <= 1000/IP); message -> declarative obs mapping or a named decoder from the hl slice; coalesced writes (rt-obs-batch-writer); `feed/1:<name>` health row. config: [feeds.<n>] kind = 'ws', url, subscribe = [..], watch_schema, heartbeat = {send, every_secs, stale_secs}, max_subscriptions.
- **No-Rust path:** An external bridge (websocat | curl HMAC POST per message into `tengu webhooks`): per-message HTTP + Jev dispatch, Tor and scopes not enforced on the external process, no typed rows; MCP cannot push. REST polling of /info is the pragmatic P0 substitute (0.35-0.59 s per call within 1200 weight/min/IP).
- **Evidence:** rg -i 'tungstenite|websocket|web_socket|wss://|ws://|WsConnect|connect_ws|ProviderBuilder' src -> 0 hits; Cargo.lock entries above; registry reads: tokio-tungstenite-0.26.2/src/lib.rs:55 (client_async_tls_with_config), hyper-util-0.1.20/src/client/legacy/connect/proxy/socks/v5/mod.rs:24, 105 (local_dns false), 243-252; connect/http.rs:462 (Response = TokioIo<TcpStream>).

### `rt-obs-batch-writer` — Batched store writes + change feed (`kind = "rows"`): execution → observation → next decision

M2 · S · rust · partial · ports | outbound | application | config · PRD §20 §30 §35 S2
After: `rt-bus-dispatch` (M2)

- **Have:** `put` runs one statement per call on spawn_blocking behind one mutex (src/adapters/outbound/observations.rs:75-89, 162-175). Index observations_schema(schema, observed_at_ms) exists (:27) but only the purge queries by time (:67). Rows written by run-agent children or tools stay invisible to loops until some event arrives.
- **Need:** ports (src/ports/observation.rs): `put_many(&[Observation])` (one transaction, same UPSERT_SQL :31-36) and `changed_since(schema, since_ms, limit)` (uses the existing index); implement both in observations.rs. application: src/application/runtime/writer.rs — coalescing writer (latest per key, flush every 250 ms, at most 1 write/s/key) used by ws/poll feeds. config: `[feeds.<n>] kind = 'rows', schema = 'paper_fill/1', every_ms = 500, topic = 'loop.market_first'` turns new rows (paper fills, architect results) into bus events — §30 closure across processes.
- **No-Rust path:** None for batching. A tick loop re-reading `world` approximates the change feed at one Jev call per tick.
- **Evidence:** read observations.rs (SCHEMA_SQL :23-27, UPSERT_SQL :31-36, with_conn :75-89); rg 'observed_at_ms (<|>)|observations_schema|WHERE schema' observations.rs -> index definition + purge only.

### `rt-series-detectors` — Deterministic anomaly detectors over series ⇒ `market.anomaly` events (market-first path)

M2 · L · rust · missing · domain | ports | outbound | application | config · PRD §3 §14 §25 §35 S2
After: `hl-book-tool` (M0), `hl-ctx-tool` (M0), `kg-equivalence` (M1), `rt-bus-dispatch` (M2), `rt-scheduler` (M0)

- **Have:** The store keeps only the latest row per key (PRIMARY KEY key, src/adapters/outbound/observations.rs:23-27; 7-day purge :20-21, 66-69), so there are no series. Only precedent: price_oracle rows carry a thinned <= 6-min sample ring + move_5m_pct for the LP storm gate (src/domain/lp/market.rs:55-58, 252-258, 386-399). No generic detector, baseline or z-score.
- **Need:** domain: src/domain/detect.rs (pure). Rules: PctChange{window_secs, min_abs_pct}; Delta{window_secs, min_abs|min_pct} (OI +25 %, funding jump); RatioToBaseline{window_secs, baseline_secs, min_ratio} (volume 8x); ZScore{window_secs, baseline_secs, min_z}; Spread{a_key, b_key, min_bps} (cross-venue divergence, static pairs). Hysteresis + cooldown_secs; `evaluate(points, rule, now) -> Option<Anomaly>`. ports/outbound: `ObservationStore::append_series(key, feature, t_ms, v)` / `series_window(key, feature, since_ms)` backed by table series(key, feature, t_ms, v) in observations.db. Series are written only for (schema, feature) pairs some detector references, thinned to 1 point per 5 s (windows <= 1 h) and 1 per 60 s beyond, 7-day retention. application: src/application/runtime/detect.rs — on each row written by a feed, run matching detectors -> row `anomaly/1:<detector>:<subject>` + a bus event on market.anomaly {subject, features, window stats}. config: `[detectors.<n>] schema = 'hl_ctx/1', feature = 'open_interest', rule = 'delta_pct', window_secs = 300, min = 25, cooldown_secs = 600, topic = 'market.anomaly'`.
- **No-Rust path:** Jev as the detector on a tick loop reading `world`. That breaks §25 (arithmetic must be deterministic), the store has no series (only features like prevDayPx for a 24 h change), and it costs a Jev call per tick.
- **Evidence:** rg -i 'zscore|z_score|rolling|baseline|anomal|detector|pct_change|move_5m|samples' src -> lp/market.rs sample ring, DlmmAnomaly decoder flags (lp/dlmm.rs:625), skill_lifecycle rolling window; rg 'observed_at_ms (<|>)|WHERE schema' observations.rs -> only the purge (:67); read observations.rs, market.rs.

### `hl-ws-stream` — HL WS: `allDexsAssetCtxs` + bbo / l2Book / trades ⇒ stream-sourced `mkt_ctx` rows

M2 · L · rust · missing · outbound + bootstrap · PRD §3 §14 §20 §35 S2
After: `hl-market-schema` (M0), `kg-sync-hyperliquid` (M1), `rt-scheduler` (M0), `rt-ws-client` (M2)

- **Have:** No WS client code (rg for tungstenite, websocket, wss://, EventSource, grpc: none). tokio-tungstenite 0.26.2 is already in Cargo.lock:5885 via alloy-transport-ws. ObsSource::Stream exists (src/domain/observation.rs:31). The phase-5 design (stream rows + heartbeat stream/1:<name>) is in docs/SESSION_HANDOFF.md.
- **Need:** outbound: src/adapters/outbound/hyperliquid/ws.rs. (1) Connect to wss://api.hyperliquid.xyz/ws through the rt egress WS dialer. (2) Subscribe {method: subscribe, subscription: {type: allDexsAssetCtxs}}; each message is about 130 KB and carries all 11 dexes' ctxs as ctxs [[dex, [ctx...]]], index-aligned with allPerpMetas. For each watched coin also subscribe bbo, l2Book, trades and activeAssetCtx. (3) Send {method: ping} at least every 60 s (the server drops connections idle for 60 s); reconnect and resubscribe with backoff. Per-IP limits: 10 connections, 30 new per minute, 1000 subscriptions, 2000 messages per minute. (4) Decode and put mkt_ctx/1:hyperliquid:<coin> rows with source = stream, plus a heartbeat row stream/1:hyperliquid. (5) Threshold crossings call DecisionLoop::handle_event (src/application/decision_loop/mod.rs:104) via rt. tokio-tungstenite becomes a direct dependency at the same version, behind a cargo feature.
- **No-Rust path:** An external WS-to-webhook bridge could POST events to 'tengu webhooks' (loop = ...). But that is out-of-repo infra, bypasses the egress audit, and still cannot write observation rows.
- **Evidence:** rg -n 'tungstenite|websocket|WebSocket|wss://|EventSource|text/event-stream|grpc' src: none; the only 'tonic' hits are the word 'monotonic'. Cargo.toml has no ws/sse/grpc dependency. rg -n -i 'interval_secs|every_secs|poll_secs|cron|schedul|tokio::time::interval' src: no scheduler. websocat probes: activeAssetCtx, bbo, trades, allDexsAssetCtxs.

### `hl-funding-tool` — `hl_funding`: settled history, current + predicted funding (HL vs Binance vs Bybit)

M2 · M · rust · missing · domain + outbound (tools) · PRD §14 §20 §21 §23 §35 S2
After: `hl-info-client` (M0), `hl-market-schema` (M0)

- **Have:** Nothing.
- **Need:** domain src/domain/hl/funding.rs + tool hl_funding. Args: coin*, lookback_h (default 24). Sources: (1) fundingHistory: hourly {coin, fundingRate, premium, time}, at most 500 per page, weight 20 + 1 per 20 items; (2) the current-hour ctx funding; (3) predictedFundings: default dex only (234 coins), entries HlPerp / BinPerp / BybitPerp with fundingIntervalHours, normalised per hour. Key hl_funding/1:<coin>, TTL 60 s. Features: funding_1h_now, funding_1h_avg_24h, funding_1h_max_24h, funding_delta_1h, funding_z_24h, funding_apr_pct, bin_funding_1h, bybit_funding_1h, hl_minus_cex_funding_bps, next_funding_s. HIP-3 coins have no predicted CEX entries, so those features are absent; hl-cex-ctx-tool covers Binance/Bybit/OKX equity perps.
- **No-Rust path:** A jev-exec POST of fundingHistory with reduce /*/fundingRate returns at most 20 raw strings. Averages, z-scores and interval normalisation need code.
- **Evidence:** rg -n -i 'open_?interest|funding_?rate|fundingRate' src: Jupiter only. Probes: fundingHistory xyz:TSLA (24 rows); predictedFundings (234 coins, none containing ':').

### `hl-candles-tool` — `hl_candles`: returns, realised vol, volume vs normal

M2 · M · rust · missing · domain + outbound (tools) · PRD §3 §14 §20 §25 §35 S2
After: `hl-info-client` (M0), `hl-market-schema` (M0)

- **Have:** Nothing. The only volatility code is the Meteora DLMM volatility accumulator (src/domain/lp/dlmm.rs:127-130).
- **Need:** domain src/domain/hl/candles.rs + tool hl_candles. Args: coin*, interval (1m 3m 5m 15m 30m 1h 2h 4h 8h 12h 1d 3d 1w 1M), lookback (at most 5000). Key hl_candles/1:<coin>:<interval>:<lookback>, TTL min(interval, 60 s). Features: ret_last_pct, ret_window_pct, realized_vol_ann_pct, vol_last_usd, vol_ratio_vs_median (the §14 '8x normal volume'), range_pct, trades_last. Weight 20 + ceil(n/60).
- **No-Rust path:** http_request candleSnapshot + reduce /*/{t,c,v} truncates to 20 candles, and the vol/median math needs code.
- **Evidence:** rg -n -i 'candle|ohlc|realized_vol|volatility' src: only DLMM volatility accumulator. Probe: candleSnapshot xyz:TSLA 1h (73 candles {t,T,s,i,o,c,h,l,v,n}).

### `hl-cex-ctx-tool` — `cex_ctx`: reference bid / ask / mark / index / funding / OI from Binance, Bybit, OKX, Coinbase

M2 · L · rust · missing · domain + outbound (tools) · PRD §2 §9 §12 §20 §21 §25 §35 S2
After: `hl-cex-universe-tool` (M1), `hl-info-client` (M0), `hl-market-schema` (M0)

- **Have:** Nothing typed. jev-exec reads the Coinbase spot price via http_request (sandboxes/jev-exec/config.toml:120-126).
- **Need:** domain src/domain/cex/ticker.rs, with one decoder per venue mapping into MarketCtx. (1) Binance USDⓈ-M: /fapi/v1/premiumIndex (markPrice, indexPrice, lastFundingRate, nextFundingTime), /fapi/v1/ticker/bookTicker, /fapi/v1/openInterest, /fapi/v1/fundingInfo (interval hours, caps). Binance spot: /api/v3/ticker/bookTicker, or data-api.binance.vision. (2) Bybit: one call to /v5/market/tickers?category=linear|spot (bid1Price, ask1Price, markPrice, indexPrice, fundingRate, fundingIntervalHour, openInterestValue). (3) OKX: /api/v5/market/ticker, /api/v5/public/funding-rate, /api/v5/public/open-interest. (4) Coinbase: https://api.exchange.coinbase.com/products/<id>/ticker; INTX https://api.international.coinbase.com/api/v1/instruments/<id>/quote (index_price, mark_price, predicted_funding). Tool cex_ctx. Args: venue*, symbol*, max_age_secs. Key mkt_ctx/1:<venue>:<symbol>, TTL 5 s. Funding is normalised per hour, and quote_ccy is tagged: USDT perps vs HL's USDC need a USDT/USDC rate for basis, e.g. HL spot @166 USDT0/USDC. Error mapping: Binance 451 => AuthRequired (restricted location), 418 => RateLimited (IP ban), 429 => RateLimited; Bybit 403 'access too frequent' => RateLimited (at least 10 min), a geo 403 => AuthRequired; OKX HTTP 200 with code != 0 is classified by that code.
- **No-Rust path:** Jev-exec actions per venue are a cheap P1 stopgap, e.g. Bybit tickers with reduce /result/list/0/{bid1Price,ask1Price,markPrice,fundingRate,openInterestValue}. Lost: funding-interval and quote-currency normalisation, rows for world/requires, ErrorClass.
- **Evidence:** rg -n -i 'binance|bybit|okx|coinbase' src: only the test URL. 14 REST probes returned 200 from DE. TSLA cross-venue snapshot (HL, Binance, Bybit, OKX).

### `hl-cex-liquidations` — CEX liquidation streams ⇒ `liq/1:<instrument id>`

M2 · M · rust · missing · domain + outbound · PRD §3 §16 §21 §35 S2
After: `hl-market-schema` (M0), `rt-scheduler` (M0), `rt-ws-client` (M2)

- **Have:** Nothing. rg for liquidation finds only the Jupiter perps liquidation price.
- **Need:** Outbound streams run by the rt runner: (1) Binance wss://fstream.binance.com/market/ws/!forceOrder@arr (the legacy /ws URLs stopped carrying /market streams on 2026-04-23); (2) Bybit wss://stream.bybit.com/v5/public/linear, topic allLiquidation.<symbol>; (3) OKX wss://ws.okx.com:8443/ws/v5/public, channel liquidation-orders (instType SWAP); REST fallback OKX /api/v5/public/liquidation-orders. Domain aggregates in liq/1:<venue>:<symbol>: long_liq_usd and short_liq_usd over 1 m / 5 m / 1 h, max_print_usd, n_prints. A no-message watchdog marks the row as error, never 0.
- **No-Rust path:** None for streams. Only the OKX REST fallback could be a jev-exec action.
- **Evidence:** rg -n -i liquidation src: Jupiter perps only. websocat: Binance /ws/btcusdt@markPrice silent vs /market/ws/btcusdt@markPrice streaming; Bybit allLiquidation.BTCUSDT and OKX liquidation-orders subscriptions acknowledged.

### `risk-calc-market-stats` — Returns, volume z-score, OI change, funding 1h / 8h / APR, realised vol, AR(1) half-life, freshness, alignment, lead-lag

M2 · M · rust · missing · domain · PRD §25 §12 §14 §20 §34 §35 S2 §35 S6
After: `ops-history-recorder` (M1)

- **Tracker note:** Add `half_life_ar1(series)` here (M2) for the M3 report.
- **Have:** Only the LP storm move (move_5m_pct against the oldest sample >= 4 min, src/domain/lp/gates.rs:144) and Jupiter-perps borrow APR (src/domain/lp/perps.rs). rg finds only DLMM's volatility accumulator (src/domain/lp/dlmm.rs:127-131).
- **Need:** domain: src/domain/xm/stats.rs. Pure, now_ms as an input, Field<f64> outputs (missing means Absent, never 0).
  - ret(series, w): needs a reference sample aged within [w, 1.25w].
  - vol_z(series, w, baseline_n): sum over w against the mean and std of the previous n windows, with a minimum sample count.
  - oi_change_pct(w).
  - funding(rate_1h, multiplier, interest_override) -> {1h, 8h = x8, apr = x24x365}. Takes the HIP-3 per-asset assetToFundingMultiplier / assetToFundingInterestRate (probe: xyz:TSLA multiplier 0.5).
  - realized_vol(series, grid_ms, w), annualised.
  - freshness(age_ms, max).
  - align(a, b, tol_ms): pairs samples by nearest-before venue timestamp and returns skew_ms. Needs venue time from the hl slice, e.g. l2Book.time, ideally also as Observation.slot for monotonic upserts.
  - lead_lag(a, b, lags): cross-correlation of 1 s returns (§34 'which venues lead').
  Tests: synthetic series with closed-form answers (constant drift, known sigma), alignment edge cases, missing-sample vectors.
- **No-Rust path:** None. §25: Jev reasons over the results and does not reproduce the arithmetic.
- **Evidence:** rg -n -i 'stddev|std_dev|variance|z_?score|zscore|realized_vol|realised|volatility|log_return|rolling|ewma|annuali[sz]' src: only DLMM volatility fields and skill_lifecycle rolling_window.
- **Absorbed `hl-oi-history`** — OI / mark / funding history for change features (oi_change_pct, funding trend). HL has no historical-OI info type; only the current openInterest is available. So sample locally: (a) a ring inside the mkt_ctx/1 data, 1-minute buckets x 60, giving oi_change_pct_15m, oi_change_pct_1h, mark_change_pct_15m and funding_delta_1h; (b) 24 h and 7 d windows from a tick-history table fed by the stream (rt / ops). Features are omitted until the window is covered, never set to 0. Regular sampling comes from rt-feed-runner or hl-ws-stream.

### `rh-quote` — `rh_quote`: RH reference bid / ask, token-equivalent (× uiMultiplier), Chainlink oracle + USD anchors, halt

M2 · M · rust · missing · domain+outbound · PRD §8 §12 §20 §25 §35 S2
After: `kg-calendars` (M1), `kg-sync-robinhood` (M1), `rh-evm-rpc` (M1)

- **Tracker note:** Key uses the convention-1 id, e.g. `rh_quote/1:robinhood:0x322F0929c4625eD5bAd873c95208D54E1c003b2d`. Also read a Chainlink USDC/USD feed (address to pin in TOML) next to USDG/USD for `x-quote-ccy-normalization`.
- **Have:** Nothing typed. A jev-exec-style http_request action could GET /rhj/prices (pattern in sandboxes/jev-exec/config.toml:120-126).
- **Need:** Tool rh_quote in src/adapters/outbound/tools/rh/quote.rs; pure math in src/domain/rh/quote.rs.
  
  Per symbol:
  - GET https://api.robinhood.com/rhj/prices/{symbol}: bid, ask, tokenBid, tokenAsk, isTradingHalt, dailyTradingVolume, dailyHigh, dailyLow, mintBurnTokenVolume, mintBurnUsdVolume, generatedAt.
  - Plus one Multicall3 eth_call covering: uiMultiplier() 0xa60bf13d, oraclePaused() 0x7706ba52, newUIMultiplier() 0xdc767007, effectiveAt() 0x97a4064f, the feed's latestRoundData() 0xfeaf968c, and the USDG / USD feed 0x61B7e5650328764B076A108EFF5fa7282a1B9aD2.
  
  Batch mode (symbols = '*'): GET https://api.robinhood.com/rhj/prices returns 195 quotes in one call. Write per-token rows plus rh_quotes/1:4663 for Stage-2 scans.
  
  Key rh_quote/1:<contract> (e.g. rh_quote/1:0xaF3D76f1834A1d425780943C99Ea8A608f8a93f9), TTL 15 s, slot = block.
  
  Features: ref_bid, ref_ask, token_bid, token_ask, token_mid, spread_bps, multiplier, halted, oracle_price, oracle_age_s, oracle_paused, oracle_vs_token_mid_bps, quote_age_s, day_volume, mint_burn_usd_day, session, mint_window_open, head_age_s.
  
  A failed leg becomes Field::Error with status partial, never 0.
- **No-Rust path:** Stopgap available today as a loop action:
  - tool = 'http_request'
  - args = { method = 'GET', url = 'https://api.robinhood.com/rhj/prices/{symbol}', return_body = true }
  - reduce = { q = '/quotes/0/{bid,ask,tokenBid,tokenAsk,isTradingHalt,generatedAt}' }
  Results reach Jev history only. No store row means no world or requires, and no join with the oracle or multiplier.
- **Evidence:** - rg -n -i 'chainlink|latestRoundData|aggregatorv3|uiMultiplier|oraclePaused' src sandboxes skills: none.
  - Probes: /rhj/prices/AAPL; AAPL token views; feed 0x6B22A786bAa607d76728168703a39Ea9C99f2cD0 was 16,167 s old; Multicall3 aggregate3 4/4 ok.

### `rh-dex-quote-v3` — `rh_dex_quote`: Uniswap v3 QuoterV2 size ladder, buy + sell

M2 · M · rust · missing · domain+outbound · PRD §20 §21 §25 §31 §35 S2
After: `kg-sync-robinhood` (M1), `rh-evm-rpc` (M1)

- **Tracker note:** Key uses the convention-1 id: `rh_dex_quote/1:robinhood:<token address>`.
- **Have:** nothing
- **Need:** Tool rh_dex_quote in src/adapters/outbound/tools/rh/dex.rs. Pure src/domain/rh/dex.rs computes effective price per size, impact_bps vs mid, and best-of-pools.
  
  Pools:
  - getPool(token, quote, fee) on UniswapV3Factory 0x1f7d7550B1b028f7571E69A784071F0205FD2EfA for fees 100 / 500 / 3000 / 10000.
  - Store as rh_pools/1:<token>, TTL 600 s, with balances and liquidity, filtered by a minimum liquidity.
  
  Quotes:
  - QuoterV2 0x33e885eD0Ec9bF04EcfB19341582aADCb4c8A9E7, quoteExactInputSingle((address,address,uint256,uint24,uint160)), selector 0xc6a5026a.
  - Batched through Multicall3, both directions, over the sizes_usd ladder (default [1000, 10000, 50000]).
  - Quote token: USDG 0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168 (6 dp), or WETH 0x0Bd7D308f8E1639FAb988df18A8011f41EAcAD73 priced via Chainlink ETH / USD 0x78F3556b67E17Df817D51Ef5a990cDaF09E8d3A9.
  
  Gas = quoter gasEstimate x L2 gas price + L1 data fee (ArbGasInfo 0x000000000000000000000000000000000000006C).
  
  Key rh_dex_quote/1:<token>:<quote_token> (e.g. rh_dex_quote/1:0xaF3D76f1834A1d425780943C99Ea8A608f8a93f9:0x5fc5360D0400a0Fd4f2af552ADD042D716F1d168), TTL 5 s, slot = block.
  
  Features: bid_1k, ask_1k, bid_10k, ask_10k, bid_50k, ask_50k, impact_bps_10k, best_fee_tier, pools_n, gas_usd, liquidity_ok.
- **No-Rust path:** None viable. QuoterV2 takes a tuple, which abi_encode cannot encode (crypto/helpers.rs:207-212). It returns 4 ABI words that loop reducers cannot slice. An EVM MCP server would only return raw hex.
- **Evidence:** - rg -n -i 'uniswap|quoter|getPool|sqrtPrice|swap_quote' src sandboxes: none (only prose in skills/privy-agentic-wallets/references/security.md:53).
  - Probes: getPool for AAPL / TSLA / NVDA against USDG and WETH; pool balances; QuoterV2 ladder (sell 1 AAPL = 331.010746 USDG; buy with 33100 USDG = 99.597012953034856933 AAPL).

### `rh-dex-quote-v4` — Uniswap v4 quoting over a curated pool allowlist (3,415 AAPL pools, mostly spam)

M2 · M · rust · missing · domain+outbound+config · PRD §20 §21 §35 S2
After: `rh-dex-quote-v3` (M2)

- **Have:** nothing
- **Need:** Extend rh_dex_quote with v4.
  
  Discovery: PoolManager 0x8366a39CC670B4001A1121B8F6A443A643e40951 Initialize(bytes32,address,address,uint24,int24,address,uint160,int24) logs, filtered to:
  - currency0 in {ETH 0x0000000000000000000000000000000000000000, USDG, WETH};
  - hooks = 0x0000000000000000000000000000000000000000 or on a TOML allowlist;
  - fee <= 10000;
  - StateView 0xF3334192D15450CdD385c8B70e03f9A6bD9E673b getLiquidity(bytes32) >= a minimum.
  Store the survivors in rh_pools/1:<token> with their PoolKeys.
  
  Quotes: V4Quoter 0x8Dc178eFB8111BB0973Dd9d722ebeFF267c98F94, quoteExactInputSingle((PoolKey,bool,uint128,bytes)).
  
  Optional TOML pin list: [evm.chains.robinhood.pools].
- **No-Rust path:** None: the same tuple and decoding limits as v3 apply.
- **Evidence:** - Probe: 3,415 AAPL Initialize events; 158 of them against ETH / USDG / WETH; many with fee 900000 / 950000.
  - Non-zero liquidity, e.g. pool id 0x7458bf9fb741da112910cd1ff786d5ee95bcb729fc3dcda258e8bc0956f3d5ef (ETH/AAPL, fee 2500) and 0xc748f4671a867db48b552f6b7650bf3255e05f80f00e3f7aad1b17ccb7898fdb (USDG/AAPL, fee 3000).
  - rg -n -i 'poolmanager|v4quoter|stateview|poolkey' src: none.

### `rh-lighter` — Lighter Robinhood-domain markets + books (stock-token spot, equity and pre-IPO perps)

M2 · M · rust · missing · domain+outbound · PRD §4 §12 §19 §20 §35 S1 §35 S2
After: `kg-sync-robinhood` (M1)

- **Have:** nothing
- **Need:** Tool lighter_rh_markets:
  - Key lighter_rh_markets/1:rh, TTL 300 s.
  - 84 markets: symbol, market_id, spot or perp, multiplier, fees, min sizes.
  - Spot '<SYM>/USDG' mapped to the RH contract via rh_asset.
  
  Tool lighter_rh_book:
  - Key lighter_rh_book/1:<market_id> (e.g. lighter_rh_book/1:2049), TTL 2 s.
  - Top N levels, walk-the-book bid/ask at the size ladder, last_trade_price, open_interest, daily base/quote volume.
  
  Endpoints: https://api.rh.lighter.xyz/api/v1/orderBooks, /api/v1/orderBookOrders?market_id=<id>&limit=<n>, /api/v1/orderBookDetails?market_id=<id>.
  
  Files: src/adapters/outbound/tools/rh/lighter.rs; pure book walk in src/domain/rh/book.rs.
  
  Also covers §4 synthetic exposure: ANTHROPIC market 38, OPENAI 42.
- **No-Rust path:** Partial. http_request GET is keyless JSON, and a reducer can project '/asks/0/price' into Jev history. There is no store row, no size-aware book walk and no market-id mapping.
- **Evidence:** - rg -n -i 'lighter|orderbook|order_book|book_walk' src sandboxes skills: none.
  - Probe: 84 markets (27 spot, 57 perp); AAPL/USDG market 2049 at 330.93 / 331.30; ANTHROPIC market 38 with OI 3391.72405.

### `info-parsers-ext` — RSS 2.0 (incl. `ndaq:` elements), JSON-pointer item mapping, Telegram `t.me/s` preview, HTML → text (`html2text`, new crate)

M2 · M · rust · missing · outbound · PRD §15 §35 S2 §35 S3
After: `info-fetch` (M0), `info-parsers` (M0)

- **Have:** Split out of `info-parsers` by the review (2026-09-29); nothing exists yet (see the `info-parsers` evidence).
- **Need:** The non-Atom half of `info-parsers`: the RSS 2.0 path of `rss_atom` (quick-xml 0.31 as a direct dependency) exposing namespaced children such as `ndaq:` for the Nasdaq halts feed; `json_map`, reusing the reducer path grammar `reduce::select` and driven by TOML `{items, id, title, url, published, published_format, body, tickers}`; `tg_preview` (data-post, tgme_widget_message_text, `<time datetime>`, forwarded-from); `html_text` via `html2text` 0.17.1 (new dependency) for EDGAR exhibits and article bodies. Goldens in `tests/fixtures/news/` from the 2026-09-29 probes (PR Newswire RSS, SEC / CFTC / Fed / ECB feeds, `t.me/s` HTML, exchange JSON, Nasdaq halts RSS).
- **No-Rust path:** None for parsing; each source stays a TOML `[feeds]` row.
- **Evidence:** Same as `info-parsers`.

### `rh-halts` — `[feeds.nasdaq_halts]` poll row (RSS) writing `us_halt/1` rows, shared with `info-sources-mvp`; the gate blocks entries on halted names

M2 · S · toml · missing · outbound+domain · PRD §20 §28 §35 S2
After: `info-parsers-ext` (M2), `kg-calendars` (M1)

- **Tracker note:** Now a TOML `[feeds.nasdaq_halts] kind = "poll"` RSS row writing `us_halt/1` rows (needs `info-parsers-ext` for `ndaq:` elements), shared with `info-sources-mvp`; only the 'halted blocks entries' check is Rust, inside the risk gate. The Rust tool described below is superseded (review, doctrine #2).
- **Have:** Nothing. quick-xml 0.31.0 is already in Cargo.lock via calamine (Cargo.lock:4259-4266, 1457-1466) but is not a direct dependency.
- **Need:** Tool us_halts in src/adapters/outbound/tools/market/halts.rs. Add quick-xml as a direct dependency; it is already in the tree, so no new crate.
  
  Source: GET https://www.nasdaqtrader.com/rss.aspx?feed=tradehalts.
  
  Rows:
  - us_halt/1:<SYMBOL>, TTL 30 s. Features: halted, reason_code (LUDP, T1, H10, ...), market, halt_age_s, resume_expected_s.
  - us_halts/1:nasdaqtrader with active_n.
  
  rh_basis and the risk gates treat halted = true, or rh_quote.halted, as a hard block.
- **No-Rust path:** None practical. http_request returns the XML as text, and loop reducers only parse JSON (src/application/decision_loop/reduce.rs:96-114).
- **Evidence:** - rg -n -i 'halt|luld|nasdaqtrader|tradehalts' src sandboxes skills: none.
  - Probe: 40 items, including LSE LUDP at 14:24:56.356.

### `rh-ref-equities` — `eq_quote` for non-tokenized tickers (Alpaca IEX, SIP later; Massive optional)

M2 · M · rust · missing · outbound+config · PRD §8 §12 §20 §35 S2 §35 S4
After: `kg-calendars` (M1), `rh-accounts` (M2), `rt-ws-client` (M2)

- **Have:** Nothing. http_request can call Alpaca with env-expanded headers (src/adapters/outbound/tools/http/request.rs:538-599).
- **Need:** Tool eq_quote in src/adapters/outbound/tools/market/equities.rs. Add a provider port src/ports/market_data.rs only when a second provider lands.
  
  Alpaca: GET https://data.alpaca.markets/v2/stocks/snapshots?symbols=<A,B>&feed=iex|sip, with APCA-API-KEY-ID and APCA-API-SECRET-KEY from $ALPACA_API_KEY_ID / $ALPACA_API_SECRET_KEY (via env_reads).
  
  Key eq_quote/1:<SYMBOL>, TTL 5 s. Features: bid, ask, last, day_volume, prev_close, change_pct, rel_volume_20d, quote_age_s, feed.
  
  config: [market_data.equities] provider = 'alpaca' or 'massive'; feed = 'iex' or 'sip'.
  
  Streams come later through the rt slice: wss://stream.data.alpaca.markets/v2/iex or /v2/sip, which carry trading statuses and LULD bands.
- **No-Rust path:** Works as a stopgap: a skill plus http_request with headers {'APCA-API-KEY-ID': '$ALPACA_API_KEY_ID', ...}, with env expansion gated by env_reads. Output is text only, so nothing reaches world or rh_basis.
- **Evidence:** - rg -n -i 'alpaca|polygon|massive|finnhub|tiingo|twelvedata|databento|eodhd|iex' src sandboxes skills config.example.toml: none.
  - Probes: Massive, Polygon and Finnhub return 401 without a key; EODHD demo AAPL.US returned 331.23 (~14 min old).

### `rh-activity` — `rh_activity`: AP mint / burn, whale transfers, pool swaps, multiplier events

M2 · L · rust · missing · domain+outbound · PRD §1 §3 §14 §20 §35 S2
After: `kg-sync-robinhood` (M1), `rh-dex-quote-v3` (M2), `rh-evm-rpc` (M1), `rt-scheduler` (M0)

- **Have:** Nothing: src has no log reading, no scheduler or interval poller, and no stream.
- **Need:** Tool rh_activity, poll-once from a cursor. Files: src/adapters/outbound/tools/rh/activity.rs; aggregates in src/domain/rh/activity.rs.
  
  eth_getLogs over [cursor+1, head] for registry tokens:
  - Transfer 0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef. From/to 0x0000000000000000000000000000000000000000 = AP mint/burn; value >= large_usd = whale transfer.
  - UIMultiplierUpdated 0x2205df4534432b2f60654a3fdb48737ffdaf3e9edb1a498bd985bc026b15b055.
  - v3 Swap 0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67 on pools from rh_pools rows.
  - v4 Swap 0x40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f on the PoolManager, filtered by poolId.
  
  Rows:
  - rh_activity/1:<contract>, TTL 60 s. Features: net_mint_usd_1h, mint_usd_1h, burn_usd_1h, swap_vol_usd_5m, swap_n_5m, last_swap_price, buy_sell_imbalance_5m, whale_n_1h, vol_x_baseline.
  - Cursor rh_cursor/1:logs:4663.
  
  Also emits an event JSON for loop triggers. The rt/ops runner calls it every N seconds.
- **No-Rust path:** Push alternative: an Alchemy Notify custom webhook to a tengu webhooks endpoint with loop = 'rh_watch'. The listener only verifies x-tengu-signature 'sha256=<hex>' or a static Authorization header (src/adapters/inbound/webhooks.rs:78, src/config/mod.rs:645-652); see rh-webhook-sig. Even then, payloads never become store rows.
- **Evidence:** - rg -n -i 'getLogs|get_logs|log.?watch|mint.?burn|whale' src: none.
  - rg -n -i 'tokio::time::interval|schedule|cron|tick_secs|poll_secs|--every|--interval' src: none (only the tool_loop cancel poll and the scanner's crontab regex).
  - rg -n -i 'websocket|tungstenite|eventsource|grpc|tonic' src: none.
  - Probe: 371 AAPL transfers per 10k blocks; RPC limits.

### `rh-corp-actions` — `rh_corp_actions`: splits, dividends, oracle-pause windows, multiplier schedule

M2 · S · rust · missing · outbound+domain · PRD §20 §28 §35 S2
After: `kg-sync-robinhood` (M1), `risk-gate-domain` (M0), `risk-gate-enforcement` (M0)

- **Have:** nothing
- **Need:** Tool rh_corp_actions.
  
  Source: GET https://api.robinhood.com/rhj/corporate-actions (1 h cache).
  - Active types: FORWARD_SPLIT, REVERSE_SPLIT, CASH_DIVIDEND, STOCK_DIVIDEND.
  - Statuses: IN_PROGRESS, COMPLETED.
  
  Join with rh_asset pendingMultiplier / effectiveAt and oraclePaused().
  
  Rows:
  - rh_corp_actions/1:4663.
  - rh_ca/1:<contract>. Features: pending, type, effective_in_s, oracle_paused, last_ratio.
  
  Feeds a risk gate: no new paper entries while a change is pending within N hours or the oracle is paused.
- **No-Rust path:** Partial. http_request GET /rhj/corporate-actions is JSON a reducer can project, but it cannot be joined with the on-chain pause flag or multiplier.
- **Evidence:** - rg -n -i 'corporate.?action|stock.?split|dividend|uiMultiplier' src: none.
  - Source: the Stock Token APIs page decoded from the docs bundle.

### `rh-research-licensing` — Terms + provenance of RHJ API quotes and reference-data licences

M2 · S · research · missing · n/a · PRD §15 §20 §33

- **Have:** nothing
- **Need:** Confirm:
  (a) api.robinhood.com/rhj terms for automated polling (docs state 60 req/s);
  (b) where the /prices bid/ask comes from (consolidated NBBO vs Robinhood's own or overnight venue), which decides whether it can serve as 'reference' in §20 and in Stage-8 evaluation;
  (c) whether Alpaca / Massive non-professional terms fit a research and paper system;
  (d) Lighter, LI.FI and Stock Token terms for the operator's jurisdiction.
- **No-Rust path:** n/a (research)
- **Evidence:** RH docs give rate limits but not data provenance. The Massive, Twelve Data and Tiingo pricing pages restrict individual plans to non-professional or internal use.

### `rh-accounts` — RPC provider, LI.FI key, Alpaca (optional Envio / OpenFIGI / Twelve Data)

M2 · S · account · missing · n/a · PRD §20 §35 S2

- **Have:** None configured. The secrets vault exists (tengu secret, src/adapters/outbound/secrets.rs).
- **Need:** All stored via tengu secret:
  - Alchemy app for Robinhood Chain (free: 30M CU/mo, 25 RPS, 5 webhooks) as ROBINHOOD_RPC_URL.
  - LI.FI partner key (100 req/min) as LIFI_API_KEY.
  - Alpaca account as ALPACA_API_KEY_ID / ALPACA_API_SECRET_KEY:
    - Basic (free): IEX real-time, 200 req/min, 30 WS symbols.
    - Algo Trader Plus ($99/mo): SIP plus statuses and LULD.
  - Optional: Envio token for HyperSync backfills, an OpenFIGI key, a Twelve Data key.
- **No-Rust path:** n/a (accounts only)
- **Evidence:** rg -n -i 'ROBINHOOD_RPC_URL|ALPACA|LIFI|ALCHEMY' src sandboxes config.example.toml: none.

### `x-accounts-secrets` — Accounts + secrets inventory for xmarket

M2 · S · account · missing · infra | config · PRD §15 §20 §35 S2 §35 S3
After: `ops-openrouter-budget-key` (M0)

- **Tracker note:** Add AWS requester-pays credentials for the HL archive (M7).
- **Have:** The vault loads secrets into env (src/adapters/outbound/secrets.rs:239), and http_request expands $ENV only for names allowed by env_reads. There is no inventory of xmarket accounts or keys. rh-accounts covers only Robinhood-side keys. Four info gaps depend on an `ops-secrets` gap that no report defines.
- **Need:** One table in the ops runbook, mirrored as comments in sandboxes/xmarket/config.toml. Entries: SEC_USER_AGENT (declared contact; EDGAR returns 403 without it); a dedicated OPENROUTER_API_KEY with a daily limit; an X developer app with prepaid credits (X_BEARER_TOKEN, per-cycle spending limit, auto-recharge off); Alpaca APCA_API_KEY_ID and APCA_API_SECRET_KEY; optional ROBINHOOD_RPC_URL (Alchemy), LIFI_API_KEY, OPENFIGI_API_KEY, COINGECKO_DEMO_API_KEY, FINNHUB_API_KEY; TELEGRAM_BOT_TOKEN and XM_ALERT_CHAT_ID. For each: owner, cost, env var, which agent's env_reads grants it (tokens never go in [default_scopes]), rotation. Optional Rust (S): `tengu doctor --sandbox xmarket` lists which names are set, never their values.
- **No-Rust path:** Yes — accounts, vault entries and TOML only; the doctor listing is optional
- **Evidence:** rg -n 'SEC_USER_AGENT|X_BEARER_TOKEN|APCA_API_KEY_ID|XM_ALERT_CHAT_ID' src sandboxes skills config.example.toml → 0; rg -i 'user_agent|user-agent' src → 0; info-store, info-sources-mvp, info-x-ingest and info-why-moving-skill all list `ops-secrets` in depends_on — **2026-10-08:** stale for SEC: `SEC_USER_AGENT` is read by `src/adapters/outbound/backfill/sec.rs` (P7) and named by the `[sources]` `sec_edgar` row (O2, `docs/source-evidence-2026-10-08.md`)

### `ops-log-rotation` — Rotation / retention for `logs/*.jsonl` + `tengu.log`; `decisions.jsonl` stays unrotated until `ops-audit-store` (M5)

M2 · S · infra · missing · infra | outbound · PRD §32
After: `ops-audit-store` (M5)

- **Tracker note:** Keep `decisions.jsonl` out of rotation until `ops-audit-store` (M5).
- **Have:** <TENGU_HOME>/logs/{decisions,egress}.jsonl and tengu.log grow without bound. Both writers reopen the file per line (src/adapters/outbound/egress.rs:484-488; src/application/decision_loop/mod.rs:455-458), so external rotation is safe, and the TUI tailer restarts on truncation (src/adapters/inbound/tui/mod.rs:66-70). `tengu prune` deletes <TENGU_HOME>/logs (src/adapters/outbound/prune.rs:56), including the decision audit. Egress audits every call, so HL polling of 11 dexes every 15 s alone adds ≈63k lines/day.
- **Need:** Make <TENGU_HOME>/state/audit/ the authoritative audit location (ops-audit-store) and keep logs/ for mirrors. Add deploy/logrotate.d/tengu (daily, rotate 30, compress, copytruncate) or rely on the compose logging driver. Optional [egress] audit_skip_hosts (or sampling) for high-rate pollers. Add a prune.rs test that state/audit and state/history survive both soft and hard prune.
- **No-Rust path:** yes — host logrotate. Trade-off: excluding the audit from prune needs a small Rust change, or a rule never to run prune on the daemon host.
- **Evidence:** rg -n 'logs|state' src/adapters/outbound/prune.rs; read egress.rs:470-491; rg -n 'tengu\.log|RollingFileAppender|tracing_appender' src → no rotation

### `ops-docs` — xmarket ops runbook; fix stale decision-audit docs

M2 · S · docs · partial · n/a · PRD §32
After: `ops-audit-record-v2` (M5), `ops-history-recorder` (M1)

- **Have:** docs/decision-loop-plan-2026-09-24.md:108 claims the audit holds 'state hash, questions'; the code writes neither (mod.rs:434-450). docs/code-map.md:174 describes decisions.jsonl. There is no operator runbook for a long-running data daemon.
- **Need:** docs/xmarket-ops-<date>.md (one screen: deploy, secrets, budget key, alerts, retention, audit/history paths, replay/report commands). Correct § Observability in the decision-loop plan. Add code-map.md/.html rows for the new files. Add the new hosts to the egress doc table (api.hyperliquid.xyz, rpc.mainnet.chain.robinhood.com, api.x.com, api.telegram.org). Add a recorder section to typed-observations-2026-09-24.md. Update the SESSION_HANDOFF entry.
- **No-Rust path:** yes — docs only
- **Evidence:** read docs/decision-loop-plan-2026-09-24.md:103-110 vs src/application/decision_loop/mod.rs:434-450; rg -n 'decisions.jsonl' docs/code-map.md


## M3 — edge check (go / no-go)

### `x-quote-ccy-normalization` — → USD rows with named anchors: Chainlink USDC/USD + USDG/USD (read in `rh-quote`), Coinbase `USDT-USD` (`cex_ctx`; Coinbase is blocked from Kazakhstan, so keep a Chainlink USDT/USD fallback), HL spot USDT0/USDC; a missing rate is an error, never 1.0

M3 · S · rust · missing · domain | outbound · PRD §20 §21 §25 §35 S6
After: `hl-cex-ctx-tool` (M2), `hl-ctx-tool` (M0), `rh-quote` (M2), `risk-calc-tools` (M0)

- **Tracker note:** Name each USD anchor with its writer: Chainlink USDC/USD + USDG/USD read in `rh-quote`'s Multicall (addresses pinned in TOML), Coinbase `USDT-USD` via `cex_ctx`, HL spot USDT0/USDC. USDH / USDE are dropped until a source exists. Coinbase is unreachable from Kazakh connections (the operator is in Kazakhstan), so keep a Chainlink USDT/USD fallback.
- **Have:** Venues quote in different stablecoins: HL HIP-3 collateral is USDC, USDH, USDE or USDT0; CEX perps quote in USDT; RH pools in USDG (6 decimals); references in USD. hl-cex-ctx-tool tags quote_ccy and rh-quote reads the USDG/USD Chainlink feed, but risk-calc-tools' xm_compare has no conversion step. The only precedent in code is the Solana Pyth USDC/USD feed id (src/domain/lp/market.rs:69-78).
- **Need:** Rows fx/1:<base>-<quote> from: HL spot @166 USDT0/USDC; Binance and Bybit USDCUSDT book tickers; the Chainlink USDG/USD feed 0x61B7e5650328764B076A108EFF5fa7282a1B9aD2 and a USDC/USD feed. A pure convert(px, from, to, rates, now) in src/domain/xmarket/ returns Field::Error when a rate is missing or stale; it never assumes 1.0. xm_compare, xm_cost and paper equity convert every leg to USD before computing basis or edge, and expose the rate and its age as features.
- **No-Rust path:** none — the conversion belongs inside the deterministic basis math (§25)
- **Evidence:** hl probe: HL xyz:TSLA bid 354.23 vs 354.43-354.56 on the USDT venues (6-9 bps) before conversion; hl probe per-dex collateralToken; rh probe USDG/USD feed; read src/domain/lp/market.rs:69-78

### `rt-latency-trace` — Source → ingest → bus → decide → execute trace rows in `runtime.db` + p50 / p95 per path

M3 · S · rust · partial · domain | application | inbound · PRD §31 §32 §34 §35 S8
After: `risk-paper-tools` (M0), `rt-bus-dispatch` (M2)

- **Tracker note:** Persist per-event trace rows in `runtime.db`: `decisions.jsonl` is rotated (M2) and deleted by `tengu prune`, so it cannot hold the M3 evidence window.
- **Have:** Jev latency exists only in tracing metrics lines (`latency_ms`, decision_loop/mod.rs:400-422; e.g. 702 ms and 279 ms in ~/.tengu/logs/tengu.log). The decision audit `ts` is unix seconds (mod.rs:435) and carries no source or ingest times.
- **Need:** domain: `ItemTrace { t_source_ms, t_ingest_ms, t_bus_ms, t_decide_start_ms, t_decided_ms, t_exec_done_ms }` on BusEvent. The audit adds ts_ms, trace and decision_latency_ms. Add CLI `tengu runtime stats --sandbox <s> [--since 1h]` (p50/p95 per feed / loop / path from decisions.jsonl), or documented jq recipes. The paper engine reads t_decided_ms for latency-aware fills.
- **No-Rust path:** jq over decisions.jsonl, but at 1 s resolution with no source time it is useless for sub-second lead-lag research (§34).
- **Evidence:** read decision_loop/mod.rs:400-464; local metrics lines in ~/.tengu/logs/tengu.log (see probes).

### `rh-research-weekend` — RH token pricing outside the mint window and US sessions: AMM vs Lighter vs last reference (RFQ after M6)

M3 · S · research · missing · n/a · PRD §2 §21 §34 §35 S6
After: `rh-dex-quote-v3` (M2), `rh-lighter` (M2), `rh-quote` (M2), `rt-scheduler` (M0)

- **Tracker note:** Measures AMM vs Lighter vs last reference only; RFQ quotes need `rh-agg-quote` (M6) and a LI.FI key (keyless 75 quotes / 2 h is below 60 s sampling).
- **Have:** nothing
- **Need:** Collect one week of 60 s samples of rh_quote, rh_dex_quote, lighter_rh_book and HL marks, spanning Fri 20:00 ET to Mon 02:00 Europe/Paris. Measure:
  - premium/discount vs the last reference;
  - the spread between AMM, Lighter and RFQ;
  - reversion speed at the session open.
  
  Output: gate defaults for rh_basis (enforceable windows, minimum edge per session) and a note in the skill.
- **No-Rust path:** n/a (research). Sampling needs the P0 tools and a runner.
- **Evidence:** Docs state the tokenization window, 'Stock feeds update 24/5', and Chainlink's 'no heartbeats during off-hours'. No data collected yet.

### `risk-s6-research-report` — `tengu xm research`: dislocation distributions after costs, half-life, lead-lag per pair, from `HistoryStore`

M3 · M · rust · missing · application + inbound (+ outbound/tools) · PRD §34 §35 S6 §33
After: `ops-history-recorder` (M1), `risk-calc-costs` (M0), `risk-calc-market-stats` (M2), `x-quote-ccy-normalization` (M3)

- **Tracker note:** Uses stats + costs only (the half-life comes from `risk-calc-market-stats`, not the M6 opportunity code), reads `HistoryStore::range` (not `series.db`), and needs `x-quote-ccy-normalization`.
- **Have:** nothing. Loops handle one event at a time; there is no batch analytics path.
- **Need:** application: src/application/xm_research.rs, a batch over series.db using domain/xm/{stats,cost,opportunity}. Outputs per pair: executable edge-after-costs percentiles, frequency above min_edge_bps, half-life, lead-lag by venue.
  inbound: `tengu xm research --sandbox xmarket --pair hyperliquid:xyz:TSLA,<reference> --since 7d`, writing JSON + Markdown under <workspace>/research/. Optionally a read-only tool `xm_research` for the architect.
  Tests: synthetic series give a known distribution.
- **No-Rust path:** Partial: the architect agent could read exported JSON with read_file and reason about it, but computing the distributions needs code (§25).
- **Evidence:** rg -n -i 'research|percentile|histogram' src/application src/adapters/inbound/cli: no market research path.


## M3b — live pilot on Hyperliquid ($100, after an M3 go)

### `hl-account-setup` — Hyperliquid sub-account funded with $100 USDC + an API wallet approved for that sub-account only (trades, cannot withdraw); key file 0600 outside every fs root; testnet first

M3b · S · account · missing · n/a · PRD §28 §30
After: `risk-evm-signer` (M3b)

- **Tracker note:** Moved to M3b (2026-09-30). Setup is a Hyperliquid sub-account funded with $100 USDC and an API wallet approved for that sub-account only: it signs orders via `vaultAddress` and cannot withdraw, so a live loss is capped at the deposit. Testnet first.
- **Have:** Nothing (no [hyperliquid] config, no keys).
- **Need:** (1) A dedicated HL master account, never the operator's main wallet. (2) Testnet faucet funds, then a mainnet USDC deposit sized by risk. (3) approveAgent for a fresh API wallet per deployment; its key goes in a 0600 file outside every fs root. (4) Optional approveBuilderFee. Record the addresses in the sandbox TOML comments. Reads need no account.
- **No-Rust path:** Operator action; no code.
- **Evidence:** rg -n -i 'hyperliquid' sandboxes src: none.

### `risk-evm-signer` — secp256k1 / EIP-712 signer port + key file + generalised signing-sandbox rules

M3b · M · rust · partial · ports + outbound + config · PRD §30 §28 §29
After: `risk-gate-enforcement` (M0)

- **Tracker note:** Moved to M3b (2026-09-30) for the Hyperliquid live pilot.
- **Have:** alloy 1.7.3 `full` is a dependency (Cargo.toml:43). Cargo.lock has alloy-signer-local (k256 0.13.4) and alloy-sol-types, but src uses only primitives and dyn-abi (src/adapters/outbound/tools/crypto/helpers.rs:6-7). `full` does not enable alloy's `eip712` feature (registry alloy-1.7.3 Cargo.toml [features]). The signer port and rules are Solana-only (src/ports/solana_signer.rs:9-13, src/config/solana.rs:79-223). EVM signing today is Privy remote only: sign_and_send_transaction, and sign_message = EIP-191 personal_sign.
- **Need:** ports: src/ports/evm_signer.rs `EvmSigner` with address() and sign_hash(B256) -> Signature (r, s, v).
  outbound: src/adapters/outbound/evm/signer.rs `LocalEvmKey`, an alloy PrivateKeySigner loaded from [evm] signer_key_file (0600; errors never echo the key).
  config: src/config/signing.rs generalising config/solana.rs: no claude_code, no mcp, no shell, key outside fs roots, `wallets = ["0x…"]` grants only on a private agent.
  Cargo.toml: alloy features += "eip712" (a feature flag, not a new crate).
  Tests: EIP-712 digest and signature against goldens produced outside the repo (Rust-only rule), stored as tests/fixtures/evm/eip712/*.json.
- **No-Rust path:** None. Privy sign_message is EIP-191, not EIP-712 over an msgpack hash, and Privy signing sits outside the risk gate.
- **Evidence:** rg -n 'alloy' src: primitives / dyn_abi only. Read ~/.cargo/registry alloy-1.7.3 Cargo.toml [features]: full = consensus, eips, essentials, k256, kzg, network, provider-ws, provider-ipc, …, pubsub, rlp (no eip712). rg -n -i 'eip-?712|typed_data' src: no hits.
- **Absorbed `hl-evm-signer`** — HL signing: secp256k1 key file, msgpack action hash, EIP-712 phantom agent, user-signed actions. (1) ports: src/ports/evm_signer.rs with address() and sign_hash([u8;32]) -> r, s, v. (2) outbound: src/adapters/outbound/hyperliquid/signer.rs, an alloy PrivateKeySigner over [hyperliquid] signer_key_file (0600, no-echo errors), mirroring src/adapters/outbound/solana/signer.rs:26. (3) domain: src/domain/hl/{msgpack.rs,action.rs,sign.rs}. Order-preserving msgpack encoder (map, array, str, bool, uint, int, nil). action_hash = keccak(msgpack(action) || nonce u64 BE || 0x00, or 0x01 || vault 20 bytes || optional 0x00 || expiresAfter u64 BE). Phantom agent {source a (mainnet) or b (testnet), connectionId}, signed under EIP-712 domain {name Exchange, version 1, chainId 1337, verifyingContract 0x0000000000000000000000000000000000000000}. User-signed actions (approveAgent, approveBuilderFee, usdClassTransfer) use HyperliquidSignTransaction with signatureChainId (the SDK uses 0x66eee). float_to_wire: at most 8 decimals, normalised, no trailing zeros. Lowercase addresses. Asset ids as in hl-universe-tool. (4) Goldens produced outside the repo with the Python SDK, committed as tests/fixtures/hyperliquid/signing/golden.json. (5) Config: src/config/hyperliquid.rs mirroring src/config/solana.rs:10-17 (no claude_code, MCP or shell; key outside fs roots; grants only on a non-routable agent).

### `risk-exec-runner-generic` — Venue-agnostic exec runner (paper / simulate / send) + write-coordination store (lease, pending record, fence)

M3b · M · rust · partial · ports + application + outbound · PRD §30 §28
After: `risk-gate-enforcement` (M0), `risk-paper-tools` (M0)

- **Tracker note:** Moved to M3b (2026-09-30): the HL client needs its lease, pending record and fence.
- **Have:** run_write / simulate / send are Solana-typed (src/adapters/outbound/tools/solana/write_common.rs:198-329). SolanaWriteStore keys lease / pending / fence by Solana wallet (src/ports/solana_writes.rs:11-35, writes_store.rs:18-25). Resource strings are already generic `wallet:<address>` (src/domain/solana_write.rs:28-31).
- **Need:** ports: src/ports/venue.rs `VenueWriter` {build(intent) -> Plan, simulate(plan), send(plan, signer) -> Report, confirm}; src/ports/write_coord.rs for lease / pending / fence / nonce keyed <venue>:<account>.
  application: src/application/exec.rs dispatches by mode:
  - paper: the fill engine.
  - simulate: venue validation + paper fill.
  - send: gate -> lease -> resolve pending -> send once -> confirm -> fence.
  Every call returns one exec/1:<venue>:<account>:<client_order_id> observation. Migrate SolanaWriteStore onto the generic store.
  Tests: a fake venue writer.
- **No-Rust path:** none
- **Evidence:** Read write_common.rs, ports/solana_writes.rs, outbound/solana/writes_store.rs.

### `risk-hl-exchange` — HL exchange client: msgpack + phantom-agent signing, nonces, API wallet, orders for the sub-account via `vaultAddress`, exchange-side TP / SL on every entry (`normalTpsl`), `hl_order` / `hl_cancel`

M3b · L · rust · missing · domain + outbound + outbound/tools · PRD §30 §20 §28
After: `hl-book-tool` (M0), `hl-ctx-tool` (M0), `kg-live-approval` (M3b), `risk-evm-signer` (M3b), `risk-exec-runner-generic` (M3b)

- **Tracker note:** Moved to M3b (2026-09-30). Orders go to the $100 sub-account via `vaultAddress`; every entry carries exchange-side TP / SL (`trigger` with `tpsl`, grouping `normalTpsl`), so stops hold while the process is down.
- **Have:** rg 'eip-?712|typed_data|msgpack|rmp|phantom|nonce' finds only the AES-GCM nonces of the secrets vault (src/adapters/outbound/secrets.rs:27, :44). No msgpack crate in Cargo.lock. Reusable: post_json (src/adapters/outbound/solana/http_json.rs:36-44), the WriteResult / refusal shape (src/domain/solana_write.rs), the lease / pending / fence store (src/ports/solana_writes.rs).
- **Need:** domain: src/domain/xm/hl_wire.rs.
  - Order wire {a, b, p, s, r, t: {limit: {tif: Alo|Ioc|Gtc}}, c?}; action {type: "order", orders, grouping: "na"}; float_to_wire (8 decimals, normalised).
  - Asset ids: perp = meta index; spot = 10000 + index; HIP-3 = 100000 + perp_dex_index x 10000 + index_in_meta (xyz:TSLA = 110001 from the probes).
  - An msgpack encoder: add rmp-serde, or write ~150 lines for maps / str / bool / int / array in SDK field order.
  - action_hash = keccak(msgpack(action) || nonce u64 BE || 0x00 | 0x01 || vault || [0x00 || expires_after u64 BE]).
  - Phantom agent {source: "a" mainnet / "b" testnet, connectionId: hash} signed under EIP-712 domain {name "Exchange", version "1", chainId 1337, verifyingContract 0x0000000000000000000000000000000000000000}.
  - User-signed actions use domain "HyperliquidSignTransaction" with signatureChainId 0x66eee.
  outbound: src/adapters/outbound/hl/exchange.rs.
  - POST https://api.hyperliquid.xyz/exchange through the egress tool_client.
  - Nonce manager: ms timestamps, strictly increasing per signer across processes via a nonces row in the write store, inside the window (T - 2 d, T + 1 d); the venue keeps the 100 highest per signer.
  - cloid = client_order_id; a retry checks orderStatus by cloid.
  - scheduleCancel as a dead-man switch.
  - Response statuses resting / filled / error map to WriteStatus.
  tools: hl_order and hl_cancel. mode = simulate (default) runs the gate and a paper fill; send needs a signer grant and LIVE-APPROVED. API wallet only: the operator approves it from the master wallet outside Tengu, and it cannot withdraw.
  Tests: goldens generated OUTSIDE the repo with hyperliquid-python-sdk signing.py (commit only JSON); testnet send smoke test marked #[ignore].
- **No-Rust path:** None. An [[mcp_servers]] HL server is refused in a signing sandbox (config/solana.rs:117-123 pattern) and would bypass the gate. Privy cannot sign L1 actions.
- **Evidence:** rg -n -i 'eip-?712|typed_data|msgpack|rmp|phantom|nonce' src: vault nonces only. rg '^name = "(rmp|rmp-serde|msgpack)"' Cargo.lock: absent. WebFetch of the HL SDK signing.py (2026-09-29) confirms domain, phantom agent and 0x66eee.
- **Absorbed `hl-order-write-tool`** — hl_order / hl_cancel / hl_positions: HL exchange writes with mode simulate | send. tools: src/adapters/outbound/tools/hyperliquid/{write_order.rs,positions.rs}. (1) hl_order: mode simulate = a local book-walk fill + fees, because HL has no dry-run endpoint; mode send = POST https://api.hyperliquid.xyz/exchange {action, nonce, signature, vaultAddress?, expiresAfter?}. IOC limit with a slippage cap, reduce-only close, notional cap, per-agent wallets grant, optional builder {b, f} (needs approveBuilderFee), subaccounts via vaultAddress. (2) hl_cancel by cloid. (3) hl_positions read: clearinghouseState with dex, weight 2. Result write/1:hl_order:<address>:<started_ms> (generalise WriteResult). Testnet first: https://api.hyperliquid-testnet.xyz.
- **Absorbed `hl-nonce-lease-store`** — HL write store: nonces, per-account lease, pending order record (cloid), fence. port src/ports/hl_writes.rs + outbound src/adapters/outbound/hyperliquid/writes_store.rs, stored in <TENGU_HOME>/state/hl-writes.db. (1) Per-signer nonce = max(now_ms, last + 1), persisted across processes. HL keeps the 100 highest nonces per signer; a new nonce must be above the smallest stored one and inside (T - 2 d, T + 1 d). (2) Per-account lease. (3) Pending record keyed by cloid, resolved via orderStatus before the next send. (4) Fence so reads after a fill are not stale. (5) One API (agent) wallet per process or subaccount, as HL recommends.

### `kg-live-approval` — LIVE-APPROVED: operator-only (TTY), expiring (30 d), separate live caps (≤ the $100 budget); ships its own `tengu xm approve`

M3b · S · rust · missing · inbound + application · PRD §28 §29 §30
After: `kg-lifecycle` (M1), `kg-review-cli` (M5), `risk-gate-domain` (M0), `risk-gate-enforcement` (M0)

- **Tracker note:** Moved to M3b (live pilot, after an M3 go): ships its own `tengu xm approve` (proposal review stays in `kg-review-cli`, M5); live caps ≤ the $100 budget. Operator decision 2026-09-30: no jurisdiction check or legal gate; LIVE-APPROVED stays operator-only (TTY), expiring and with separate live caps. Ignore the jurisdiction flag in the research notes below.
- **Have:** nothing. The Solana write tools gate on per-agent wallet grants (src/config/solana.rs), but there is no per-instrument live approval.
- **Need:** `tengu xm approve <instrument_id> --expires 30d` requires:
  - PAPER_TRADABLE for ≥ N days
  - per-instrument risk limits present in the risk config
  - venue allow-listed
  - operator jurisdiction flag (RH stock tokens restricted in US, CA, UK, CH per docs)
  Also `tengu xm revoke`; automatic expiry; any demotion clears approval. The risk gate requires LiveApproved + unexpired for live (`mode = "send"`) actions. Audit row with actor, reason and the limits snapshot.
- **No-Rust path:** none. A security control must be hard runtime enforcement, not TOML or a prompt.
- **Evidence:** rg -n -i 'LIVE_APPROVED|live_approved|approve_asset' src/ → 0; docs.robinhood.com/chain/stock-tokens (restricted jurisdictions).
- **Absorbed `risk-live-approval`** — Live mode switch: LIVE-APPROVED instruments, separate live caps, optional human approval. config: [risk] mode = "live" requires a [risk.live] block (smaller caps, max_live_notional_usd, approved_by), a configured signer, dry_run = false, and mode = "send" on exec actions.
  gate: live orders need lifecycle live_approved (operator CLI only).
  Optional: a human approval hook that wires tool_approvals for exec tools through the Telegram inline keyboard; a timeout denies.
  Tests: config refusals; the gate denies live orders without approval.

### `jev-live-sandbox-split` — Decide whether live orders need a separate signing sandbox; xmarket already meets the signing rules (convention 12, incl. hardened `claude_code`), so the default is the same sandbox with a private executor agent

M3b · S · research · missing · n/a · PRD §26 §28 §29
After: `risk-evm-signer` (M3b)

- **Tracker note:** Moved to M3b (2026-09-30). Default answer: the same xmarket sandbox with a private executor agent, since convention 12 already applies the signing rules (no shell, hardened `claude_code` only, no `[[mcp_servers]]`).
- **Have:** Signing-sandbox rules exist only for Solana (src/config/solana.rs:10-17,99-139,185-199). A claude_code agent is refused whenever [solana] signer_key_file is set, as are [[mcp_servers]] and shell scopes. Because the architect must have a description to run as run-agent (src/adapters/inbound/cli/run_agent.rs:156-172), it can never hold a wallet grant.
- **Need:** Decide and document one of: (a) an `xmarket-live` sandbox / process with Jev loops and OpenRouter agents only, consuming research and candidate rows from the research sandbox (shared observation store path or a loop webhook); (b) one sandbox with an OpenRouter architect (allowed by the rule) and no Claude Code. Generalize the signing rules of src/config/solana.rs to any venue signer (Hyperliquid API wallet, Robinhood Chain key); hand-off to the risk / hl / rh slices. Note: the jev-exec run_command resume path is impossible in a signing sandbox.
- **No-Rust path:** Decision + docs first; code follows in risk / hl / rh slices.
- **Evidence:** read src/config/solana.rs:10-17,99-139,185-199; rg -n "description" src/adapters/inbound/cli/run_agent.rs -> :156-172


## M4 — information layer

### `info-config` — `[feeds.<n>] news = {…}` source metadata (tier, org, reliability prior, mapping) + `[news.filter]`, `[news.dedup]`, `[news.extract]`

M4 · M · rust · missing · config · PRD §15 §16 §18 §35 S3

- **Tracker note:** `[feeds.<n>]` is the only transport / schedule table; news metadata goes in its `news = {…}` sub-table. No `[news.hosts]`: per-host limits use `[rate_limits.<name>]` (conventions 6, 14).
- **Have:** Nothing. The Config struct (src/config/mod.rs:124) has runtime_profile, hub, agents, orchestrator, memory, telegram, webhooks, decision_loops, scaffold, claude_code, default_scopes, egress, mcp_servers, solana and skill_lifecycle, but no news or feeds section.
- **Need:** src/config/news.rs, NewsConfig (deny_unknown_fields). [[news.sources]] rows: {name, kind = rss|atom|json|tg_preview|edgar_current|x_stream|x_search|webhook, url with {since}/{symbols} templating, method, body, tier = primary|professional|fast|social, org (independence key), poll_secs, headers {..$ENV}, user_agent_env, json mapping, max_items, reliability_prior, enabled}. Also [news.filter] (keyword rules and thresholds), [news.dedup] {simhash_max_hamming = 3, same_event_cos = 0.88, ambiguous_cos = 0.80, window_h = 48}, [news.extract] {model, batch, max_item_chars, skill} and [news.hosts] (per-host rps). Validation: unique names, https, poll_secs ≥ host minimum, kind-specific fields. Config.news + Default; config.example.toml; docs/code-map.md + .html.
- **No-Rust path:** Once this lands, every new RSS/Atom/JSON source is one TOML row (doctrine #2). Until then, every source has to be written in code.
- **Evidence:** rg -i '[news|NewsConfig|news_sources|[[news' → 0; rg -i 'feeds|FeedConfig' → 6 prose hits (e.g. domain/decision.rs:3 'feeds back'). Read src/config/mod.rs:124-192 and code-map §3.

### `info-store` — Canonical event store: SQLite `events.db` behind `NewsStore` (`UNIQUE(source, source_item_id)`, simhash64, 48 h cosine)

M4 · L · rust · missing · ports + outbound + bootstrap · PRD §16 §17 §18 §32 §35 S3
After: `kg-catalog-store` (M1)

- **Tracker note:** Store is SQLite `<TENGU_HOME>/state/xmarket/events.db` behind `NewsStore`, not Postgres (tracker convention 4).
- **Have:** agentic_memory tables (agentic_memory/mod.rs:535-577) with HNSW/FTS (:579-593) exist, but: memory_sources.hash is not unique (:554), a connection opens per call (:503-512), recall has no time window (:760-827, :868-925), output is text only (:491), and the claims/links tables never landed. The observation store keeps the latest row per key with a 7-day purge and no listing (ports/observation.rs:11-37, observations.rs:21). A pgvector/pgvector:pg16 service exists (docker-compose.yml:55-56).
- **Need:** src/ports/news.rs NewsStore: upsert_mentions ON CONFLICT (source, source_item_id) → new ids; near_dups(simhash, since); nearest_events(embedding, entity_ids, since, k); create_event; attach(mention, event, role = origin|confirmation|repost|update|correction); set_state(event, to, cause, rule); pending(stage, limit); record_extraction; source_stats. src/adapters/outbound/news/pg_store.rs: one client per process, advisory-lock DDL once as in agentic_memory :496-523. Tables: xm_sources; xm_mentions (UNIQUE(source, source_item_id), canonical_url_hash, content_sha256, simhash64 bigint, embedding vector(1536), published_at, fetched_at, vendor_tickers text[], author, org, repost_of, cites_org, gate/triage cols, event_id, role); xm_events (event_type, info_state, confidence, independent_sources, mention_count, first_seen_at, official_at, status, merged_into, embedding); xm_event_entities (entity_id → kg, role, impact direct|strong|possible|speculative, confidence, method dict|llm); xm_event_transitions; xm_extractions (model, tokens, cost, raw JSON). Indexes: btree(published_at), hnsw(embedding), GIN(vendor_tickers). Retention job; feature postgres_memory; [news] database_url_env (default TENGU_MEMORY_DATABASE_URL).
- **No-Rust path:** agentic_memory capture (kind = news_item, metadata jsonb) works for a demo only: no uniqueness, windowed queries, event links or states, and its implementation doc calls it a reference spike.
- **Evidence:** rg -i 'xm_events|xm_items|xm_mentions|news_store|NewsStore' → 0; rg -i 'canonical_event|event_store|EventStore' → 0. Read agentic_memory/mod.rs, ports/observation.rs, observations.rs, docs/agentic-memory-implementation-2026-05-13.md.

### `info-dedup` — URL canonicalisation, SimHash, windowed embedding clustering

M4 · M · rust · missing · domain + application · PRD §17 §18 §35 S3
After: `info-extract` (M4), `info-store` (M4)

- **Have:** No similarity or URL code. A sha256 helper is private to agentic_memory (mod.rs:1059-1061). The Embedding port with embed_batch exists (ports/memory.rs:93-96, embedder.rs:73), as does the pgvector 1536-dim guard (agentic_memory/mod.rs:1063-1069). aho-corasick, siphasher and unicode-normalization are only in Cargo.lock.
- **Need:** src/domain/news/novelty.rs (pure): canonical_url (lowercase host, drop fragment and utm_*/fbclid/gclid/mc_cid/ref/cmpid, amp and m. hosts, twitter.com → x.com, /status/<id>, EDGAR accession → id), normalize_text (NFKC, lowercase, collapse whitespace), simhash64 over word 3-shingles (siphasher), hamming. src/application/news/cluster.rs steps: exact (source, id) match → URL hash within 30 d → SimHash ≤ 3 within 48 h means repost/duplicate → cosine ≥ 0.88 within 48 h AND entity overlap AND compatible type means attach → 0.80-0.88 is ambiguous (Jev noul via info-jev-gate, otherwise a new event) → otherwise a new event. Labelled pair fixtures to tune [news.dedup]; merge/split repair via info-cli.
- **No-Rust path:** None: hashing and windowed similarity have to be code. The thresholds live in [news.dedup] TOML.
- **Evidence:** rg -i 'simhash|minhash|near_dup|canonical_url' → 0; rg -i 'hamming|shingle|jaccard' → 0. Cargo.lock: aho-corasick 1.1.4, siphasher 1.0.3, unicode-normalization 0.1.25.

### `info-prefilter` — Entity / alias dictionary + keyword / form rules (no LLM)

M4 · M · rust · missing · domain + application · PRD §3 §6 §15 §35 S3
After: `info-config` (M4), `kg-catalog-store` (M1)

- **Have:** No ticker, CIK or alias code (the only rg hits are serde alias attributes in config). regex is a dependency (Cargo.toml:41) and aho-corasick 1.1.4 is in Cargo.lock. SEC company_tickers.json has 10,431 ticker/CIK/name rows (probe).
- **Need:** src/domain/news/filter.rs (pure): Aho-Corasick over aliases (company names, $TICKER, (NASDAQ: X) patterns, token symbols with ambiguity guards, CIKs) → candidate entity ids + spans. Keyword rules per §16 type from [news.filter]. EDGAR form/item allow-list: drop NPORT-P (1,348), 424B2 (884) and 497K (254) of the 5,088 filings on 2026-09-28. Tier prior; score + reasons. src/application/news/triage.rs runs the drop/keep stage. The dictionary loads from the kg registry, seeded from company_tickers.json and venue symbols. Only 3 of 20 PR Newswire items carry an exchange:ticker (probe), so most wire items drop before any model call.
- **No-Rust path:** Keywords, thresholds and allow-lists go in TOML. The matcher has to be code; an LLM call per raw item defeats the cost goal.
- **Evidence:** rg -i 'aho_corasick|AhoCorasick|entity_dict|alias_dict' → 0; rg -i 'ticker|cik|company_tickers' src → 0. Probed company_tickers.json and PRN RSS.

### `info-taxonomy-skill` — SKILL.md: §16 taxonomy, §17 states, §7 levels, extraction schema + evals

M4 · S · skill · missing · skill · PRD §7 §16 §17 §35 S3

- **Have:** Nothing. skills/ contains german-teacher, orchestration-e2e, orchestrator, privy-agentic-wallets, skill-creator, skill-eval, spanish-teacher and telegram-rag-ingest; the last turns user-shared resources into vector memory and does not read channels or news.
- **Need:** skills/xmarket-event-extraction/SKILL.md with name/description frontmatter. Content: definitions + 2 examples per §16 category (extensible other:<label>); §17 rules (official = issuer/regulator/exchange own channel; confirmed = ≥ 2 independent credible orgs; credible_report = professional outlet or named sourcing; claim; rumor; opinion; analysis; duplicate; repost; correction); §7 levels for relations stated in text; stance per entity; the rule 'names as written, never invent tickers'; multilingual input (Upbit Korean titles); a JSON schema block the Rust extractor loads verbatim. evals/prompts.yaml fixtures from today's probes (EDGAR 8-K items, Binance/Bybit/Upbit listings, SEC/CFTC press titles, Hyperliquid TG posts) for tengu eval.
- **No-Rust path:** This is the no-Rust part: edits to the prompt, taxonomy and schema need no rebuild.
- **Evidence:** ls skills/; rg -il 'edgar|prnewswire|rss' skills → 0; read head of skills/telegram-rag-ingest/SKILL.md.

### `info-extract` — Batched LLM extraction (strict JSON schema): type, entities, claims, info state, stance

M4 · L · rust · partial · domain + ports + outbound + application · PRD §6 §7 §16 §17 §35 S3
After: `info-config` (M4), `info-store` (M4), `info-taxonomy-skill` (M4), `kg-catalog-store` (M1)

- **Tracker note:** `event_type` is a `String` validated at runtime against the SKILL.md list (convention 7); Rust enums only for info state and impact level. Concurrency / rate / timeout caps for this stage live in `info-pipeline`.
- **Have:** agentic_memory/mod.rs:1228-1275 has a private one-shot OpenRouter chat call with no response_format or JSON schema; its model comes from TENGU_WIKI_COMPILER_MODEL (:1220-1223). MetricsKind has no extraction kind (domain/metrics.rs:88-102). The engines are chat/tool loops, not batch extractors. OpenRouter lists structured_outputs for google/gemini-2.5-flash-lite, deepseek/deepseek-v4-flash, openai/gpt-5-nano and anthropic/claude-haiku-4.5 (probe).
- **Need:** src/domain/news/taxonomy.rs: EventType per §16 + Other(String), InfoState per §17, Impact per §7, Stance, and Extraction with validation. src/ports/extract.rs: Extractor::extract_batch. src/adapters/outbound/llm_json.rs: generalized chat_complete with response_format {type: json_schema, strict}, egress llm_api_client, usage → cost, new MetricsKind::Extraction. src/application/news/extract.rs: 8-10 items per call; per-item retry on schema failure; fail-soft (the mention stays pending); entity names resolved against the kg registry, unresolved ones become DISCOVERED candidates (§29). The prompt and schema are read from skills/xmarket-event-extraction/SKILL.md. [news.extract]: model = google/gemini-2.5-flash-lite, batch, max_item_chars = 2000, daily_usd_cap. Tool news_extract (key news_extract/1:<started_ms>, ttl 0).
- **No-Rust path:** An [agents.news_extractor] agent (openrouter, cheap model) with the SKILL.md, dispatched by the planner per item, works for demos and escalations. But it costs a planner call and a run-agent subprocess per item, with no schema enforcement and no batching: roughly 10-50x the cost and seconds per item.
- **Evidence:** rg -i 'event_type|EventType|info_state|InfoState' → 0; rg -i 'json_schema|response_format|structured_output' → 0. Read agentic_memory chat_complete; OpenRouter /api/v1/models probe.

### `info-pipeline` — Use case: poll → parse → store → prefilter → extract (concurrency / rate / timeout caps) → cluster → emit; EDGAR Ex-99.1 text

M4 · M · rust · missing · application + ports + bootstrap · PRD §3 §13 §35 S3
After: `info-dedup` (M4), `info-extract` (M4), `info-fetch` (M0), `info-parsers` (M0), `info-prefilter` (M4), `info-store` (M4), `rt-scheduler` (M0)

- **Tracker note:** Absorbs `rt-llm-stage`, narrowed to extraction concurrency / rate / timeout caps around the `Extractor` port — no agent turn per item, no first-JSON-object parsing (review). Also fetches EDGAR Ex-99.1 text (moved from `info-edgar`).
- **Have:** No interval or scheduler in src. tengu decide is one-shot (cli/decide.rs:17-52), the webhook listener is push-only (webhooks.rs:1-35), and observe() does cache-or-fetch per call (application/observe.rs:16).
- **Need:** src/application/news/pipeline.rs: NewsPipeline::tick(source) -> TickReport runs fetch → parse → upsert_mentions → prefilter → [gate] → batch extract → cluster → evolve → emit, with a backpressure queue (tier priority) and a news_feed/1:<source> observation per source. EventSink port in src/ports/news.rs: in-process broadcast + xm_event/1:<event_id> row + optional POST to a webhook loop endpoint for kg/jev consumers. src/bootstrap/news.rs builds the store, fetcher, extractor, embedder (ports::memory::Embedding) and DecisionEngine. Runs under rt's scheduler.
- **No-Rust path:** As a stopgap, OS cron can call tengu news poll --sandbox xmarket --once (one process start per tick, no in-process backpressure). The stage logic itself has to be code.
- **Evidence:** rg -n 'interval(|time::sleep' src → only retries/UI sleeps; rg -i 'cron|tick_secs|poll_secs|schedule' → only skill-scanner patterns; rg -i 'tick|every_|period_secs|poll_interval' → tests + solana send poll. Cargo.lock: no tokio-cron-scheduler/cron.
- **Absorbed `rt-llm-stage`** — Bounded LLM worker stage (info.raw -> extraction agent -> info.event) with concurrency, timeout and rate caps. config: `[stages.<n>] input = 'info.raw', agent = 'extractor', prompt_template, output = 'info.event', concurrency = 2, timeout_secs = 30, max_per_min = 30, json_required = ['event_type', 'entities']`. application: src/application/runtime/stage.rs — Semaphore + timeout; parse the first JSON object, validate required keys, publish to `output` with the item's trace/correlation; failures are audited and dropped. bootstrap: reuse the single-agent turn moved to src/bootstrap/escalation.rs (`run_agent_turn`). Metrics via the existing MetricsKind::Subagent records.

### `info-evolution` — rumor → credible → confirmed → official; independent confirmations counted by originating org

M4 · M · rust · missing · domain + application · PRD §17 §18 §24
After: `info-dedup` (M4), `info-extract` (M4), `info-store` (M4)

- **Have:** Nothing. The rg hits for 'rumor|confirmation|repost' are Solana tx confirmations and Privy skill text.
- **Need:** src/domain/news/evolution.rs (pure). States: official|confirmed|credible_report|claim|rumor|opinion|analysis|correction|denied|retracted. Independence key = originating org (the source row's org, an X author→org map, and extracted cites_org). Reposts, quotes, syndicated SimHash dups and 'according to <org>' cites of an already-counted org never increment the count. Official = a primary-tier org matching the subject (issuer, regulator, exchange). Correction and denial paths. Confidence = log-odds update weighted by source reliability. src/application/news/evolve.rs runs on each attach and writes xm_event_transitions (cause mention, rule id), refreshes xm_event/1 and signals the EventSink. [news.evolution] confirmed_min_independent = 2. Property tests: reposts never raise the count; results are order-independent.
- **No-Rust path:** None: the counting must be deterministic (§25). Thresholds go in TOML.
- **Evidence:** rg -i 'rumor|confirmation|independent_sources|repost' → 73 hits, all Solana tx confirmation (domain/solana_write.rs, outbound/solana/send.rs) or Privy skill text.

### `info-tools` — `news_*` typed tools + observation keys for `world` / `requires`

M4 · M · rust · missing · outbound (tools) + domain · PRD §22 §23 §25 §35 S3
After: `info-pipeline` (M4), `info-store` (M4), `jev-event-templating` (M0)

- **Have:** The typed tool pattern exists: domain/observation.rs:154, :17; application/observe.rs:16; tools/solana/defs.rs + mod.rs. Catalog row at tools/mod.rs:54-65; opt-in names at domain/tools.rs:28-50. http_request and agentic_memory return text only (request.rs:485-504, agentic_memory/mod.rs:491).
- **Need:** src/adapters/outbound/tools/news/{mod,defs,poll,items,extract,event,entity,sources}.rs + one NewsPlugin catalog row + names in domain/tools.rs. Tools: news_poll (source*, max_age_secs; key news_feed/1:<source>; features new, fetched, http_status, last_ok_age_s, error_class); news_items (pending/watch list, mark triage); news_extract (item_ids or pending N, ttl 0); news_event (key xm_event/1:<event_id>; features event_type, info_state, confidence, indep_n, mentions, age_s, first_tier, max_src_rel, direct_entities_n, stance); news_entity (key xm_entity_news/1:<entity_id>; features events_1h, events_24h, latest_age_s, top_type, net_stance; data = event ids); news_sources (health + reliability). Scope check on the first line of each tool (fs_roots = workspace, net_hosts, env_reads). Update docs/typed-observations, docs/tools.md and the code map.
- **No-Rust path:** None for typed rows Jev can read; text tools cannot feed world/requires.
- **Evidence:** rg -i 'news_|NewsItem' src → 0 relevant. Read domain/tools.rs, tools/mod.rs, domain/observation.rs, tools/solana/mod.rs.

### `info-sources-mvp` — Source rows + ingest scopes: EDGAR, SEC / CFTC / Fed / ECB, Federal Register, PR Newswire, exchange announcements, Nasdaq halts (shared row)

M4 · S · toml · missing · sandbox + config · PRD §15 §35 S3
After: `info-config` (M4), `info-edgar` (M0), `info-fetch` (M0), `info-parsers` (M0), `x-accounts-secrets` (M2)

- **Tracker note:** Source rows are `[feeds.<n>]` entries with a `news` sub-table (convention 6); the Nasdaq halts row is shared with `rh-halts`.
- **Have:** There is no sandboxes/xmarket (ls sandboxes: jev-exec, lping, storage-test, tor-check, unlimited). Every MVP endpoint returned 200 on 2026-09-29 (probes).
- **Need:** sandboxes/xmarket/config.toml: [egress] network = open. [agents.news_ingest] (no description, tools = news_*) with scopes: net_hosts www.sec.gov, data.sec.gov, efts.sec.gov, www.cftc.gov, www.federalreserve.gov, www.ecb.europa.eu, www.federalregister.gov, www.prnewswire.com, www.binance.com, www.okx.com, api.bybit.com, api-manager.upbit.com, t.me, www.nasdaqtrader.com, api.llama.fi, hub.snapshot.org, gov.uniswap.org, rekt.news, www.youtube.com; env_reads SEC_USER_AGENT. [[news.sources]] rows: edgar_current_{8k,6k,sc13d,425,s1}, sec_press, cftc_press, fed_press, ecb_press, fedreg_fin (agencies SEC/CFTC/Fed/Treasury), prn_all (poll 60 s), binance_listings (catalogId 48), binance_delist (161), okx_announcements (unfiltered: the new-listings annType's newest item is 2026-07-10), bybit_new, upbit_trade, hl_announcements + hl_api (t.me/s), nasdaq_halts, llama_hacks, snapshot_props (POST body), uniswap_gov, rekt, youtube_coinbureau (UCqK_GSMbpiV8spgD3ZGloSw). Each row gets a tier and an org.
- **No-Rust path:** This gap is TOML-only once info-config, info-parsers and info-fetch exist.
- **Evidence:** ls sandboxes/; rg -i 'prnewswire|federalregister|binance|okx|bybit|upbit|t.me/s' sandboxes → 0. Probe list.

### `info-cli` — `tengu news poll / tail / event / sources / merge / split / backfill`

M4 · S · rust · missing · inbound · PRD §32 §35 S3
After: `info-pipeline` (M4)

- **Have:** Commands has Decide (cli/mod.rs:74, :483) but no news commands.
- **Need:** src/adapters/inbound/cli/news.rs + a Commands::News variant: tengu news poll --sandbox xmarket [--source <name>] [--once]; tengu news tail --sandbox xmarket [--min-state credible_report]; tengu news event --sandbox xmarket <event_id>; tengu news sources --sandbox xmarket; tengu news merge|split <event_id>; tengu news backfill --sandbox xmarket --source edgar_current_8k --from 2026-09-01 --to 2026-09-28. A TUI System bubble for new or changed events, like decision_loop::render_audit.
- **No-Rust path:** None for an operator CLI; OS cron + --once serves as the stopgap scheduler.
- **Evidence:** rg -i 'Commands::News|news poll|tengu news' → 0. Read cli/decide.rs and cli/mod.rs:74.

### `info-webhook-sink` — Webhook → news store sink for push vendors

M4 · S · rust · partial · inbound + config · PRD §15 §35 S3
After: `info-config` (M4), `info-store` (M4)

- **Have:** Webhook endpoints route to the planner or a Jev loop (config/mod.rs:633-664; webhooks.rs:1-35); payloads are never stored as items.
- **Need:** WebhookEndpointConfig.news_source = <[[news.sources]] name> (kind webhook, json mapping) → NewsStore::upsert_mentions → pipeline. Mutually exclusive with loop/agent; validation; update docs/webhooks-2026-05-11.md.
- **No-Rust path:** loop = news_triage alone labels payloads without persisting or deduplicating them.
- **Evidence:** rg -i 'sink\s*=|WebhookSink|news_sink' → 2 hits, TUI cb_sink only.

### `info-alpaca-news` — Pro-news proxy: Alpaca News (Benzinga) source row + account

M4 · S · account · missing · sandbox + config · PRD §15 §35 S3
After: `info-config` (M4), `kg-catalog-store` (M1), `x-accounts-secrets` (M2)

- **Have:** Nothing (rg 'alpaca|benzinga|APCA_' → 0). The host answers 401 without keys (probe).
- **Need:** An Alpaca account with APCA_API_KEY_ID/APCA_API_SECRET_KEY in the vault. [[news.sources]] alpaca_news: kind json; url https://data.alpaca.markets/v1beta1/news?symbols={symbols}&start={since}&limit=50&sort=desc; headers {APCA-API-KEY-ID = $APCA_API_KEY_ID, APCA-API-SECRET-KEY = $APCA_API_SECRET_KEY}; mapping {items=/news, id=/id, title=/headline, url=/url, published=/created_at, body=/summary, tickers=/symbols}; tier professional; org benzinga; poll 15 s (free plan allows 200 calls/min). Symbol batches come from the kg universe. Upgrade path: Massive /benzinga/v2/news ($99/mo add-on). The wss://stream.data.alpaca.markets/v1beta1/news websocket comes later and needs rt's websocket client.
- **No-Rust path:** Pure TOML after info-config (json kind + $ENV headers). REST polling adds up to 15 s of latency compared with the websocket.
- **Evidence:** rg -i 'alpaca|benzinga|APCA_' → 0; curl data.alpaca.markets/v1beta1/news → 401; alpaca.markets/data fetched 2026-09-29.

### `info-x-ingest` — X pay-per-use filtered stream + recent search, rule sync, reposts, read cap

M4 · L · rust · missing · outbound + application + config · PRD §3 §14 §15 §18 §34 §35 S3
After: `info-pipeline` (M4), `ops-cost-guard` (M4), `rt-http-stream` (M4), `x-accounts-secrets` (M2)

- **Have:** Nothing (rg 'api.x.com|twitter|tweet' → 0). The reqwest stream feature is on (Cargo.toml:20), but the egress tool_client sets a total timeout (egress.rs:333-340), which kills a long-lived stream.
- **Need:** src/adapters/outbound/news/x.rs. Stream: GET https://api.x.com/2/tweets/search/stream (NDJSON + keep-alives, reconnect with backoff, 1 connection, ≤ 1,000 rules × 1,024 chars). Rule sync: POST https://api.x.com/2/tweets/search/stream/rules from [news.x] rules and account lists, with diff and dry-run. Recent search: GET https://api.x.com/2/tweets/search/recent (since_id, ≥ 10 results, 450 requests/15 min). referenced_tweets → repost_of/quote_of; author → org map. Read meter at $0.005 per post (billed once per UTC day) with a hard x_reads_per_day cap that pauses the stream. Auth via $X_BEARER_TOKEN. src/application/news/x_ingest.rs feeds the same pipeline.
- **No-Rust path:** (a) A skill + http_request with auth_bearer_env = X_BEARER_TOKEN for recent search: architect and reverse discovery only. (b) SocialData user monitors (about $4.49 per account per month) → [webhooks.endpoints.x] + info-webhook-sink; needs public HTTPS ingress and carries vendor ToS risk. (c) twitterapi.io websocket ($0.15/1k tweets; the vendor says it is not affiliated with X Corp).
- **Evidence:** rg -i 'api.x.com|twitter|tweet' → 0; rg -i 'filtered.stream|search/recent|X_BEARER' → 0. Fetched docs.x.com pricing, filtered-stream and search pages; curl api.x.com recent search → 401.

### `kg-resolve-entity` — `xm_resolve_entity`: mention → canonical entity; unknown-asset discovery

M4 · M · rust · missing · domain + outbound(tool) · PRD §4 §6 §13 §18 §26 §29 §35 S3 §35 S4
After: `kg-catalog-store` (M1), `kg-sync-us-reference` (M1)

- **Have:** nothing
- **Need:** Domain src/domain/xmarket/resolve.rs:
  - normalization: cashtags ($TSLA), tickers, names, former names (SEC formerNames: TESLA MOTORS INC), HL keywords (anthropic → hl:io:ANTH)
  - scoring: ext-id 1.0 > ticker+exchange 0.95 > alias 0.9 > FTS5 name 0.6–0.85 > keyword 0.7; ambiguity margin
  On a miss with external = true: OpenFIGI /v3/search, GLEIF fuzzycompletions + lei-records, Wikidata wbsearchentities, local CoinGecko list ⇒ new entity in DISCOVERED with provenance and needs_research = true (escalation hook for the architect).
  Tool `xm_resolve_entity {mention*, kind_hint, context, external = false}` → `xm_resolve/1:<normalized mention>` (TTL 3600 s hit / 300 s miss). data.candidates[{entity_id, name, kind, listing, score}]; features top_score, margin, n_candidates, status resolved | ambiguous | unknown.
  P0 minimum: exact ticker/cashtag/alias; external lookups can follow.
- **No-Rust path:** Partial: the architect skill (kg-architect-skill) resolves unknowns via http_request on the slow path (§27). Every event on the fast path needs deterministic local resolution.
- **Evidence:** rg -n -i 'resolve_entity|entity_resol|ner|cashtag' src/ → 0; live probes GLEIF (Anthropic 984500B6DEB8CEBC4Z70), Wikidata (Q116758847), OpenFIGI, CoinGecko ('eth' → ≥ 10 ids ⇒ symbol-only resolution is unsafe).

### `rt-http-stream` — NDJSON / SSE streams through egress (X filtered stream, SSE news)

M4 · M · rust · missing · domain | outbound | config · PRD §14 §15 §35 S3
After: `rt-backoff-budget` (M0), `rt-daemon` (M0), `rt-dedup-state` (M2)

- **Have:** No SSE or NDJSON consumer: engines call with `stream: false` (src/adapters/outbound/engines/openrouter.rs:291, local.rs:228), and the Claude CLI stream-json is subprocess stdout (claude_code.rs:538). The reqwest `stream` feature is on (Cargo.toml:20), so Response::bytes_stream (reqwest-0.12.28 async_impl/response.rs:356) and ClientBuilder::read_timeout (client.rs:1453) exist. But egress `tool_client` sets a total timeout (egress.rs:333-340) that would cut any stream.
- **Need:** egress: `EgressPolicy::stream_client(connect_timeout, read_timeout)` (proxy, redirects off, tcp_keepalive, no total timeout) in src/adapters/outbound/egress.rs. domain: src/domain/feed/framing.rs (pure) — NDJSON line framer (\r\n keep-alives) and SSE parser (event/data/id/retry, blank-line dispatch, Last-Event-ID). outbound: src/adapters/outbound/feeds/http_stream.rs — connect; staleness timer (stale_secs = 20 for X keep-alives); reconnect via rt-backoff-budget, including X's 'maximum allowed connection limit' 429 on fast reconnects; single-connection guard (runtime lease); connection-level audit (tool = 'stream', host, path, connect_ms, items, bytes, close reason); max_items_per_day spend cap (X bills $0.005 per post read). config: [feeds.<n>] kind = 'stream', format = 'ndjson'|'sse', url, auth_env, stale_secs, max_items_per_day, topic.
- **No-Rust path:** An external streamer as [[mcp_servers]] exposing get_new_items, polled by a tick loop: an extra process, egress only advisory for stdio MCP (docs/egress-2026-09-16.md:41), text results, and MCP cannot push (mcp_client/client.rs:193-218 skips notifications). X webhooks into `tengu webhooks` need rt-webhook-signatures + public HTTPS.
- **Evidence:** rg 'event-stream|eventsource|event_source|\bsse\b|bytes_stream|chunk\(\)\.await|lines_stream' src and rg 'strip_prefix\("data|\[DONE\]' src -> no stream consumer; rg '"stream"|stream: (true|false)|stream-json' engines -> stream: false; reqwest source read in ~/.cargo registry.

### `ops-metrics-cost` — Cost in `MetricsRecord` (OpenRouter, Jev, Claude Code) + persisted metrics

M4 · S · rust · partial · domain | outbound | application | inbound · PRD §27 §35 S3

- **Have:** MetricsRecord has tokens and latency but no cost (src/domain/metrics.rs:42-83). The OpenRouter engine parses prompt/completion tokens only (src/adapters/outbound/engines/openrouter.rs:63, 217-220). Jev cost sits in DecisionUsage.cost (src/domain/decision.rs:75-84) and in the audit, but not in metrics (mod.rs:400-422). Claude Code total_cost_usd is only logged (src/adapters/outbound/engines/claude_code.rs:350-365). The sink is tracing plus an in-process broadcast, not persisted (src/application/metrics.rs:52-89).
- **Need:** domain/metrics.rs: cost_usd: Option<f64> with #[serde(default, skip_serializing_if = Option::is_none)], which keeps it compatible across IPC. engines/openrouter.rs reads usage.cost, which OpenRouter now always returns. decision_loop record_metrics fills it from d.usage.cost; also fill it for Claude Code and the embedder. A persistence subscriber in application/metrics.rs writes <TENGU_HOME>/state/spend.db (day, kind, agent, model, calls, tokens, cost), shared across processes; run-agent children already re-emit their records through IPC. New `tengu metrics today`.
- **No-Rust path:** partial — OpenRouter GET /api/v1/key usage_daily gives an account-level daily total, and jq over decisions.jsonl gives usage.cost. Trade-off: no per-loop or per-agent split, and LLM spend is invisible in-process.
- **Evidence:** rg -n -i 'daily_usd|BudgetGuard|spend\.db|cost_usd' src → only claude_code.rs:350-360 (logged); rg -n 'usage|include' src/adapters/outbound/engines/openrouter.rs → tokens only

### `ops-cost-guard` — `[spend]` guard over `spend.db`: per-kind caps (LLM, X reads, paid search), degrade order

M4 · M · rust · partial · config | ports | application | outbound | bootstrap · PRD §27 §28
After: `ops-alerts-telegram` (M5), `ops-metrics-cost` (M4)

- **Tracker note:** Config table is `[spend]` (not `[budget]`), convention 6.
- **Have:** limits.max_cost_per_flow and warn_at_cost are parsed and validated (src/config/mod.rs:443-445, 1211-1223) but read nowhere else. Decision loops and engines have no budget check.
- **Need:** config/budget.rs: [budget] daily_usd, warn_at = 0.8, per_kind = { decision, subagent, planner, embedding }, on_exceed = 'stop_acting'. ports/budget.rs: BudgetGuard { check(kind) -> Allow | Warn | Deny, add(kind, cost_usd) }, backed by spend.db from ops-metrics-cost and shared across processes. DecisionLoop checks before decide() and audits outcome 'budget_exceeded'; bootstrap wires the guard in front of engine calls; a Notifier alert fires at warn and at deny. Wire or delete max_cost_per_flow / warn_at_cost.
- **No-Rust path:** partial — ops-openrouter-budget-key provides the hard cap. Trade-off: no per-loop split and no graceful stop (calls simply start failing).
- **Evidence:** rg -n 'max_cost_per_flow|warn_at_cost' src → only src/config/mod.rs; rg -n -i 'budget' src/application/decision_loop → none
- **Absorbed `info-budget`** — Info cost meter + daily budget guard (LLM, Jev, X reads, paid search). [news.budget] {llm_usd_per_day = 2, x_reads_per_day = 3000, search_usd_per_day = 2}. A daily counters table (xm_budget_daily) updated from usage. Degrade order: pause the X stream → turn off paid search → extract only primary-tier items. Observation news_budget/1:<yyyy-mm-dd>; spend shown in tengu news sources.

### `x-scheduled-event-calendar` — Macro releases, FOMC, earnings, token unlocks ⇒ pre-event monitors + risk blackout

M4 · M · rust · missing · outbound | domain | config · PRD §16 §17 §21 §28 §35 S3
After: `info-fetch` (M0), `info-parsers` (M0), `risk-gate-domain` (M0), `rt-scheduler` (M0)

- **Have:** Only after-the-fact press RSS is planned (info-sources-mvp: Fed, ECB, SEC, CFTC). There is no release schedule, no actual-vs-prior values, no earnings dates, no unlock schedule, and no risk rule around known high-volatility times. The info report lists token-unlock sources as external (unverified) but gives them no gap.
- **Need:** Outbound: src/adapters/outbound/news/sources/calendar.rs with these sources: BLS iCalendar https://www.bls.gov/schedule/news_release/bls.ics (parsed by a small hand-rolled ICS parser); FOMC dates as a yearly TOML list copied from the federalreserve.gov calendar page; earnings dates from Finnhub /calendar/earnings (free key) or Alpaca/Benzinga; token unlocks from Tokenomist or DefiLlama (licensing still open). Domain: sched_event/1:<id> rows {kind, entity_ids, at_ms, importance, source}. Runtime: rt-scheduler `kind = 'tick'` rows fire at T-10 min (event `sched.pre`, which watches the affected instruments) and at T0 (`sched.at`). Released values (BLS API v2, FRED with a key) attach as official-fact mentions. Risk: knob `blackout_secs = { macro_high = 120 }` denies new entries inside the window unless the strategy is event-driven.
- **No-Rust path:** Partial — FOMC/CPI/NFP dates as a yearly TOML list plus rt-scheduler tick rows need no new Rust; earnings and unlock feeds and the blackout rule need code
- **Evidence:** WebFetch https://www.bls.gov/schedule/news_release/bls.ics (2026-09-29): iCalendar with Employment Situation 2026-12-04 08:30 ET, CPI 2026-12-10 08:30 ET, PPI 2026-12-15 08:30 ET; rg -i 'fomc|nonfarm|cpi|bls.gov|economic.?calendar|earnings.?calendar' src sandboxes skills → only an Anchor 'events CPI' comment in src/domain/solana.rs:206


## M5 — event ↔ asset, Jev classification, slow path

### `kg-find-instruments` — `xm_find_instruments`: venue discovery + reverse lookup

M5 · S · rust · missing · outbound(tool) + application · PRD §3 §4 §8 §14 §19 §23 §35 S4
After: `kg-calendars` (M1), `kg-equivalence` (M1), `kg-lifecycle` (M1)

- **Have:** nothing
- **Need:** src/adapters/outbound/tools/xmarket/find.rs. Args: entity_id | instrument_id (reverse lookup for market-first discovery, §14), min_state (observed), include_proxies (false), venues. Returns the class members: venue, symbol verbatim, kind, ratio, relation, lifecycle, status, session_open_now, reference_open, max_leverage, liquidity snapshot (24h notional, OI). Observation `xm_instruments/1:<entity_id>`, TTL 300 s. Features (≤ 32): n_instruments, n_paper_tradable, has_hl, has_rh, has_reference, reference_open. data.instruments[] ranked by lifecycle, then liquidity, for FromHistory slots with value = "instrument_id". First line of execute: ctx.scope.check_fs_write(workspace). Callers: loop agent, researcher, architect (read-only).
- **No-Rust path:** none. It must read the catalog DB and emit typed rows for slots/world; MCP or http_request give text only.
- **Evidence:** rg -n -i 'instrument|universe' src/adapters/outbound/tools/ → Solana-only; docs/typed-observations-2026-09-24.md tool table has no catalog tools.

### `kg-graph-edges` — Relationship graph: seed load, propose tools, statuses, 1–2 hop queries

M5 · M · rust · missing · application + outbound(tool) · PRD §5 §6 §7 §11 §24 §35 S4
After: `kg-catalog-store` (M1), `kg-seed-data` (M1)

- **Have:** nothing. agentic_memory has no claims/links tables (docs/SESSION_HANDOFF.md:280).
- **Need:** src/application/xmarket/graph.rs:
  - idempotent seed load (status accepted, provenance seed)
  - related(entity, hops ≤ 2, min_strength, statuses); path strength = weakest link
  Tools:
  - `xm_propose_edge {from*, to*, kind*, strength* (≤ strong, never direct), role, evidence* [{url, quote}]}`
  - `xm_propose_mapping {instrument_id*, entity_id*, relation* same|proxy, ratio, evidence*}`
  Both write status = proposed plus an edge_events row; the architect/researcher may call them.
  Use rules: accepted edges feed trade-candidate ranking; proposed/speculative edges feed only research actions (investigate, request_confirmation). §24: Jev never creates an edge.
- **No-Rust path:** Partial: curated edges live in TOML seeds (git review = human review). The architect could propose patches to the seed file (proposal-only, like agentic-memory behaviour patches). Trade-off: no DB status, and slow review for slow-path discoveries.
- **Evidence:** rg -n -i 'propose_edge|propose_mapping|review_queue|approve_asset|operator_approval' src/ → 0; rg -n 'memory_links' src/ → 0.

### `kg-related-assets` — `xm_related_assets` + per-event impact set `xm_impact/1:<event_key>` (top-N, provenance)

M5 · M · rust · missing · domain + outbound(tool) · PRD §5 §6 §7 §11 §12 §21 §23 §24 §32 §35 S4
After: `info-store` (M4), `kg-confirm-reaction` (M5), `kg-find-instruments` (M5), `kg-graph-edges` (M5)

- **Have:** nothing
- **Need:** Pure src/domain/xmarket/impact.rs:
  - impact confidence = path strength ∧ event info_state (info slice) ∧ confirmation
  - rank = strength weight × (0.5 + 0.5 × confirm_score) × liquidity factor
  - speculative excluded unless research = true
  Tool `xm_related_assets {entity_id*, event_id, max_hops 1..2, min_strength possible, status accepted, top ≤ 8}` → `xm_related/1:<entity_id>`. With event_id it also writes `xm_focus/1:<event_id>` (direct + related instruments ranked) and persists `event_impacts` rows. Features: n_direct, n_related, best_confirm, max_abs_move_bps, reference_open. Every candidate carries edge_id, provenance and an evidence ref, so decisions.jsonl answers 'why related'. The ≤ 8 cap reflects Jev's 32k context (docs/lping-2026-09-24.md:61,86) and 300-char descriptions (slots.rs:19).
- **No-Rust path:** none. Ranking and path strength are deterministic policy (§24, §25); an LLM ranking would re-introduce invented relationships.
- **Evidence:** rg -n -i 'relationship|impact|related_assets' src/ → 0 relevant; read src/application/decision_loop/slots.rs:19,140-171 (description budget).

### `kg-confirm-reaction` — `xm_confirm_reaction`: measured event-window reactions per instrument (§12)

M5 · M · rust · missing · domain + outbound(tool) · PRD §12 §14 §20 §23 §25 §35 S4
After: `hl-info-client` (M0), `info-store` (M4), `kg-calendars` (M1), `kg-catalog-store` (M1), `ops-history-recorder` (M1)

- **Have:** No return/volume/OI statistics in src. The only price history is the ≤ 6-min sample ring in price_oracle rows (src/domain/lp/market.rs:57,255).
- **Need:** Pure src/domain/xmarket/confirm.rs:
  - window returns (bps)
  - abnormal return vs benchmark (beta from pre-window bars; beta = 1 when < N bars)
  - volume ratio vs trailing baseline; OI change %; funding delta (HL)
  - quality flags: stale oracle, delisted, reference_closed
  - confirmation score in [0,1]
  Bars: HL candleSnapshot (works for HIP-3, e.g. io:ANTH 5m) via hl slice; reference and RH prices via rt / rh slices.
  Tool `xm_confirm_reaction {event_id*, event_ts_ms*, instruments* (≤ 8), windows_s [300, 1800, 7200], benchmark}` → `xm_reaction/1:<event_id>:<instrument_id>` (TTL 60 s while the window is open, 7 d after). Appends `reactions` rows. Failed bar reads are Field::Error, never 0.
- **No-Rust path:** none. §25 puts this arithmetic in deterministic code, not in Jev or an LLM.
- **Evidence:** rg -n -i 'lead_lag|leadlag|abnormal_ret|beta\b|pearson|correlation|cross_corr|rolling_beta' src/ → 0 relevant hits; live probe candleSnapshot io:ANTH 5m → 37 bars.

### `kg-import-public-graph` — Ownership / investor / sector edges from Wikidata, GLEIF Level 2, SEC SIC

M5 · M · rust · missing · outbound + application + inbound · PRD §5 §11 §35 S4
After: `kg-graph-edges` (M5), `kg-sync-us-reference` (M1)

- **Have:** nothing
- **Need:** src/adapters/outbound/xmarket/{wikidata,gleif}.rs + importer:
  - Wikidata SPARQL: P1951 investor, P127 owned by, P749 parent organization, P355 subsidiary, P452 industry. QID ↔ entity via P1278 LEI / P5531 CIK / P946 ISIN or the resolver. UA required; ≤ 5 parallel; 60 s processing/min.
  - GLEIF lei-records + direct-parent / ultimate-parent.
  - SEC submissions sic → sector_member (auto-accepted, provenance sec_sic).
  Wikidata/GLEIF edges land as proposed with strength possible unless allow-listed.
  CLI: `tengu xm import --source wikidata|gleif|sec --entities <ids>`.
- **No-Rust path:** Partial: the architect skill can run the same SPARQL for one entity and call xm_propose_edge. That is fine for the slow path; bulk import needs Rust.
- **Evidence:** rg -n -i 'wikidata|gleif|sparql|lei\b' src/ → 0; live probes: Wikidata P1951 for Q116758847 → Q3884 Amazon, Q941127 Salesforce, Q20800404 Alphabet Inc.; GLEIF record 984500B6DEB8CEBC4Z70 has direct-parent / ultimate-parent relationships; SEC submissions sic 3711 for CIK0001318605.

### `kg-review-cli` — Operator review of proposed edges / mappings; suspend / resume (TTY only, no LLM surface)

M5 · S · rust · missing · inbound · PRD §24 §26 §29 §35 S4
After: `kg-graph-edges` (M5), `kg-lifecycle` (M1), `kg-xm-cli` (M1)

- **Tracker note:** Operator commands refuse without a TTY (`is_terminal()`), since the existing prompt accepts piped input.
- **Have:** No approval mechanism exists: TelegramConfig.tool_approvals is parsed but never read (src/config/mod.rs:579-581; docs/SESSION_HANDOFF.md:27).
- **Need:** Commands, all run by the operator (actor = `operator:<$USER>`) and written to edge_events / lifecycle_events:
  - `tengu xm review` (proposed mappings/edges with evidence and full ids)
  - `tengu xm accept|reject <proposal_id> [--strength strong|possible]`
  - `tengu xm suspend|resume <instrument_id> --reason`
  No LLM-callable tool exposes accept or suspend; that is the hard enforcement. A Telegram approval UI can come later.
- **No-Rust path:** Partial: edits to the git-reviewed seed TOML + `tengu xm seed` can accept edges. They cannot handle lifecycle suspend/resume or mapping proposals quickly.
- **Evidence:** rg -n -i 'approve|review' src/adapters/inbound/cli/ → skill-evolve approval only (application/skills/lifecycle/approval_gate.rs); rg -n 'tool_approvals' src/ → config only.

### `x-etf-index-membership` — ETF + index constituent edges (sector ETFs, index perps, SPY / QQQ tokens)

M5 · M · rust · missing · outbound | application | inbound · PRD §5 §6 §10 §19 §35 S4
After: `info-edgar` (M0), `kg-graph-edges` (M5), `kg-sync-us-reference` (M1)

- **Have:** Planned edge sources are SEC SIC (sector_member), Wikidata/GLEIF ownership and curated seeds (kg-graph-edges, kg-import-public-graph, kg-seed-data). Nothing links an ETF or index to its holdings, yet venues list them: hyperliquid:xyz:SMH, hyperliquid:xyz:SOXL, hyperliquid:xyz:XLE, hyperliquid:xyz:SP500, hyperliquid:xyz:XYZ100, hyperliquid:mkts:US500, hyperliquid:mkts:USTECH, and the RH SPY and QQQ tokens. info-prefilter drops NPORT-P filings, the public source of fund holdings.
- **Need:** Importer `tengu xm import --source nport|issuer --funds <tickers>`. Sources: SEC N-PORT-P holdings for the listed ETFs (public quarter-end filings via EDGAR, with the declared User-Agent), plus issuer daily holdings files where the ToS allow. Index constituents come from the tracking ETF (SPY → S&P 500, QQQ → Nasdaq-100). Writes `constituent` edges {weight, as_of, provenance nport|issuer} into the catalog. kg-related-assets ranks by weight × relation strength (§6 'semiconductor ETFs', §10 'energy ETFs'). An edge with as_of older than 120 days is downgraded to possible.
- **No-Rust path:** Partial — hand-curated TOML seeds for the top ~10 weights of each listed ETF or index (git-reviewed) are enough for P1 research; full holdings need the importer
- **Evidence:** rg -i 'constituent|holdings|etf_member|index_member' src → no relevant hits; the hl probe lists these ETF/index markets as listed; the info probe counted 1,348 NPORT-P filings on 2026-09-28, which the planned allow-list drops (N-PORT holdings availability not probed)

### `x-crypto-relationship-seeds` — Chain ↔ token, LST / LRT ↔ underlying, protocol ↔ token (DefiLlama, CoinGecko)

M5 · M · rust · missing · outbound | application · PRD §5 §9 §35 S4
After: `kg-graph-edges` (M5), `kg-sync-crypto` (M1)

- **Have:** kg-sync-crypto maps only CoinGecko ids and HL tokens. The relationship importers cover companies only (Wikidata, GLEIF, SEC SIC), and the seeds list crypto majors without edges. §9 (Ethereum staking → liquid-staking tokens and DeFi protocols) has no source.
- **Need:** Importer over https://api.llama.fi/protocols (keyless; 8,416 protocols with category, chains, gecko_id, parentProtocol) plus CoinGecko coin categories. Edges: protocol token → chain (derivative/ecosystem); LST/LRT → underlying (category 'Liquid Staking' or 'Liquid Restaking'); grouping by parentProtocol. New edges are proposed, strength possible, provenance defillama; the top N are accepted through TOML seeds. Weekly refresh; the payload is 9 MB, so parse it as a stream.
- **No-Rust path:** Partial — TOML seeds for the ETH, SOL and BTC ecosystems (stETH, rETH, JitoSOL, cbBTC) cover MVP research; the long tail needs the importer
- **Evidence:** curl https://api.llama.fi/protocols (2026-09-29) → HTTP 200, 9,000,478 B, 8,416 entries; Lido = {category 'Liquid Staking', chains 5, gecko_id lido-dao, symbol LDO}; rg -i 'lido|steth|llama.fi' src → 0

### `rh-macro-ref` — `macro_quote`: FX, DXY proxy, oil / gold / index proxies for §10 events

M5 · M · rust · partial · domain+outbound+config · PRD §6 §10 §20 §35 S2 §35 S4
After: `kg-sync-hyperliquid` (M1), `rh-lighter` (M2), `rh-quote` (M2)

- **Have:** Only a text http_request action for daily ECB FX (Frankfurter) in sandboxes/jev-exec/config.toml:128-134.
- **Need:** Tool macro_quote, driven by a TOML source table [market_data.macro.<symbol>] (source + id).
  
  Sources:
  - Daily FX: https://api.frankfurter.dev/v1/latest?from=USD&to=EUR,JPY,GBP,CAD,SEK,CHF (keyless). Pure src/domain/macro_ref.rs::dxy_proxy applies the ICE weights (2026-09-29 gives 101.269).
  - Intraday FX and commodities: Twelve Data with $TWELVE_DATA_API_KEY. Basic is free (8 req/min, 800/day, real-time forex); commodities need Grow ($79/mo).
  - On-venue proxies, no new provider:
    - RH tokens USO / SLV / SPY / QQQ / SGOV via rh_quote.
    - Chainlink 'GLD / USD' 0x470A51258068043bd43dC0a56245625C9fE86eB0 and 'Robinhood USO / USD' 0x75a9c76Ef439e2C7c2E5a34Ab105EcFe3766431c.
    - Lighter RH perps XAU / XAG / USO.
    - HL HIP-3 commodity perps from the hl slice.
  
  Keys: macro_quote/1:<symbol> (e.g. macro_quote/1:DXY_PROXY, macro_quote/1:EURUSD), TTL set per source.
  
  CME futures (CL, GC, ES, DX) only via Databento Standard ($199/mo), later.
- **No-Rust path:** Mostly yes for daily data: http_request loop actions over Frankfurter, as in jev-exec. The DXY-proxy arithmetic and staleness checks need code (§25).
- **Evidence:** - rg -n -i 'dxy|wti|brent|xau|gold|commodit|forex|fx_rate' src sandboxes skills: only the jev-exec fx_rate action.
  - Probe: Frankfurter 2026-09-29 returned all six rates; DXY proxy 101.269.

### `rh-evm-read-tools` — `evm_call` / `evm_logs` for the architect (unknown tokens + contracts)

M5 · S · rust · missing · outbound · PRD §26 §29 §35 S1
After: `rh-evm-rpc` (M1)

- **Have:** Only abi_encode / hex_to_uint256 (src/adapters/outbound/tools/crypto/mod.rs:41-49).
- **Need:** Two tools in src/adapters/outbound/tools/evm/:
  - evm_call. Args: chain, to, signature (e.g. 'latestRoundData()(uint80,int256,uint256,uint256,uint80)'), args, block. Decoded with alloy dyn-abi into typed JSON. Key evm_call/1:<chain_id>:<to>:<keccak of calldata>, TTL 10 s.
  - evm_logs. Args: address, event signature, from/to block. Decoded, capped at 200 rows. Key evm_logs/1:<chain_id>:<address>:<topic0>:<from>:<to>.
  
  Scope: net_hosts = the chain's RPC host.
  
  This lets the slow-path architect inspect a new token (§26) without writing new Rust for each contract.
- **No-Rust path:** Partial today: http_request + abi_encode (flat args only) + hex_to_uint256. Multi-word returns and events have to be decoded by hand.
- **Evidence:** rg -n -i 'evm_call|evm_logs|abi_decode|decode_function_result' src: none.

### `info-news-search` — `news_search`: reverse-discovery fan-out for Jev's investigate action

M5 · M · rust · missing · ports + outbound + application + tools · PRD §3 §14 §22 §35 S4
After: `info-dedup` (M4), `info-extract` (M4), `info-tools` (M4), `rt-series-detectors` (M2)

- **Have:** Nothing.
- **Need:** SearchProvider port (src/ports/news.rs); src/adapters/outbound/news/search/{local,edgar_fts,alpaca,finnhub,gdelt,exa,brave,tavily,x_recent,sonar}.rs. src/application/news/discover.rs: parallel fan-out with per-provider timeout and cost budget; results become mentions and go through dedup/extract; precedes_move_s is measured against the anomaly start. Tool news_search (entity* | symbol*, since_ms*, until_ms, providers, max_cost_usd); key why_moving/1:<symbol>:<since_ms>; features candidates_n, best_conf, best_lead_s, official_found, cost_usd. [news.search] lists providers and key envs.
- **No-Rust path:** info-why-moving-skill (architect path), which is slower and untyped.
- **Evidence:** rg -i 'why_moving|reverse_discovery|news_search' → 0.

### `info-why-moving-skill` — Reverse-discovery skill for the architect ("why is X moving")

M5 · S · skill · missing · skill + sandbox · PRD §14 §26 §27
After: `x-accounts-secrets` (M2)

- **Have:** http_request supports headers, bearer/basic env auth and POST bodies (request.rs:28-78), with a per-host scope and egress gate (:108-118). The architect pattern exists (sandboxes/jev-exec/config.toml:41-87). No search skill in skills/.
- **Need:** skills/xmarket-why-moving/SKILL.md. Inputs: symbol/entity, venue, window, anomaly features. Ordered recipe with budgets: 1) local news_entity/news_event; 2) EDGAR full-text search https://efts.sec.gov/LATEST/search-index?q=…&dateRange=custom&startdt=…&enddt=… (UA header); 3) Alpaca/Finnhub news by symbol; 4) GDELT DOC (≤ 1 request per 5 s); 5) Exa POST https://api.exa.ai/search (category news), Brave https://api.search.brave.com/res/v1/news/search or Tavily POST https://api.tavily.com/search (topic news); 6) X recent search (budgeted); 7) perplexity/sonar via POST https://openrouter.ai/api/v1/chat/completions. Sonar lists no tools on OpenRouter, so it can only be an http_request target, never an agent engine. Output: JSON {candidates[{event, url, published_at, precedes_move_s, confidence}], none_found}. [agents.researcher] scopes: net_hosts + env_reads EXA_API_KEY, BRAVE_API_KEY, TAVILY_API_KEY, X_BEARER_TOKEN, APCA_API_KEY_ID, APCA_API_SECRET_KEY, FINNHUB_API_KEY, OPENROUTER_API_KEY, SEC_USER_AGENT.
- **No-Rust path:** This is the no-Rust path. Trade-offs: text-only results, 10-30k LLM tokens per investigation (about $0.07 on Sonnet), 20-90 s, and Jev cannot consume the output.
- **Evidence:** rg -i 'exa.ai|tavily|brave|perplexity|sonar' → 0; rg -i 'why_moving|reverse_discovery|news_search' → 0. OpenRouter models probe: perplexity/sonar supported_parameters has no tools.

### `info-reliability` — Source reliability + lead-time scoring

M5 · M · rust · missing · domain + application + outbound · PRD §24 §33 §34
After: `info-evolution` (M4)

- **Have:** Nothing.
- **Need:** src/domain/news/reliability.rs: Beta(α,β) per source with tier priors; success = a claim later confirmed or made official, failure = corrected or denied; lead_s = first mention − official_at, tracked per asset class. Table xm_source_stats; job tengu news score-sources; features max_src_rel and indep_n on xm_event/1; rows news_source/1:<name>; priors in [news.reliability].
- **No-Rust path:** Tier priors in TOML give a static reliability until outcomes accumulate. A weekly architect review through a skill would be uncalibrated.
- **Evidence:** rg -i 'reliability|source_score|lead_time' → 0.

### `info-jev-gate` — Jev in the pipeline: material? (`noul`), type / state choices, same event? (`noul`)

M5 · M · rust · partial · application + bootstrap · PRD §17 §18 §22 §34 §35 S5
After: `info-pipeline` (M4), `info-prefilter` (M4)

- **Have:** DecisionEngine::decide accepts any Question map (ports/decision.rs:17-26) and JevClient posts it unchanged (decisions.rs:48-50, :58-80). Noul/Score types exist with probed wire shapes (domain/decision.rs:5-8, :20-33). Loops ask Choice only (decision_loop/mod.rs:163, :178) and cannot pass an item id to a tool (slots.rs:109). About $0.00014 and 0.3-0.6 s per decision (decision-loop plan :11, :27).
- **Need:** src/application/news/gate.rs: one Jev call per surviving mention with questions material (noul), event_type (choice over §16 labels), info_state (choice over §17) and an optional subject (choice over dictionary candidates + none). material ≥ gate_at → extract; material < drop_at → drop; in between, extraction decides. A same-event noul handles the 0.80-0.88 dedup band. Audit lines (loop = news_gate) + xm_mentions.gate_* columns. [news.gate]: model = ~typesafe/jev-latest, sources, gate_at, drop_at. bootstrap/news.rs builds JevClient::from_env. Calibration log for §34.
- **No-Rust path:** A [decision_loops.news_triage] loop fed by a webhook loop endpoint can label items with Choice actions, but the label stays in loop history and the loop cannot call news_extract with the item id (slots.rs:109) until jev-event-slots lands.
- **Evidence:** rg -n 'Question::(Noul|Score|Choice)' src → the decision loop uses only Choice (mod.rs:163, :178). Read ports/decision.rs, outbound/decisions.rs, slots.rs, config/decision_loop.rs:128-161.

### `jev-action-kinds` — Tool-then-stop actions + an explicit, chosen `escalate` action

M5 · S · rust · partial · config | application · PRD §22 §27

- **Have:** An action either runs a tool and continues, or is terminal with no tool and no slots (src/config/decision_loop.rs:96-100,196-199; src/application/decision_loop/mod.rs:292-309). An event stops only on a non-Executed outcome (mod.rs:113-120). Escalation happens only when confidence < act_at (mod.rs:268-276), so Jev cannot choose 'escalate'. The gate also runs before the terminal branch, so an unsure `ignore` escalates too.
- **Need:** config ActionConfig: `stop = true` runs the tool, appends history and ends the event (ignore / monitor / reject / paper_enter bookkeeping without an extra Jev call). `escalate = true` calls the Escalator with reason `chosen` and returns `Escalated { chosen: true }`. Loop-level `escalate_on_terminal = false` skips escalation for unsure terminal picks. Validation: `escalate` needs the loop's escalation configured. application: branches in apply(). Tests: stop runs the tool once and stops; a chosen escalate passes full args and state.
- **No-Rust path:** Follow each state-writing tool with a `done` pick: one extra Jev call per event (0.3-0.6 s, about $0.0002). Explicit escalate as `http_request` POST to a planner webhook endpoint with auth_header_env works only with network = open (the tool client does not exempt loopback from the Tor proxy, src/adapters/outbound/egress.rs:332-339) and carries no event context. Workable but wasteful; Rust S recommended.
- **Evidence:** read mod.rs:113-120,268-309; config/decision_loop.rs:93-123,190-199; rg -n "escalate" src/config/decision_loop.rs -> only the loop-level bool (65-68)

### `jev-triage-state-tool` — `xm_mark`: durable ignore / monitor / reject / candidate state

M5 · M · rust · missing · domain | adapters/outbound (tool) | config (catalog) · PRD §17 §22 §29 §32
After: `info-store` (M4), `jev-event-templating` (M0)

- **Have:** A terminal pick lives only in in-process history and one audit line (src/application/decision_loop/mod.rs:292-309,424-464). No loop- or LLM-callable observation writer exists. The store's only writers are observe() (src/application/observe.rs:46), Solana accounts/plan (src/adapters/outbound/solana/accounts.rs:106; src/adapters/outbound/solana/plan.rs:399) and lp_state CAS (src/adapters/outbound/tools/solana/lp.rs:187,745).
- **Need:** domain/xmarket/triage.rs (pure): TriageState {new, ignored, monitoring, investigating, confirming, candidate, rejected, escalated, paper_open, closed}, the allowed transitions, and reason codes (duplicate, not_novel, no_tradable_asset, speculative_relation, no_market_confirmation, edge_below_costs, stale_data, low_liquidity, risk_refused). adapters/outbound/tools/xmarket/mark.rs: tool `xm_mark` {subject_kind: event|candidate, subject_id*, state*, reason, until_s}. It writes with put_if_unchanged (src/ports/observation.rs:21-31) to row `xm_triage/1:<subject_kind>:<subject_id>` (features: state, reason, since_ms, until_ms, transitions_n). An illegal transition returns a typed error. Add a catalog row, a WORKSPACE_TOOLS entry, and `ctx.scope.check_fs_write(workspace)` as the first line. Loop use: ignore / monitor / reject = xm_mark with read_only = true and stop = true; reason / until_s are static slots; ids come from `{event:/event_id}` or a FromHistory candidate.
- **No-Rust path:** shared_cache / persistent_store puts (these also need event templating for the key). Trade-off: world / requires read only the observation store (src/application/decision_loop/world.rs:50-86), so the rows are invisible to Jev and gating, and there is no transition validation. Rust required.
- **Evidence:** rg -n "store\.put\(|\.put\(&|put_if_unchanged\(" src/ -> only the writers cited; rg -c -i "xmarket|xm_" src/ sandboxes/ -> 0 matches

### `jev-classify-questions` — Config-declared `noul` / `score` / `choice` questions (today loops ask `choice` only)

M5 · M · rust · partial · domain | config | application · PRD §7 §16 §17 §27 §35 S5

- **Have:** The wire types exist and serialize to the probed shape: Question::{Choice,Score,Noul}, ScoreAnchor, Answer.{score,noul,probabilities,confidence} (src/domain/decision.rs:17-57, test 141-161), and JevClient sends any question map (src/adapters/outbound/decisions.rs:48-50). Gaps: the loop builds only Choice questions (src/application/decision_loop/mod.rs:159-189); config has no question surface; Answer has no `legend` field (decision.rs:43-57, silently dropped); Noul has no optional criteria (decision.rs:31-32).
- **Need:** config: `[decision_loops.<n>.classify.<key>] type = noul|score|choice, instructions, criteria`, asked once per event in its own call before step 0. Questions are evaluated independently, so a same-call answer cannot inform next_action (TypeSafe docs). application: store answers as `state.classification.<key>` = {choice | score | p, probabilities, confidence}, visible to later steps, logged in the audit, and usable as action gates: `when = { relevance = 0.7, info_state = ["official_fact","confirmed_reporting","credible_report"] }` (noul p >= x, choice in set, score >= x); otherwise the action is illegal. domain: add Answer.legend and optional Noul criteria. Fit: `relevance` noul = S5 relevance gate; `info_state` choice over the 10 §17 labels; `category` choice over §16 (<= 255 options); `impact_class` score with 4 anchors (0 speculative .. 3 direct, §7). impact_class is advisory only; the KG / architect stay authoritative (§24).
- **No-Rust path:** Fold classes into next_action labels (ignore_rumor, investigate_official, ...). Trade-off: combinatorial menu, no calibrated noul probability, no per-question answers for §33 learning. Rust required.
- **Evidence:** rg -n "Question::Score|Question::Noul|ScoreAnchor|\.noul|\.score\b|legend" src/ -> only src/domain/decision.rs (types + test at :150); read mod.rs:159-189

### `jev-context-composer` — `xm_event_context`: deterministic multi-asset × venue row, ≤ 8 candidates

M5 · L · rust · missing · domain | adapters/outbound (tool) · PRD §12 §20 §23 §24 §25 §35 S5
After: `hl-book-tool` (M0), `hl-ctx-tool` (M0), `info-store` (M4), `jev-event-templating` (M0), `kg-related-assets` (M5), `rh-dex-quote-v3` (M2), `rh-quote` (M2), `risk-calc-costs` (M0)

- **Have:** The pattern exists only for one wallet x pool: lp_snapshot composes many reads into one typed row, and hedge_decide / lp_decide are pure decisions over cached rows (docs/typed-observations-2026-09-24.md:53-55; src/adapters/outbound/tools/solana/lp.rs). World renders only decision_value (status / age / source / <= 32 features, no data: src/domain/observation.rs:344-366). data reaches Jev only through reducers into history or slot descriptions of at most 300 chars (src/application/decision_loop/slots.rs:17-19,140-171). Nothing composes an event's asset set across venues.
- **Need:** domain/xmarket/context.rs (pure, now_ms as input) takes the canonical event (info), the impact set with relation classes (kg), per-venue market rows (hl / rh / reference) and the cost model (risk). It builds a matrix assets x venues {ret_since_event_pct, basis_bps_vs_ref, spread_bps, depth_usd, funding_bps, oi_chg_pct, vol_ratio, age_s, status}. It also builds candidates[] {candidate_id, long/short instrument ids, gross_edge_bps, est_cost_bps, net_edge_bps, relation, max_size_usd}: direct / strong relations and fresh rows only, sorted by net edge; failed reads are Error fields, never 0. adapters/outbound/tools/xmarket/context.rs: tool `xm_event_context` {event_id*, max_age_secs} -> row `xm_ctx/1:<event_id>` (ttl 15 s). Features <= 32: n_assets, n_venues, n_fresh_rows, n_candidates, best_net_edge_bps, leader_venue, max_abs_ret_pct, data_age_max_s. data = matrix + candidates. Loop use: `build_context` (read_only, reduce `/data/candidates/*/{candidate_id,relation,net_edge_bps,est_cost_bps,max_size_usd}` so next_action sees the matrix) and paper_enter slot `candidate = { from = "build_context", items = "/candidates/*", value = "candidate_id" }`; an empty list makes the action illegal.
- **No-Rust path:** Per-venue http_request reads with reducers into history, letting Jev compare. Trade-off: breaks §25 (arithmetic outside deterministic code), and JEV-as-a-Judge (arXiv 2609.26550) finds Jev weakest where the verdict must be derived. Rust required.
- **Evidence:** rg -c -i "xmarket|xm_" src/ sandboxes/ -> 0; read world.rs:113-138, observation.rs:344-376, slots.rs:140-171

### `jev-gate-semantics` — Per-action `act_at`, review band; log p(chosen) beside Jev's normalised-peak confidence

M5 · S · rust · partial · domain | config | application · PRD §27 §28 §34

- **Have:** Gate = min over next_action and each multi-candidate slot of gate_confidence() = confidence if present, else p(chosen), else noul (src/domain/decision.rs:59-73; src/application/decision_loop/mod.rs:243-252,269). There is one act_at per loop (src/config/decision_loop.rs:55-58). Jev confidence = (n*p_max-1)/(n-1): the repo probe had p = 0.83 with n = 3, giving 0.745 against a reported 0.74 (decision.rs:166). With act_at 0.8 the implied p_max is 0.90 for 2 options, 0.867 for 3, 0.84 for 5 and 0.815 for 13, so the bar drifts with the legal-set size. Single-candidate slots are never asked (mod.rs:175,246-247).
- **Need:** config: `gate_on = probability | confidence` (default confidence for back-compat); a per-action act_at override (reads 0.6, state marks 0.7, paper_enter 0.9, close 0.6); a `review_below` band that returns Review (log + monitor, no escalation). domain: Answer::p_chosen(). application: gate on the configured signal; the audit logs both.
- **No-Rust path:** Put risky actions in their own loop with a higher act_at. Trade-off: the implied probability bar still moves with the option count, and there are more loops.
- **Evidence:** read decision.rs:59-73, mod.rs:243-276; formula from marktechpost.com 2026-09-23 checked against decision.rs:166; rg -n "act_at" src/config/decision_loop.rs -> a single loop-level field

### `jev-durable-history` — Per-lane history restored from `audit.db` after restarts / escalations

M5 · M · rust · missing · ports | adapters/outbound | application | bootstrap | config · PRD §27 §30 §32
After: `jev-event-key` (M0)

- **Tracker note:** Restore from `audit.db`; no `loops.db`; `lane` = configurable JSON pointer (convention 19).
- **Have:** LoopState {t, event_start, history: VecDeque} lives in process memory (src/application/decision_loop/mod.rs:70-79). The ring holds 8 entries (src/config/decision_loop.rs:52-54; mod.rs:370-375) and is 'lost on restart' (mod.rs:27). state.history carries earlier events' entries for unrelated assets (mod.rs:190-195; test mod.rs:965-967). `tengu decide` starts empty on every call (src/adapters/inbound/cli/decide.rs:38-39).
- **Need:** ports/loop_state.rs: LoopStateStore {load(loop, event_key), append(loop, event_key, entry), list_open(loop)}. adapters/outbound/loop_state.rs: SQLite `<workspace>/.tengu/loops.db` with loop_history(loop, event_key, t, entry_json, ts_ms) and loop_events(loop, event_key, session_id, status, first_seen_ms, last_step_ms, resumes); WAL; 7-day purge. config: `history_scope = event | loop` (default loop). application: load and save per event_key. bootstrap: open the store from the loop agent's workspace, like SqliteObservationStore (src/bootstrap/decision.rs:76-83).
- **No-Rust path:** none: history exists only in process memory.
- **Evidence:** rg -n -i "agentic|memory|recall|resume" src/application/decision_loop/ src/bootstrap/decision.rs src/ports/decision.rs -> none; read mod.rs:27,70-79,190-195,370-375
- **Absorbed `rt-loop-lanes-persist`** — Decision-loop lanes (per subject / correlation) + history restored after restart. application: DecisionLoop holds `Mutex<HashMap<lane, LoopState>>` with an LRU cap (max_lanes = 256). The lane is an event pointer from `[decision_loops.<n>] lane = '/subject'`; the default is a single lane, i.e. today's behaviour. Per-lane serialization, cross-lane parallelism bounded by rt-bus-dispatch. Restore on build: replay the last `history` audit lines per (sandbox, loop, lane) from decisions.jsonl into HistoryEntry (answers.next_action.choice, args, ok, output, obs), with event_start at the restored t so FromHistory never reuses them (mod.rs:73-77 semantics). The audit gains `sandbox`, `lane` and `ts_ms`.

### `jev-engine-failure-policy` — Jev call failure: retry, audit line, deterministic fallback

M5 · S · rust · missing · domain | application | config · PRD §28 §31

- **Have:** `self.engine.decide(...).await?` aborts the whole event (src/application/decision_loop/mod.rs:202,114), and the webhook task only logs a warning (src/adapters/inbound/webhooks.rs:261-263). JevClient maps any non-2xx response to an error (src/adapters/outbound/decisions.rs:71-79). There is no retry or backoff, and no audit line (see jev-decision-audit).
- **Need:** application: one retry with jittered backoff on 429 / 5xx / timeout, honouring Retry-After. After that, return outcome EngineFailed with an audit line and run the configured fallback `on_engine_error = stop | <action>`, a named action executed without Jev (e.g. xm_mark monitoring). domain: new StepOutcome variant. The metrics record carries the error.
- **No-Rust path:** none.
- **Evidence:** rg -n -i "retry|backoff|attempt" src/application/decision_loop/ src/adapters/outbound/decisions.rs -> none

### `jev-state-hygiene` — State budget + untrusted-text guard; loops with write actions see structured fields only

M5 · S · rust · partial · config | application | docs · PRD §15 §23 §24

- **Have:** Reducers project events and results (src/application/decision_loop/reduce.rs:21-31; event_reduce / reduce at src/config/decision_loop.rs:69-72,108-110), capped at MAX_ITEMS 20 and MAX_RAW_CHARS 2000 (reduce.rs:16-19). There is no total state budget. When event_reduce is empty, the raw event text enters the state (mod.rs:111,190-195). The escalation prompt embeds the full pretty-printed state (mod.rs:389-396).
- **Need:** config: `max_state_chars = 40000` (about 10-16k tokens), trimming oldest history results first, then stale world entries; still over budget is a hard error (never a silent cut of ids). Validation warning: a loop with non-read_only actions and an empty or string-projecting event_reduce. Docs: loops with write actions get structured fields only (codes, ids, counts, reliability); headline text only in read-only triage loops.
- **No-Rust path:** Mostly TOML: strict event_reduce / reduce projections plus a small history. Trade-off: no guard against a long string or an oversized world.
- **Evidence:** rg -n -i "max_state|token_budget|estimate_tokens|state_chars|max_chars" src/application/decision_loop/ src/config/decision_loop.rs -> none

### `jev-escalation-guard` — Per-loop escalation budget, single-flight per event, no escalation on unsure terminal picks

M5 · S · rust · missing · config | application · PRD §26 §27 §35 S5
After: `jev-event-key` (M0)

- **Have:** Every step below act_at, including terminal picks, spawns a full one-shot orchestrator turn (planner + subagents). There is no budget, no dedupe and no in-flight cap (src/application/decision_loop/mod.rs:268-276,377-398; src/adapters/inbound/webhooks.rs:332-388,639-656).
- **Need:** config `[decision_loops.<n>.escalation] max_per_hour = 20, once_per_event = true, max_in_flight = 2`. application: an hourly counter plus single-flight per event_key, made durable through row `xm_escalation/1:<event_key>` {status pending|done|failed|timeout, started_ms, agent} so a restart or a second process does not re-escalate. A suppressed escalation returns `EscalationSuppressed { reason }` plus an audit line and ends the event like `monitor`.
- **No-Rust path:** escalate = false, or a low act_at. Trade-off: no slow path (§27).
- **Evidence:** rg -n -i "budget|rate.?limit|max_escalat|per_hour|cooldown" src/application/decision_loop/ src/config/decision_loop.rs src/adapters/inbound/webhooks.rs -> none

### `jev-architect-config` — `xm_architect` (OpenRouter or hardened `claude_code` on the subscription) + skills: entity research, why-moving, research-submit contract

M5 · S · skill · missing · config | skill · PRD §26 §27
After: `info-tools` (M4), `jev-research-submit-tool` (M5), `kg-related-assets` (M5)

- **Tracker note:** One routable `xm_architect`, on OpenRouter or a hardened `claude_code` agent on the subscription (conventions 11, 12, 20); the result contract (`xm_submit_research`) is identical over the bridge.
- **Have:** Architect patterns: the jev-exec architect (claude_code, builtins none, run_command -> tengu only; sandboxes/jev-exec/config.toml:41-87) and the lping crypto_researcher (openrouter, description, typed read tools; sandboxes/lping/config.toml). Claude Code builtin profiles offer only Read/Glob/Grep/Edit/Write/MultiEdit/Bash, and the default is editor_shell (src/adapters/outbound/engines/claude_code.rs:29-55; src/config/mod.rs:780-782).
- **Need:** [agents.xm_architect]: engine openrouter, model anthropic/claude-sonnet-4-6 (repo-standard slug), a description (required by run-agent), and workspace = the loop workspace. tools = [http_request, read_file, <info / kg read tools>, xm_submit_research]: no paper / live execution tool and no xm_mark. Per-agent scopes: http_request net_hosts limited to news / primary / regulator hosts, explicit env_reads. limits: max_tool_rounds 30, step_timeout_secs 240. identity + skills/xm-architect/SKILL.md: the research procedure, the xm_submit_research schema, 'call xm_submit_research exactly once, then compress_and_store', never invent tickers, classify relations per §7.
- **No-Rust path:** Yes: TOML + SKILL.md only.
- **Evidence:** read sandboxes/jev-exec/config.toml:41-87; claude_code.rs:29-55
- **Absorbed `kg-architect-skill`** — Architect research skill for unknown entities, mappings and relationships. skills/xmarket-entity-research/SKILL.md. Trigger: an escalation with needs_research / mapping conflict / unknown instrument.
  Sources via http_request: SEC EDGAR full-text + submissions, company IR, GLEIF, Wikidata SPARQL, OpenFIGI search, HL perpAnnotation.
  Rules:
  - a URL + verbatim quote for every claim
  - output only through xm_propose_mapping / xm_propose_edge
  - never `direct`; cap `strong`
  - full ids always
  `[agents.architect]` tools: xm read tools + propose tools + http_request with scoped net_hosts.

### `jev-architect-escalator` — `Escalator` returns an escalation id; moves out of the `webhooks`-gated module into `src/bootstrap/`; direct dispatch, timeout, post-check, audit link

M5 · M · rust · partial · ports | bootstrap | config | inbound · PRD §26 §27
After: `jev-escalation-guard` (M5), `jev-event-key` (M0), `jev-research-submit-tool` (M5)

- **Tracker note:** `OrchestratorEscalator` / `run_one_shot` sit in the `webhooks`-gated module today; moving them to `src/bootstrap/` also removes that gate.
- **Have:** OrchestratorEscalator runs a full planner turn with a hard-coded, Solana-worded free-text prompt (src/application/decision_loop/mod.rs:389-396) and discards the result (src/adapters/inbound/webhooks.rs:639-656). Escalation is available only when [orchestrator] exists (webhooks.rs:126-131); `tengu decide` has no escalator (src/adapters/inbound/cli/decide.rs:38-39). SubprocessRunner can run one agent with a timeout (src/adapters/outbound/subprocess_runner.rs:178-247), but the child accepts only agents with a description (src/adapters/inbound/cli/run_agent.rs:156-172).
- **Need:** config `[decision_loops.<n>.escalation] agent = "xm_architect", timeout_secs = 240, goal = "<template with {event:/...}, {reason}, {state}>", result_key = "xm_research/1:{event:/event_id}", resume = true`. ports/decision.rs: Escalator::escalate(EscalationRequest {loop, event_key, session_id, reason, state, result_key}). bootstrap/decision.rs ArchitectEscalator: SubprocessRunner::run_with_timeout on the architect (same sandbox_config and session id), then a post-check that the result_key row exists with observed_at_ms >= start (ObservationStore::get). One retry with a stricter goal, then write xm_escalation/1:<event_key> {status ok|missing|failed|timeout, agent, ms} and call LoopDispatch::resume. OrchestratorEscalator stays as the fallback when escalation.agent is unset.
- **No-Rust path:** Partial: keep OrchestratorEscalator and let the planner route to xm_architect by its description, with a SKILL.md contract. Trade-off: an extra planner call per escalation; the planner may answer Direct or route elsewhere (parse_verdict prose fallback); no post-check and no resume.
- **Evidence:** read mod.rs:377-398, webhooks.rs:126-131,332-388,639-656, subprocess_runner.rs:178-247, run_agent.rs:156-172
- **Absorbed `ops-escalation-link`** — Link escalations (architect runs) to the decision that triggered them. ports/decision.rs: escalate(..) returns EscalationTicket { escalation_id } (esc-<uuid>). webhooks.rs run_one_shot records {escalation_id, decision_id, session_id, started_ms, ended_ms, final_text sha256, agentic_memory row id} via DecisionAudit.link(decision_id, 'escalated_to', escalation_id). The architect's structured result is stored as observation arch/1:<escalation_id> for the jev slice's resume step. Escalations raise an alert (ops-alerts-telegram).

### `jev-research-submit-tool` — `xm_submit_research`: validated result ⇒ `xm_research/1:<event_key>`

M5 · M · rust · missing · domain | adapters/outbound (tool) | config (catalog) · PRD §5 §7 §17 §24 §26 §29
After: `info-store` (M4), `kg-catalog-store` (M1)

- **Tracker note:** Result key `xm_research/1:<event_key>`; free text is never parsed (convention 11). `category` is a `String` validated at runtime against the SKILL.md list, not a §16 enum (convention 7).
- **Have:** The only completion contract is compress_and_store(summary: string) (src/adapters/outbound/tools/skill_lifecycle/compress_and_store.rs:22-42), intercepted in run-agent (src/adapters/inbound/cli/run_agent.rs:419-462). Claude Code subagents often skip it (CLAUDE.md:425-427). No tool writes a research observation.
- **Need:** domain/xmarket/research.rs: Research {event_id, entities[{entity_id, name, role}], assets[{asset_id, relation: direct|strong|possible|speculative, basis_code, evidence_urls[]}], venues[{venue, instrument_id}], info_state (§17 enum), category (§16 enum), recommendation: monitor|investigate|candidate|reject|ignore, horizon_mins, notes <= 500 chars}, with serde deny_unknown_fields. Features <= 32: n_direct, n_strong, n_possible, n_speculative, n_venues, info_state, recommendation, sources_n. adapters/outbound/tools/xmarket/research.rs: tool `xm_submit_research` validates instrument ids against the universe catalog; an unknown id is rejected by name, so the architect cannot mint tradable instruments (§29). It then puts xm_research/1:<event_id> (ttl 3600 s) into the loop workspace store. Add a catalog row, WORKSPACE_TOOLS, and [default_scopes.xm_submit_research] fs_roots = [workspace]. The architect's workspace must equal the loop agent's, because the store is <workspace>/.tengu/observations.db (src/bootstrap/decision.rs:57-61,76-83).
- **No-Rust path:** write_file a JSON file, or compress_and_store free text. Trade-off: the loop cannot read files or memory (world = observation store only, src/application/decision_loop/world.rs:50-86), and nothing is validated. Rust required.
- **Evidence:** rg -n "store\.put\(|put_if_unchanged\(" src/ -> no research writer; read compress_and_store.rs:22-42

### `jev-loop-resume` — Route the architect's result back into the originating loop + lane ("JEV resumes")

M5 · M · rust · missing · application | config · PRD §26 §27 §36
After: `jev-architect-escalator` (M5), `jev-durable-history` (M5), `jev-event-templating` (M0), `rt-bus-dispatch` (M2)

- **Tracker note:** Result key `xm_research/1:<event_key>`; escalation id in `xm_escalation/1:<event_key>` (convention 11).
- **Have:** Escalation stops the event (StepOutcome::Escalated, src/application/decision_loop/mod.rs:113-120,269-276). The Escalator port is fire-and-forget with no return channel (src/ports/decision.rs:28-33). The orchestrator's final text goes only to agentic_memory, with step_id webhook-handler (src/adapters/inbound/webhooks.rs:332-388,416-421). The loop never reads memory.
- **Need:** application: DecisionLoop::resume(event_key, reason) reloads the event and its history (jev-durable-history), re-reads world (research via templated key xm_research/1:{event:/event_id}, plus xm_escalation/1:<event_key>), adds `state.resume = {n, reason, research_status}` and runs up to max_steps. config: `max_resumes = 1`. Research-dependent actions use `requires = { research = 3600 }`. On failure, no research row is written: an `absent` row would count as usable (src/domain/observation.rs:48-50). That leaves only monitor / ignore / reject legal.
- **No-Rust path:** Stopgap: the architect re-triggers the loop itself via run_command -> `tengu decide --loop <n> --event -` with the research JSON (sandboxes/jev-exec/config.toml:74-79). Trade-offs: a fresh process without the pre-escalation history; shell_bins = ["tengu"] is a first-token check only (sandboxes/jev-exec/config.toml:52) and is forbidden in a signing sandbox (src/config/solana.rs:136-139); no shape validation; `--sandbox` is cwd-relative (docs/SESSION_HANDOFF.md:73).
- **Evidence:** rg -n -i "agentic|memory|recall|resume" src/application/decision_loop/ src/bootstrap/decision.rs src/ports/decision.rs -> none; read webhooks.rs:639-656, ports/decision.rs:28-33
- **Absorbed `rt-escalation-resume`** — Slow path closes the loop: escalation result -> bus event for the originating loop / lane (+ timeout, concurrency caps). `Escalator::escalate(correlation, message)` with correlation = `<loop>:<lane>:<event id>`. src/bootstrap/escalation.rs awaits orchestrator.handle() inside its task, extracts the architect JSON (schema from the jev slice), writes row `architect/1:<correlation>` (world-readable) and publishes {kind: escalation_result, correlation, result} on loop.<name>. escalation_timeout_secs = 300 -> an escalation_timeout event. [runtime] max_escalations_in_flight = 2, at most one per lane, identical escalations coalesced.

### `jev-obs-prefix` — Prefix listing on the observation store (watch lists, open candidates)

M5 · S · rust · missing · ports | adapters/outbound | application | config · PRD §22 §23 §29

- **Have:** ObservationStore offers only exact-key get / get_many / put / put_if_unchanged / remove (src/ports/observation.rs:10-37). SQLite reads `WHERE key = ?1` (src/adapters/outbound/observations.rs:93-100).
- **Need:** ports: list_prefix(prefix, limit) -> Vec<Observation>. adapters/outbound/observations.rs: `key LIKE ?1 ESCAPE '\'` with an escaped prefix and a limit. config: world alias form `{ prefix = "xm_triage/1:event:", limit = 20, where_status = ["monitoring"] }` rendering a map with full ids. Ticker option `for_each_prefix` ticks one event per monitoring row.
- **No-Rust path:** Composite rows: one deterministic writer keeps a list row (xm_paper_book/1:paper-main, xm_watchlist/1:<loop>) that FromObservation slots read (src/application/decision_loop/slots.rs:88-96). Trade-off: the writer must keep it consistent. Acceptable for P0.
- **Evidence:** rg -n "list_prefix|scan_prefix|keys_with_prefix|LIKE" src/ports/observation.rs src/adapters/outbound/observations.rs -> none

### `x-watch-set` — `monitor` raises data frequency: `watch/1` rows read by feeds + WS subscriptions

M5 · S · rust · missing · outbound (tool) | application | config · PRD §20 §22 §35 S2 §35 S5
After: `jev-obs-prefix` (M5), `jev-triage-state-tool` (M5), `rt-scheduler` (M0)

- **Have:** rt-ws-client plans to reconcile subscriptions from `watch/1:<venue>:<coin>` rows, but no gap writes those rows. rt-scheduler `kind = 'tool'` feeds take static args. jev's `monitor` only records xm_triage state. The observation store cannot list keys by prefix (src/ports/observation.rs:10-37).
- **Need:** Tool `xm_watch` {op add|extend|remove, instrument_id*, ttl_s*, reason, event_key} writes watch/1:<instrument_id> {until_ms, reasons[], event_keys[]}, capped at max_watched with audited LRU eviction. rt-scheduler gains `args_from = { prefix = 'watch/1:' }` so a tool feed (hl_book, hl_candles, rh_dex_quote) fans out over live watch rows. rt-ws-client and hl-ws-stream subscribe and unsubscribe from the same set. Rows expire at until_ms. A loop's `monitor` action becomes xm_mark(monitoring) + xm_watch(add).
- **No-Rust path:** Partial — a fixed watch list in TOML feed args works but needs a restart to change; dynamic watching needs the tool
- **Evidence:** rg -i 'watchlist|watch_set|watch/1' src → 0; read src/ports/observation.rs:10-37; rt-ws-client need text (subscription set reconciled from watch rows)

### `rt-followup-timers` — `recheck_after_secs` on monitor / hold / paper actions ⇒ delayed lane event (timers in `runtime.db`)

M5 · S · rust · missing · config | application | outbound · PRD §22 §30
After: `jev-durable-history` (M5), `rt-dedup-state` (M2)

- **Tracker note:** Timers live in `runtime.db`, not `<workspace>/.tengu/feeds.db` (convention 3).
- **Have:** A loop runs only when an event arrives (decision_loop/mod.rs:104-122). A `monitor` / `hold` choice ends the event with no scheduled re-check, and there are no timers anywhere (see rt-scheduler evidence).
- **Need:** config: ActionConfig (src/config/decision_loop.rs:91-123) gains `recheck_after_secs`. application: after the step, the runtime schedules {kind: recheck, lane, correlation} on loop.<name> at now+N (latest wins per lane; cap max_monitors). A `timers(fire_at_ms, loop, lane, payload)` table in feeds.db lets monitors survive restarts.
- **No-Rust path:** A tick feed over the whole loop (rt-scheduler) re-evaluates every lane each tick: Jev calls on idle lanes and no per-lane timing.
- **Evidence:** read config/decision_loop.rs:91-123 and decision_loop/mod.rs:104-122; interval/timer rg as in rt-scheduler -> 0.

### `ops-audit-record-v2` — Audit v2: event key, state digest, world versions, legal / hidden actions, gate signal, config hash

M5 · M · rust · partial · domain | application | config | bootstrap · PRD §22 §23 §31 §32 §35 S5
After: `info-store` (M4), `jev-event-templating` (M0), `ops-audit-atomic-write` (M0)

- **Have:** Each line is {ts, loop, session_id, t, decision_id (= the Jev gen id, e.g. gen-dec-1790700563-VCUDote92K781blfoXog), model, answers, usage, result, args, ok, output, obs} (mod.rs:434-450). This answers the §32 questions 'what did JEV choose' and 'with what confidence'; the keys of answers.next_action.probabilities implicitly list the legal actions. Not recorded: the triggering event (only session_id webhook-<endpoint>-<uuid> or decide-<loop>-<uuid>), the world as sent to Jev, slot label→value maps (e.g. pool_2 → address), actions hidden by requires or empty slots, config/state hash, run/event ids, sandbox. The plan doc claims 'state hash, questions' are audited (docs/decision-loop-plan-2026-09-24.md:108); they are not. `t` restarts per process (LoopState default, mod.rs:70-79).
- **Need:** Schema v2, one record per step: v, ts_ms, sandbox, loop, cfg_hash (sha256 of the DecisionLoopConfig JSON; sha2 is already a dependency, Cargo.toml:34), run_id (one per handle_event), event_id (a new DecisionLoopConfig field event_id_path, a JSON pointer into the raw event; otherwise minted evt-<uuid>), trigger (webhook:<endpoint> | decide | replay | schedule), step, decision_id (tengu dec-<uuid>; the provider id moves to jev_id), engine (jev | rules), model, state_hash (sha256 of the canonical state), world {alias: {key, observed_at_ms, slot, status}} (new World::versions() in world.rs), legal {action: {slot: {label: value}}}, hidden {action: reason} (collected in the legal-action pass, mod.rs:139-157), answers, confidence, act_at, result, args, ok, output, obs, usage, cost_usd, latency_ms, links {escalation_id}. Plus one event record per run: the reduced event, redacted with SecretRegistry::redact_value (src/domain/secrets.rs:52). Files: domain/decision.rs (AuditRecord, WorldRef), application/decision_loop/{mod,world}.rs, config/decision_loop.rs (event_id_path; the struct is deny_unknown_fields), bootstrap/decision.rs (sandbox, trigger). render_audit shows hidden actions and ids. Optionally the uuid 'v7' feature for time-ordered ids (Cargo.toml:27 has v4 only).
- **No-Rust path:** none — only the loop has the legal/hidden sets and world versions at decision time; they cannot be rebuilt later from TOML or logs
- **Evidence:** jq -c 'keys' ~/.tengu/logs/decisions.jsonl | sort | uniq -c → 4 key sets, none with event/world/questions/state_hash; rg -n 'state_hash|"questions"|"world"|"event"' src/application/decision_loop/mod.rs → only the state JSON sent to Jev (192, 197), never the audit
- **Absorbed `jev-decision-audit`** — Decision audit for §32 and calibration: event key, state digest, world versions, legal set, gate, failures. In application/decision_loop/mod.rs audit(), add: event_key, step, state_sha256 (sha2 is already a dependency), world {alias: {key, observed_at_ms, status}}, legal {action: {slot: {label: full value}}}, gate {signal, value, p_chosen, threshold, by: <question key>}, escalation_id, act_at. Write the full state + questions to <TENGU_HOME>/logs/decision-states/<date>.jsonl, keyed by decision_id, for replay. On an engine error, write {outcome: engine_error, error} and continue fail-soft. Keep render_audit compatible (mod.rs:482-527).

### `ops-audit-store` — `audit.db` behind a `DecisionAudit` port + link table (risk, paper, escalation, outcome)

M5 · M · rust · partial · ports | outbound | config | bootstrap | inbound · PRD §32 §33
After: `ops-audit-record-v2` (M5), `risk-gate-domain` (M0), `risk-gate-enforcement` (M0), `risk-paper-tools` (M0)

- **Have:** One global JSONL file, <TENGU_HOME>/logs/decisions.jsonl, shared by every sandbox (src/bootstrap/decision.rs:26-31). No rotation, no query. `tengu prune` deletes <TENGU_HOME>/logs (src/adapters/outbound/prune.rs:56). The TUI feed filters by loop name only (src/adapters/inbound/tui/mod.rs:83-85). No id links to risk verdicts, paper fills, escalations or outcomes.
- **Need:** ports/audit.rs: DecisionAudit { append_run, append_event, append_decision, link(from_id, rel, to_id, body), query }. adapters/outbound/audit_sqlite.rs writes <TENGU_HOME>/state/audit/<sandbox>.db (WAL) with tables runs, events, decisions, states(state_hash PK, body), links(from_id, rel, to_id, ts_ms, body) and outcomes(subject_id, instrument_key, horizon_s, ret, ret_net, reacted, computed_ms). adapters/outbound/audit_jsonl.rs keeps today's writer as a mirror. config/audit.rs: [audit] store = 'sqlite' | 'jsonl', jsonl_mirror = true, states_keep_days = 180. bootstrap/decision.rs selects the adapter; the risk, paper and escalation code writes links. Id chain: event_id → run_id → decision_id → risk_verdict_id → paper_order_id → fill_id; outcomes are keyed by decision_id or event_id plus instrument plus horizon. CLI adapters/inbound/cli/audit.rs: `tengu audit tail` (server-side feed via render_audit) and `tengu audit show <decision_id>` (the joined chain). Retention: runs, decisions, links and outcomes kept forever (~1–2 KB per decision; 3k–25k decisions/day ≈ 5–50 MB/day); states 180 days; JSONL mirror 30 days. prune never touches state/audit.
- **No-Rust path:** partial — keep JSONL and join offline with jq or the DuckDB CLI outside the repo (DuckDB reads JSONL). Trade-off: no live joins, other slices cannot write links, and the prune and interleaving risks remain.
- **Evidence:** rg -n -i 'trait DecisionAudit|trait AuditStore|audit\.db' src → none; rg -n 'logs' src/adapters/outbound/prune.rs → :56; read tui/mod.rs:51-100

### `ops-rules-baseline` — Deterministic rules engine + shadow mode (Jev vs rules on identical states)

M5 · M · rust · missing · outbound | application | config · PRD §25 §34 §35 S8
After: `jev-xmarket-loops-toml` (M0), `ops-audit-record-v2` (M5)

- **Have:** Only JevClient implements DecisionEngine (src/adapters/outbound/decisions.rs:53), plus a test-only Scripted engine (src/application/decision_loop/mod.rs:556). Deterministic policy exists only as pure typed decide tools (hedge_decide / lp_decide, src/domain/lp/), which Jev runs rather than being replaced by them.
- **Need:** adapters/outbound/rules_engine.rs (or the pure application/decision_loop/rules.rs): RulesEngine: DecisionEngine answers the same questions from TOML: [decision_loops.<n>.baseline] rules = [{ when = [{ path = '/world/<alias>/features/basis_bps', op = '>', value = 25 }], pick = '<action>', slots = { <slot> = 'first' | 'max:<feature>' } }], default = '<terminal action>'. Confidence is always 1.0; config/decision_loop.rs validates paths, actions and slots. New keys: [decision_loops.<n>] engine = 'jev' | 'rules', and shadow = 'rules' | 'jev'. With shadow set, each step also asks the shadow engine and audits shadow_answers without acting, which gives paired Jev-vs-rules evidence on identical live states from S5 onward (§34).
- **No-Rust path:** partial — write the rules as a pure typed *_decide tool (Rust per strategy, like lp_decide), or as a second Jev loop whose goal spells out the rules. Trade-off: Rust per strategy vs 'rules' that are not deterministic.
- **Evidence:** rg -n 'impl DecisionEngine for' src → decisions.rs:53 and the mod.rs:556 test engine only; rg -n -i 'baseline|shadow' src/config src/application/decision_loop src/ports → none

### `ops-alerts-telegram` — Push alerts to Telegram: candidates, paper entries, escalations, risk denies, stale feeds, spend

M5 · M · rust · missing · ports | outbound | application | inbound | config · PRD §27 §30 §35 S5 §35 S7
After: `ops-audit-record-v2` (M5), `rt-daemon` (M0)

- **Have:** The Telegram bot sends only inside the inbound message handler (src/adapters/inbound/telegram.rs:171, 353-376). There is no Notifier port. The decision feed exists only in the interactive TUI (src/adapters/inbound/tui/mod.rs:51-100). TelegramConfig { enabled, allowed_users, tool_approvals, approve_only } (src/config/mod.rs:573-582); approvals are not wired. Loops run in `tengu webhooks` or `tengu decide` processes that have no Telegram client.
- **Need:** ports/notify.rs: Notifier { notify(Alert { level, title, body, dedup_key }) }. adapters/outbound/notify_telegram.rs: POST https://api.telegram.org/bot<token>/sendMessage through the egress tool client, token from TELEGRAM_BOT_TOKEN, chat from [alerts] chat_id_env. application/alerts.rs: rules over audit records and heartbeats (on = ['escalated', 'paper_enter', 'risk_deny', 'error', 'feed_stale', 'budget_warn']), throttled to ≤1 msg/s per chat and ≤20/min in groups (Telegram FAQ), with a digest window. Wire it in the daemon process (the webhooks listener today) as an audit subscriber. Alert text shows ids in full (render_audit style).
- **No-Rust path:** partial — a loop action 'alert' = http_request POST https://api.telegram.org/bot$TELEGRAM_BOT_TOKEN/sendMessage with a static chat_id and slot-templated text (request.rs:36, 145 expand $VAR in URLs). Scope it with env_reads = ['TELEGRAM_BOT_TOKEN'] and net_hosts = ['api.telegram.org'] on the non-routable executor only. Trade-offs: each alert costs a Jev decision, the text is limited to enumerated slot values, it cannot fire on escalations, errors or budget, and there is no throttling.
- **Evidence:** rg -n -i 'trait Notifier|fn notify|push_notif|sendMessage|send_alert' src → none; rg -n 'send_message|send_text' src/adapters/inbound/telegram.rs → reply paths only; WebFetch core.telegram.org/bots/faq

### `jev-docs` — Loops, templating, classify questions, slow path, audit fields

M5 · S · docs · missing · n/a · PRD §22 §27 §32
After: `jev-architect-escalator` (M5), `jev-classify-questions` (M5), `jev-event-templating` (M0)

- **Have:** docs/decision-loop-plan-2026-09-24.md describes static loops, and its line 108 claims the audit logs 'state hash, questions', which the code does not. docs/code-map.md:64,100,174,269-293 cover the current files.
- **Need:** Update docs/decision-loop-plan-2026-09-24.md (templating, classify, action kinds, escalation contract, resume, audit fields; fix :108). Update docs/code-map.md and its .html GRAPH for new files, docs/typed-observations-2026-09-24.md for the xm_* schemas (xm_triage/1, xm_ctx/1, xm_research/1, xm_escalation/1), and CLAUDE.md + AGENTS.md gotchas. New docs/xmarket-jev-loops-<date>.md, at most one screen.
- **No-Rust path:** n/a (docs).
- **Evidence:** read docs/decision-loop-plan-2026-09-24.md:103-110 vs src/application/decision_loop/mod.rs:434-450


## M6 — opportunity research + full paper execution

### `risk-opportunity-metrics` — `xm_opportunity`: convergence, information latency, over- / under-reaction, related-asset

M6 · M · rust · missing · domain + outbound/tools · PRD §21 §12 §14 §34 §35 S6
After: `kg-graph-edges` (M5), `risk-calc-costs` (M0), `risk-calc-market-stats` (M2)

- **Have:** nothing
- **Need:** domain: src/domain/xm/opportunity.rs.
  - Convergence: basis z-score against a rolling mean, plus a half-life from an AR(1) fit on the basis series.
  - Information latency: lead-lag, plus time since the reference move.
  - Overreaction: leveraged move / reference move, funding z, OI change, top-N book imbalance.
  - Underreaction: reference moved more than k sigma while the venue's residual gap remains.
  - Related-asset: co-move beta against the direct asset over the event window; takes kg relation confidence as an input.
  tool: `xm_opportunity` (family*, a*, b, knobs*) -> xm_opportunity/1:<family>:<a>:<b> with the score inputs, edge_after_costs_bps and confidence inputs as features. Jev selects; nothing here trades.
  Tests: synthetic series per family.
- **No-Rust path:** none
- **Evidence:** rg -n -i 'opportunit|convergence|lead_?lag|overreact|underreact' src: no hits.

### `kg-edge-stats` — Per-edge hit rate, beta, lead-lag over time; deterministic strength evolution

M6 · M · rust · missing · domain + application + inbound · PRD §12 §21 §33 §34 §35 S6 §35 S8
After: `kg-confirm-reaction` (M5), `kg-graph-edges` (M5), `ops-history-recorder` (M1)

- **Have:** nothing
- **Need:** Pure src/domain/xmarket/stats.rs:
  - rolling beta/corr on aligned returns
  - lead-lag cross-correlation at ±{1,5,15,60} min
  - per-edge hit rate, sign consistency and mean |abnormal| from `reactions`
  Job src/application/xmarket/edge_stats.rs: recompute on new reactions + nightly; writes the `edge_stats` table.
  Deterministic strength evolution (thresholds in `[xmarket.graph]`), recorded as edge_events with provenance market_confirmed: possible → strong proposal after ≥ K confirmed reactions with consistent sign; strong → possible after M misses.
  CLI: `tengu xm edges stats <entity_id>`.
- **No-Rust path:** none (numeric statistics).
- **Evidence:** rg -n -i 'lead_lag|beta\b|correlation|rolling' src/ → 0 relevant.

### `ops-outcome-labeller` — Forward returns per venue at {1m, 5m, 15m, 1h, 4h, 24h} ("what happened afterward")

M6 · M · rust · missing · domain | application | inbound · PRD §12 §32 §33
After: `kg-catalog-store` (M1), `ops-audit-store` (M5), `ops-history-recorder` (M1), `risk-paper-tools` (M0)

- **Have:** Nothing records what happened after a decision. The audit keeps only the executed step's immediate reduced output (mod.rs:444-449). The only price history is the ≤5-minute sample ring inside a price_oracle row (src/domain/lp/market.rs:255, 386).
- **Need:** domain/outcome.rs (pure): horizon grid [60, 300, 900, 3600, 14400, 86400] s; as-of mid or executable price with a max staleness; forward return; net return through the risk slice's cost model (fees, spread, slippage, funding); reaction flag |r| > k·σ(5 min). application/outcome/label.rs: select decisions and events whose last horizon has elapsed and which are not yet labelled; the instruments are the action args plus the event's affected instruments on every venue (from kg); read HistoryStore::asof; write outcomes(subject_id, instrument_key, horizon_s, ret, ret_net, reacted, computed_ms) via DecisionAudit. CLI adapters/inbound/cli/xm.rs: `tengu xm label --sandbox xmarket [--since <ts>]`, idempotent, run by the rt daemon's schedule or by host cron.
- **No-Rust path:** partial — offline DuckDB ASOF JOIN over exported history outside the repo. Trade-off: not reproducible in-repo and no links back into the audit.
- **Evidence:** rg -n -i 'forward_return|forward return|outcome' src/domain src/application → none for markets; read mod.rs:424-464

### `ops-report-markets` — Venue lead-lag, source lead-time, relationship stability reports

M6 · L · rust · missing · domain | application | inbound · PRD §14 §33 §34
After: `info-store` (M4), `kg-graph-edges` (M5), `ops-history-recorder` (M1), `ops-outcome-labeller` (M6), `rt-scheduler` (M0)

- **Have:** Nothing, and no venue timestamps are stored. observed_at_ms is our own read time (src/domain/observation.rs:161), so it carries ± poll-interval error.
- **Need:** domain/stats/leadlag.rs: 1 s as-of grid, lagged cross-correlation ±60 s, first-to-cross-kσ counts, split by asset class and session (US regular hours, extended, weekend — RH tokens and HL trade 24/7). domain/stats/leadtime.rs: lead_s = t_reaction(direct asset, fastest venue) − first_seen_ms, per source, asset class and §17 info state. domain/stats/relationship.rs: per-edge hit rate with a Wilson CI and rolling beta vs the direct asset; an edge is stable when the CI lower bound exceeds a threshold with n ≥ 10. application/research/{markets,sources,relationships}.rs. CLI: `tengu xm report markets|sources|relationships`. Needs venue_ts_ms from feeds; lead-lag under 15 s needs stream feeds (rt).
- **No-Rust path:** partial — offline analysis on exported Parquet with the DuckDB CLI outside the repo. Trade-off: not reproducible or tested in-repo.
- **Evidence:** rg -n -i 'lead.lag|leadlag|lead_time|cross.corr' src → none

### `rh-paper-fill` — RH-venue paper fills (owns the AMM / RFQ quote path): AMM quote at size, Lighter book walk, gas incl. L1 data fee, failures

M6 · M · rust · missing · domain+outbound · PRD §31 §34 §35 S7
After: `rh-dex-quote-v3` (M2), `risk-paper-ledger-store` (M0)

- **Tracker note:** Owns the AMM / RFQ quote path (moved out of `risk-paper-fill-engine`).
- **Have:** Nothing. rg finds only Solana write-tool slippage bounds (src/adapters/outbound/tools/solana/write_perps.rs:108-147).
- **Need:** Pure src/domain/xmarket/fill_rh.rs.
  
  Fill model:
  - Re-read the quote at decision time; latency = quote age + poll cadence.
  - AMM: the QuoterV2 / V4Quoter exact output for the order size. Partial fill above max_impact_bps; failed fill if the pool moved beyond slippage.
  - Lighter: walk the book levels.
  
  Costs: pool fee (already inside the quote), gas (L2 execution + L1 data fee), and the LI.FI fixed fee when routed through it.
  
  Failure modes: stale quote, oracle paused, halted underlying, outside the mint window.
  
  Output: a paper_fill record for the risk slice's ledger (paper_fill/1:<id>, never cached). Tests are pinned to the 2026-09-29 AAPL ladder.
- **No-Rust path:** None. Fills are arithmetic over quotes (§25, §31).
- **Evidence:** rg -n -i 'paper.?trad|paper_fill|fill.?model|partial.?fill' src: none.
- **Absorbed `risk-paper-rh-costs`** — Robinhood Chain token paper fills: quote price + gas (L2 execution + L1 data) + spread. domain: a quote path in src/domain/xm/paper.rs.
  - Fill = the quoted amount_out for the exact size, from an AMM / PropAMM quote row supplied by the rh slice (e.g. rh_quote/1:<token>:<side>:<notional>).
  - Reject when the quote is older than max_data_age_ms.quote or the market is closed.
  - Network cost = gas_units x gas_price x ETH/USD row. Robinhood Chain bundles the L1 data fee into gas (docs); probe eth_gasPrice = 0x144bcd0.
  - Record spread against the reference mid separately for S8.
  Tests: vectors with a fixed quote and gas.

### `rh-agg-quote` — `evm_swap_quote` via LI.FI (keyless; 0x only if bought)

M6 · S · rust · missing · outbound · PRD §20 §31 §34
After: `kg-sync-robinhood` (M1), `rh-accounts` (M2)

- **Have:** nothing
- **Need:** Tool evm_swap_quote in src/adapters/outbound/tools/rh/aggregator.rs.
  
  LI.FI:
  - GET https://li.quest/v1/quote?fromChain=4663&toChain=4663&fromToken=<sell>&toToken=<buy>&fromAmount=<raw>&fromAddress=<paper address>.
  - Returns toAmount, toAmountMin, tool (rialto, nordstern, openocean, kyberswap), fee costs (0.25 % LI.FI fixed fee observed) and gas USD.
  - Optional key $LIFI_API_KEY raises the limit to 100 req/min.
  
  Key swap_quote/1:4663:<sell>:<buy>:<amount>, TTL 10 s. An in-tool budget guard maps the keyless limit (75 quotes per 2 h) to the rate_limited class.
  
  0x Swap API v2 (https://api.0x.org/swap/allowance-holder/price, headers 0x-api-key and 0x-version: v2) goes behind the same schema only if the Standard plan ($1,000/mo) is bought.
- **No-Rust path:** Works for exploration now: a skill plus http_request GET to li.quest (keyless JSON). Output is text only, and the keyless budget of 75 per 2 h rules out loop use.
- **Evidence:** - rg -n -i 'li.quest|api.0x.org|0x-api-key|rfq|aggregator' src sandboxes skills: none.
  - Probes: LI.FI quote for 3310 USDG returned toAmount 9967895361237707551 (18-dp AAPL); 0x returned 'No API key found in request'.

### `risk-paper-funding` — Hourly funding accrual on paper perps (HIP-3 multipliers from market rows)

M6 · S · rust · missing · domain + outbound · PRD §31 §25
After: `hl-funding-tool` (M2), `ops-history-recorder` (M1), `risk-paper-ledger-store` (M0)

- **Have:** nothing (only the Jupiter perps borrow fee in src/domain/lp/perps.rs).
- **Need:** outbound + domain: on every ledger touch (paper_order, paper_positions), for each open perp and each whole hour crossed since funding_through_ms:
  - Read the rate from the hl slice's fundingHistory rows (probe: xyz:TSLA rows at time = hour + ~20 ms) and the oracle price at that hour from series.db.
  - Payment = size x oracle x rate; a positive rate means longs pay.
  - INSERT into funding(account, instrument, hour_ms); the primary key makes it idempotent.
  - A missing rate sets funding_unknown and makes P&L status partial (never assume 0).
  Tests: vectors across hour boundaries, sign, a missing hour.
- **No-Rust path:** none
- **Evidence:** rg -n -i 'funding' src: only Jupiter perps borrow and hedge carry knobs (tools/solana, domain/lp). Probe fundingHistory confirms hourly rows for HIP-3.

### `risk-paper-pair-orders` — Two-leg paper orders: hedge availability, leg latency, unwind

M6 · M · rust · missing · domain + outbound/tools · PRD §21 §28 §31
After: `risk-paper-tools` (M0)

- **Have:** nothing. A loop runs one tool per step (src/application/decision_loop/mod.rs:328-368).
- **Need:** tool `paper_pair` (leg_a*, leg_b* {instrument, side}, notional_usd*, max_leg_gap_ms*, max_slippage_bps*, client_order_id) -> paper_pair/1:<account>:<client_order_id>.
  1. Gate both legs before leg A. Hedge availability = leg B permitted, fresh, depth >= min.
  2. Fill A, inject the leg latency, fill B against a fresh book.
  3. If B fails or partials beyond tolerance, unwind A at market and record the legging cost.
  Tests: both legs fill; B fails and A unwinds; gate denies B so nothing fills.
- **No-Rust path:** Partial: two sequential loop actions (leg A, then leg B with requires). There is no all-or-nothing and no unwind, so legging risk goes unmeasured.
- **Evidence:** rg -n -i 'pair_order|two.?leg|legging|unwind' src: no hits.

### `x-risk-relation-gate` — Gate enforces §7 / §17 on entries (`min_relation`, `min_info_state`) + `xm_tradable/1` rows for the opportunity loop

M6 · S · rust · missing · domain | outbound (tool) · PRD §7 §17 §24 §28 §35 S7
After: `info-evolution` (M4), `jev-context-composer` (M5), `kg-related-assets` (M5), `risk-gate-domain` (M0), `risk-gate-enforcement` (M0)

- **Tracker note:** Also owns the `xm_tradable/1` rows (moved from `risk-kill-switch`).
- **Have:** Relations are filtered only while building candidates (jev-context-composer: direct/strong; kg-related-assets: accepted edges). The planned gate rules (risk-gate-domain) cover venue, instrument, lifecycle, notional, exposure, leverage, loss, kill switch, minimum edge, slippage and depth, hedge, freshness and skew, and order rate. None checks relation strength or information state. risk's xm_tradable/1 slot would let Jev pick any permitted instrument regardless of the event.
- **Need:** New risk-gate-domain rules for entries: `min_relation` (default strong) and `min_info_state` (default credible_report). Both are read inside the gate transaction from the candidate row passed as paper_order's `opportunity` key (xm_ctx/1 candidate → xm_impact/1 edge id and strength; xm_event/1 info_state). A missing value denies with `missing:relation`. Reduce and close orders are unaffected. Tests: a speculative or possible relation, or a rumor-only event, never fills.
- **No-Rust path:** none — only runtime enforcement protects; prompts and candidate filtering can be bypassed through slot choice
- **Evidence:** rule list in the risk-gate-domain need; loop caps are numeric bounds per slot only (src/config/decision_loop.rs:111-114; src/application/decision_loop/mod.rs:281); PRD §7: 'Automated trading should initially focus primarily on direct and strongly supported relationships'


## M7 — evaluation

### `x-hl-historical-backfill` — Import the HL public S3 archive (L2 snapshots, asset contexts, fills) into `HistoryStore`; check HIP-3 coverage first; the archive lags ≈ 1 month

M7 · M · rust · missing · outbound | inbound | infra · PRD §33 §34 §35 S6 §35 S8
After: `hl-book-tool` (M0), `hl-market-schema` (M0), `ops-history-recorder` (M1)

- **Tracker note:** Moved to M7 (review): the archive is updated ≈ monthly with no timeliness guarantee, HIP-3 coverage is unverified and the RH leg has no archive, so it cannot serve the M3 7-day window. Check HIP-3 coverage first; AWS requester-pays credentials go in `x-accounts-secrets`.
- **Have:** The repo keeps no history: the store is a latest-row cache with a 7-day purge (src/adapters/outbound/observations.rs:21-27). The HL info API returns at most 5000 candles and fundingHistory, but no historical books or OI. No report mentions the archive, and Cargo.lock has no lz4 crate.
- **Need:** (1) Research (S): which coins exist, including HIP-3 `dex:COIN` names, and for which dates. (2) Operator download, requester pays: `aws s3 cp --request-payer requester s3://hyperliquid-archive/market_data/<date>/<hour>/l2Book/<coin>.lz4`, `s3://hyperliquid-archive/asset_ctxs/<date>.csv.lz4`, and fills from `s3://hl-mainnet-node-data/node_fills_by_block`. (3) Rust importer `tengu history import hl-archive --dir <path>` (lz4 via the pure-Rust lz4_flex crate) writes hl_book/1 and mkt_ctx/1 series into the HistoryStore with source = archive. S6 research and S8 replay then start with months of books instead of weeks of local recording.
- **No-Rust path:** Partial — the download is infra; decoding and import must be Rust (no scripts in the repo)
- **Evidence:** WebFetch https://hyperliquid.gitbook.io/hyperliquid-docs/historical-data (2026-09-29): bucket hyperliquid-archive, paths market_data/[date]/[hour]/[datatype]/[coin].lz4 and asset_ctxs/[date].csv.lz4, requester pays, updated about monthly, 'no guarantee of timely updates and data may be missing'; HIP-3 coverage not stated; Cargo.lock has no lz4 or lz4_flex package

### `ops-clock-port` — Injectable clock for decision loops (replay prerequisite)

M7 · S · rust · partial · ports | application | bootstrap · PRD §35 S8

- **Have:** DecisionLoop reads the wall clock through now_ms() (src/application/decision_loop/mod.rs:138, 333) and now_unix() (403, 435). observe() already takes now_ms as a parameter (src/application/observe.rs:20), and World::read takes now (src/application/decision_loop/world.rs:50-54).
- **Need:** ports/clock.rs: Clock: Send + Sync { fn now_ms(&self) -> i64 }, with a SystemClock and a FakeClock (for replay and tests). DecisionLoop::new takes Arc<dyn Clock>; replace the four wall-clock reads; the audit ts comes from the clock; bootstrap/decision.rs passes SystemClock. Typed tools keep the wall clock, because replay never runs real tools.
- **No-Rust path:** none
- **Evidence:** rg -n 'now_ms\(\)|now_unix\(\)' src/application/decision_loop → mod.rs:138, 333, 403, 435; rg -n -i 'trait Clock|FakeClock|SystemClock' src → none

### `ops-replay-harness` — `tengu xm replay`: recorded data through the loops on a fake clock; recorded-Jev and rules arms

M7 · L · rust · missing · application | ports | outbound | inbound | config · PRD §33 §34 §35 S8
After: `info-store` (M4), `ops-audit-record-v2` (M5), `ops-clock-port` (M7), `ops-history-recorder` (M1), `ops-rules-baseline` (M5), `risk-paper-tools` (M0)

- **Have:** Pieces only: StubbedExecutor replays canned tool outputs (src/adapters/inbound/eval.rs:598-642); an in-memory MemStore exists in observe.rs tests; a pure policy has a golden-vector replay (src/domain/lp/hedge.rs:588, tests/fixtures/hedge-vectors.jsonl). The loop reads the wall clock (mod.rs:138, 333). There is no event replay, as-of store, fake clock, recorded-decision cache or arms configuration.
- **Need:** application/replay/:
  - mod.rs: per arm, reset the loops and feed events in time order through DecisionLoop::handle_event on a FakeClock.
  - source.rs: merge recorded info events event/1 (by first_seen_ms only, to avoid lookahead), market-anomaly triggers recomputed from recorded rows by the deterministic layer, and schedule ticks.
  - asof_store.rs: ReplayObservationStore, where get/get_many call HistoryStore::asof(t).
  - executor.rs: ReplayToolExecutor. Typed read tools return the recorded observation as of t; paper tools go to the risk slice's paper broker on the FakeClock with recorded books/depth.
  - engines.rs: RecordedDecisionEngine caches (state_hash, questions hash) → Jev decision in <TENGU_HOME>/state/replay/jev-cache.db and calls live Jev only on a miss (~$0.042 per million input tokens), so re-runs are deterministic; plus RulesEngine.
  Config: [replay.arms.<name>] engine = 'jev' | 'rules', events = ['market'] | ['market', 'info'], mask_world = ['info_*']. CLI adapters/inbound/cli/xm.rs: `tengu xm replay --sandbox xmarket --from <ts> --to <ts> --arms market,market+info,market+jev,market+info+jev --out evals/xm/<ts>/` (reuse eval.rs:48 prune_old_run_dirs and eval.rs:40 OutputFormat). Replay decisions are audited with trigger = replay into a separate audit db.
- **No-Rust path:** none
- **Evidence:** rg -n -i 'replay|backtest|fake_clock|FakeClock' src → only the skill dialog_replay metric and the hedge vector test; read eval.rs:598-642, observe.rs tests (MemStore)
- **Absorbed `jev-state-replay`** — Replay recorded states against Jev builds and a rules baseline (§35 S8 ablations). inbound CLI `tengu decide --replay <decision-states.jsonl> --loop <n> [--model typesafe/jev-1.13] [--policy rules]` re-asks the recorded questions, or applies a deterministic rules policy from TOML (first legal action by priority). It writes comparison JSONL (agree, p_chosen delta, would-have-acted) for the market-only / +info / +JEV comparisons.

### `ops-ablation-report` — 4-arm report: market-only vs +information vs +JEV vs +information+JEV

M7 · M · rust · missing · domain | application | inbound · PRD §34 §35 S8
After: `ops-outcome-labeller` (M6), `ops-replay-harness` (M7), `ops-report-decisions` (M7)

- **Have:** Nothing. rg for ablation, sharpe, drawdown, forward_return or reliability finds only Jupiter perps position PnL decoding (src/domain/lp/perps.rs:136, 768).
- **Need:** application/replay/report.rs, over identical event sets per arm, reporting: #events, #candidates, #paper entries, hit rate, gross and net P&L after costs (risk cost model), daily Sharpe, max drawdown, turnover, escalation rate, and calibration for the JEV arms. A paired bootstrap CI of the net P&L difference between arms, using the domain/stats functions shared with ops-report-decisions. Output: evals/xm/<ts>/report.json, summary.md and a table (eval.rs print_table style).
- **No-Rust path:** partial — export per-arm audit and outcomes and compare them with the DuckDB CLI outside the repo. Trade-off: not reproducible in-repo.
- **Evidence:** rg -n -i 'ablation|sharpe|drawdown|forward_return|reliability|pnl' src --type rust → only src/domain/lp/perps.rs position PnL

### `ops-report-decisions` — Jev calibration (reliability, Brier, `act_at` sweep), strategy P&L after costs

M7 · M · rust · missing · domain | application | config | inbound · PRD §21 §33 §34
After: `jev-xmarket-loops-toml` (M0), `ops-outcome-labeller` (M6), `risk-paper-tools` (M0)

- **Have:** Nothing. The audit already stores confidence and full probability vectors (src/domain/decision.rs:43-57), but nothing aggregates them.
- **Need:** domain/stats/calibration.rs: 10 reliability bins, ECE, Brier, and multi-class Brier over the probabilities vector. domain/stats/pnl.rs: gross and net P&L, hit rate, average edge in bps, daily Sharpe, max drawdown and turnover per §21 opportunity family. application/research/report.rs. Per-action success criteria in TOML: [decision_loops.<n>.actions.<a>] label = { horizon_s = 900, success = 'ret_net > 0' } (ActionConfig is deny_unknown_fields, so this is a new field). An act_at sweep over 0.6–0.95 answers 'when should JEV escalate' (§34). Segment by audited model, because the default alias ~typesafe/jev-latest floats (src/config/decision_loop.rs:240-242). CLI: `tengu xm report decisions --from --to [--loop] --format table|json` writing evals/xm/reports/<ts>.json.
- **No-Rust path:** partial — jq/DuckDB over the audit and outcome exports. Trade-off: ad hoc, with no per-action success rules in config.
- **Evidence:** rg -n -i 'calibrat|brier|ece|sharpe|drawdown' src → none
- **Absorbed `jev-calibration-report`** — Calibration + outcome join: `tengu decisions calibrate`. domain/decision.rs: DecisionOutcome {decision_id, event_key, label: correct|wrong|unknown, horizon_s, ret_bps, paper_pnl_usd, source}. Outcomes come from the paper engine (risk / ops) plus a labeler that marks market confirmation (did the chosen asset / venue move more than x bps within h). application/decision_calibration.rs (pure): reliability bins on p(chosen), Brier / log-loss, coverage vs accuracy per threshold, split by action / loop / model build. inbound CLI: `tengu decisions calibrate --loop <n> --since <date> [--outcomes <path>]` prints a table plus suggested per-action act_at.

### `info-replay` — Backfill + replay for lead-time and source evaluation

M7 · M · rust · missing · application + inbound · PRD §33 §34 §35 S8
After: `info-pipeline` (M4), `info-reliability` (M5), `ops-history-recorder` (M1)

- **Have:** Nothing news-related; the replay/backfill hits are skill dialog_replay and hedge vectors.
- **Need:** Backfill from: the EDGAR daily index (https://www.sec.gov/Archives/edgar/daily-index/2026/QTR3/form.20260928.idx, 5,088 rows), GDELT timespans, Alpaca news history (since 2015) and X full-archive (pay-per-use). src/application/news/replay.rs: deterministic time travel (now = published_at, cached extractions) through filter → cluster → evolve. Metrics: lead time per source and asset class, events followed by a market reaction (join rt market history), false-rumor rate. Command: tengu news replay --from --to.
- **No-Rust path:** None; analysis notebooks would be out of repo under the Rust-only rule.
- **Evidence:** rg -il 'replay|backfill' src → application/skills/lifecycle/*, adapters/inbound/eval.rs, domain/lp/* only.

### `x-discovery-eval` — Discovery accuracy: event → affected tradable assets; anomaly → explaining news

M7 · M · rust · missing · application | inbound · PRD §33 §34 §35 S4 §35 S8
After: `info-news-search` (M5), `info-replay` (M7), `kg-related-assets` (M5), `ops-audit-store` (M5)

- **Have:** The reports plan lead-lag, calibration and P&L reports (ops-report-*), extraction fixtures (info-taxonomy-skill evals), and `tengu eval` is an LLM-judge skill runner (src/adapters/inbound/eval.rs). Nothing measures §34's first two questions: 'Can news reliably identify affected tradable assets automatically?' and 'Can market anomalies reliably identify relevant news?'
- **Need:** Gold set: operator-labelled JSON under evals/xm/gold/, with {event_key → direct and related instrument ids plus relation class} and {anomaly id → explaining event_key, or none}. Samples come from `tengu xm label-sample --n 50`, which prints candidates with full ids; no LLM labels. Metrics in src/application/research/discovery.rs: precision and recall@k of xm_impact against the gold set, by relation class and asset class; anomaly attribution hit rate and precedes_move_s from why_moving/1 rows; misses by info_state. CLI `tengu xm eval discovery --gold <dir>` writes evals/xm/<ts>/discovery.json, which feeds the S8 report.
- **No-Rust path:** Partial — labelling is manual (operator); the metrics must be Rust (no notebooks in the repo)
- **Evidence:** rg -i 'recall@|gold' src → only unrelated numeric 'precision' hits under src/domain/lp; the ops-report-markets and ops-report-decisions needs (lead-lag, calibration, P&L) contain no mapping-accuracy metric

### `x-edge-decay-report` — Expected edge at detection / decision / gate vs realised fill + markouts, by latency bucket

M7 · S · rust · missing · domain | application | inbound · PRD §21 §31 §34 §35 S8
After: `ops-outcome-labeller` (M6), `ops-report-decisions` (M7), `risk-audit-verdicts` (M0), `risk-paper-fill-engine` (M0), `rt-latency-trace` (M3)

- **Have:** The pieces are planned but never joined: xm_compare edge_after_costs_bps (risk-calc-tools), the gate context digest (risk-audit-verdicts), fills (risk-paper-fill-engine), forward returns (ops-outcome-labeller) and pipeline timestamps (rt-latency-trace). ops-report-decisions reports P&L per family, not edge decay.
- **Need:** A pure fn in src/domain/xmarket/stats.rs: edge_decay(expected_bps at detect, at decision, at gate; realized_bps at fill; markouts at +1, +5 and +15 min), bucketed by latency (source→ingest→decide→fill ms). Report `tengu xm report edge --sandbox xmarket --from --to`, per opportunity family and venue: share of entries whose realized edge after costs is above 0, median decay per 100 ms of latency, and adverse-selection markouts. Rows are joined by call_id and decision_id.
- **No-Rust path:** Partial — SQL over ledger and audit exports outside the repo, but not reproducible in-repo
- **Evidence:** rg -i 'expected_edge|edge_decay|adverse' src → 0; PRD §34 Execution: 'Does theoretical edge survive realistic latency and execution costs?'


## M8 — P2: extensions

### `risk-evm-tx-pipeline` — RH Chain EIP-1559 send pipeline (nonce, simulate, confirm) + `rh_swap`

M8 · L · rust · missing · outbound + outbound/tools · PRD §30 §8 §31
After: `rh-dex-quote-v3` (M2), `rh-quote` (M2), `risk-evm-signer` (M3b), `risk-exec-runner-generic` (M3b)

- **Have:** Privy remote send only (src/adapters/outbound/tools/crypto/sign_tx.rs; always-on, scope = `default` wallet). The Solana pipeline is the pattern (src/adapters/outbound/solana/send.rs: lease -> resolve pending -> simulate -> sign -> pending record -> send once -> confirm -> fence).
- **Need:** outbound: src/adapters/outbound/evm/{rpc,send}.rs.
  - An alloy provider over the egress tool_client, never alloy's own reqwest client.
  - Chain id check: 4663 mainnet / 46630 testnet.
  - nonce = max(eth_getTransactionCount(pending), local pending + 1), with a pending record.
  - eth_estimateGas (includes the L1 data fee on Arbitrum Orbit) plus an eth_call simulation before sending.
  - EIP-1559 fees from eth_feeHistory; send once; poll the receipt; replace / cancel policy.
  tools: rh_swap (router and calldata from the rh slice's venue adapter), mode = simulate | send.
  Tests: a fake RPC like src/adapters/outbound/solana/test_chain.rs; testnet smoke test marked #[ignore].
- **No-Rust path:** Partial: Privy sign_and_send_transaction can already send EVM transactions if Privy supports chain 4663. But it has no simulation and no nonce / lease discipline, and it sits outside the risk gate; it would have to be wrapped by run_exec.
- **Evidence:** rg -n -i 'eth_getTransactionCount|feeHistory|estimateGas|ProviderBuilder' src: no hits. Probe: Robinhood Chain eth_chainId = 0x1237.
- **Absorbed `rh-evm-writes`** — Live execution on Robinhood Chain (EVM signer, swap or RFQ fill, lease + pending + fence). - Signer port src/ports/evm_signer.rs: a local key like [solana] signer_key_file, or Privy.
  - Write tool rh_swap: UniversalRouter 0x06AfBA43Fd06227fA663b0DAecF536f6EaA6bf99 + Permit2 0x000000000022D473030F116dDEE9F6B43aC78BA3, or an RFQ fill. mode = simulate or send, using eth_call simulation, eth_estimateGas, and min-out from rh_dex_quote.
  - Per-wallet lease, pending record and fence on the SolanaWriteStore shape (src/ports/solana_writes.rs:12).
  - Signing-sandbox rules like src/config/solana.rs.
  - An eligibility attestation in config.

### `hl-outcome-markets` — `hl_outcomes`: HIP-4 outcome markets as event probabilities

M8 · M · rust · missing · domain + outbound (tools) · PRD §10 §16 §17 §35 S4 §35 S8
After: `hl-info-client` (M0), `kg-sync-hyperliquid` (M1)

- **Have:** Nothing (rg for outcome_meta, prediction_market, polymarket, kalshi: none).
- **Need:** domain src/domain/hl/outcomes.rs + tool hl_outcomes. Source: {type: outcomeMeta}, currently 233 outcomes and 18 questions. Templates: companyIpoConfirmed, policyRateDecrease, policyRateIncrease, policyRateNoChange, binaryPrice, priceTouch, sports. Join with the spot ctx coins #<outcome><side>; e.g. outcome 2597 'Anthropic IPO by 20261031' => #25970 / #25971. Features: implied_prob, spread, vol_24h_usd. Key hl_outcome/1:<outcome>. The WS outcomeMetaUpdates channel comes later.
- **No-Rust path:** A skill + http_request works for LLM research. Jev features need the join.
- **Evidence:** Probes: outcomeMeta (233 outcomes); spotMetaAndAssetCtxs (466 coins with a # prefix).

### `x-prediction-markets` — Decide on Polymarket / Kalshi as observation-only probabilities

M8 · S · research · missing · n/a · PRD §4 §10 §17
After: `hl-outcome-markets` (M8), `kg-graph-edges` (M5)

- **Have:** Only hl-outcome-markets (HL HIP-4 outcomes, P2) is planned. rg polymarket|kalshi src → 0.
- **Need:** Research note covering: public read APIs; geo and ToS for the operator's jurisdiction; how well markets map to entities and events; whether implied-probability moves add confirmation (§17) or lead time (§34). If the answer is yes, add a read-only typed tool writing outcome/1:<venue>:<market> rows (implied_prob, spread, volume) mapped to kg entities.
- **No-Rust path:** Yes for exploration — a skill plus http_request for the architect; typed rows need a small adapter later
- **Evidence:** rg -i 'polymarket|kalshi|prediction_market' src → 0; PRD §10: 'relevant crypto prediction/perpetual markets if available'

### `hl-liquidation-research` — Detect HL liquidations without a public liquidation feed

M8 · S · research · missing · n/a · PRD §3 §21
After: `hl-ws-stream` (M2)

- **Have:** Nothing.
- **Need:** HL info and WS expose no public liquidation channel. Public trades carry users [buyer, seller]. Identify the liquidator/backstop addresses (validator perps: the HLP liquidation vault; HIP-3: deployer-specific). Test whether trades plus those addresses give a reliable liquidation signal. Document the result in docs/typed-observations.
- **No-Rust path:** Research only.
- **Evidence:** The fetched HL websocket subscription list has no liquidation type. Probe: trades for xyz:NVDA carry users and an all-zero hash.

### `rh-stream` — Streaming chain feed (`eth_subscribe` logs or the sequencer feed)

M8 · L · rust · missing · outbound · PRD §1 §34 §35 S2
After: `rh-activity` (M2), `rt-ws-client` (M2)

- **Have:** No WS client in Cargo.toml. alloy-transport-ws and tokio-tungstenite 0.26.2 are only transitive (Cargo.lock:803-817, 5885-5897) and are not proxy-aware.
- **Need:** Use the rt slice's egress-aware WS client:
  - Subscribe via eth_subscribe('logs') on a provider WS (Alchemy), or read the sequencer feed wss://feed.mainnet.chain.robinhood.com (pre-confirmation messages).
  - Decode swaps and transfers, then write rh_activity rows. This is the same seam as the acct/1 rows (docs/typed-observations § Cache).
  - Open network only until WS-over-SOCKS exists.
- **No-Rust path:** none
- **Evidence:** rg -n -i 'websocket|tungstenite|eventsource|grpc|tonic' src Cargo.toml: none.

### `rt-webhook-signatures` — Configurable webhook signature header / encoding + CRC responder (X, Alchemy)

M8 · S · rust · partial · inbound | config · PRD §15 §35 S3
After: `info-x-ingest` (M4), `rt-bus-dispatch` (M2), `x-public-https-ingress` (M8)

- **Have:** The listener verifies only a hex HMAC in X-Tengu-Signature (webhooks.rs:78, 571-613) or an exact Authorization header (:580-591), and routes POST only (:167-169). X webhooks send `X-Twitter-Webhooks-Signature-OAuth2: sha256=<base64>` and require a GET crc_token challenge.
- **Need:** config (src/config/mod.rs:633-650): `[webhooks.endpoints.<n>] signature = { header = 'x-twitter-webhooks-signature-oauth2', encoding = 'base64', prefix = 'sha256=', secret_env = 'X_CLIENT_SECRET' }`, `crc = true`. inbound: GET /webhooks/:name?crc_token= -> {response_token: 'sha256=<base64 HMAC-SHA256(crc_token)>'}. Deliveries publish to the bus, deduped by post id.
- **No-Rust path:** None; a reverse proxy cannot compute the CRC HMAC. The alternative is the outbound filtered stream (rt-http-stream).
- **Evidence:** read webhooks.rs:78, 167-169, 571-613; https://docs.x.com/x-api/webhooks/introduction.
- **Absorbed `rh-webhook-sig`** — Webhook signature knob for provider pushes (Alchemy X-Alchemy-Signature, bare hex). Add WebhookEndpointConfig.signature_header (default 'x-tengu-signature') and signature_format ('sha256=hex' or 'hex') in src/config/mod.rs:637-663, with validation in webhooks.rs validate_endpoints. Add tests and update docs/webhooks-2026-05-11.md.
  
  This enables Alchemy Notify to feed loop = 'rh_watch'.

### `x-public-https-ingress` — Public HTTPS ingress for push providers

M8 · S · infra · missing · infra · PRD §15 §35 S3
After: `ops-deploy-compose` (M0)

- **Have:** Sandboxes bind the webhook listener to loopback. With NETWORK=tor, containers sit on an internal network that cannot publish ports (docker-compose.tor.yml:13-14). deploy/ contains only cloud-init.yml, install.sh and tor/. rt-webhook-signatures depends on `ops-public-https`, which no report defines.
- **Need:** A Caddy reverse proxy with automatic TLS under deploy/xmarket/: port 443 → 127.0.0.1:<webhooks port>, only /webhooks/<name> paths, request-size and rate limits, and provider IP allowlists where the provider publishes them. Add a per-provider decision table: push (webhooks) or outbound stream (rt-http-stream). Needed only if X webhooks or Alchemy Notify are chosen.
- **No-Rust path:** Yes — reverse proxy and compose config only
- **Evidence:** read docker-compose.tor.yml:1-30; ls deploy → cloud-init.yml, install.sh, tor

### `info-pro-news-licensing` — Reuters / LSEG, Bloomberg, FT, Dow Jones licensing

M8 · S · research · missing · n/a · PRD §15 §34
After: `info-replay` (M7)

- **Have:** Nothing.
- **Need:** Decision memo covering entitlements (machine-readable real-time, storage, derived signals), latency versus wires and X as measured by info-replay, and cost: LSEG news reported from about $25k/yr to six figures (unpublished); Bloomberg B-PIPE is a contract on top of about $3k/mo per Terminal; FT APIs need an FT licence; Dow Jones/Factiva is contract-only. Go/no-go after S8.
- **No-Rust path:** Not applicable (commercial). Integration afterwards is a TOML json/stream row or a small adapter.
- **Evidence:** Web research (URLs in external); nothing in the repo.

### `info-social-extended` — Reddit, Farcaster, Telegram MTProto, YouTube transcripts

M8 · S · research · missing · n/a · PRD §15 §34
After: `info-replay` (M7)

- **Have:** The Telegram adapter is a bot channel (src/adapters/inbound/telegram.rs) and cannot read public channels. The public t.me/s preview works without auth (probe).
- **Need:** Evaluate each: Reddit (non-commercial free at 100 QPM; commercial use needs approval and costs about $0.24/1k calls; unauthenticated requests get 403 per probe), Farcaster via Neynar credits, Telegram private channels (MTProto account), YouTube transcripts (channel RSS titles work: Coin Bureau probe). Decide per the §34 question 'which sources are useful' using info-replay data.
- **No-Rust path:** YouTube and Telegram-preview rows become TOML once info-parsers exist; the others need accounts or contracts.
- **Evidence:** rg -i 't.me/s|tgme_widget|getChatHistory|mtproto|tdlib' → 0.

### `ops-history-cold-tier` — Parquet export or a Postgres / TimescaleDB adapter behind `HistoryStore`

M8 · M · rust · missing · outbound | inbound | infra · PRD §33 §35 S8
After: `ops-history-recorder` (M1)

- **Have:** No columnar or time-series store. Cargo.toml has no arrow, parquet or duckdb (Cargo.toml:12-67) and rust-version = '1.78' (Cargo.toml:5). Postgres is used only for agentic_memory, on image pgvector/pgvector:pg16 (docker-compose.yml:56; DDL src/adapters/outbound/tools/agentic_memory/mod.rs:533-595).
- **Need:** Option A (default): `tengu history export --day <YYYYMMDD> --parquet` (adapters/inbound/cli/history.rs + adapters/outbound/history_parquet.rs) using arrow/parquet 60.0.0 with zstd. Their MSRV is 1.88, so bump rust-version; the Dockerfile toolchain is already rust:1.91 (Dockerfile:2). Output goes to <TENGU_HOME>/state/history/<sandbox>/parquet/<schema>/<day>.parquet, after which SQLite day files past retention are dropped. Option B: adapters/outbound/history_postgres.rs behind the same port, with a TimescaleDB hypertable obs_history and a compression policy on timescale/timescaledb-ha:pg16. That image ships pgvector, but its PGDATA is /home/postgres/pgdata/data, so agentic_memory needs a pg_dump/restore. Compression is under the Timescale License: free when self-hosted, absent from the -oss tags.
- **No-Rust path:** yes for export — run the DuckDB CLI on the host (ATTACH the SQLite day file, COPY TO parquet). Trade-off: manual or cron outside the repo, and not covered by tests.
- **Evidence:** rg -n -i -w 'parquet|arrow|duckdb|timescaledb' Cargo.toml src → only a file-extension list in src/adapters/outbound/tools/workspace/read_file.rs:85; crates.io API probe; timescaledb-docker-ha Dockerfile/versions.yaml probe

### `ops-egress-split-routing` — Per-host Tor / direct split (research over Tor, venue feeds direct)

M8 · M · rust · missing · config | outbound · PRD §15

- **Have:** [egress] network applies to the whole sandbox. EgressConfig fields are network, proxy, route_llm_api, allow_hosts, deny_hosts, https_only, shell_network, audit, audit_log (src/config/egress.rs:26-63). The only split is LLM vs tools (route_llm_api).
- **Need:** config/egress.rs: [egress] direct_hosts = [...] (or proxy_hosts) under network = 'tor'. The egress.rs tool_client and check_url choose the proxied or direct client per host; the setting round-trips through TENGU_EGRESS to child processes; the egress audit records via per host; docs/egress-2026-09-16.md is updated.
- **No-Rust path:** partial — run two sandboxes (research over Tor, markets open) that share Postgres/agentic_memory. Trade-off: two processes, and market loops cannot escalate in-process to the Tor researcher.
- **Evidence:** read src/config/egress.rs:1-80; rg -n 'direct_hosts|proxy_hosts|bypass|no_proxy' src/config/egress.rs → none

### `kg-generic-venue-mapper` — TOML-mapped generic JSON venue source ("other venues added later")

M8 · M · rust · missing · outbound + config · PRD §4 §19
After: `kg-catalog-store` (M1), `kg-equivalence` (M1)

- **Have:** The reducer path grammar exists (src/application/decision_loop/reduce.rs:1-78) but is used only for loop state.
- **Need:** `[xmarket.venues.<name>] url, method, body, items = "/assets/*", fields = { native_id = "/deployments/0/contractAddress", symbol = "/tokenSymbol", isin = "/isin", status = "/status" }, status_map, kind, calendar_id`. A generic adapter builds Instrument rows through the same sync transaction, equivalence rules and lifecycle; hosts come from the tool scope.
- **No-Rust path:** This is the doctrine-#2 path for simple venues after a one-time Rust build. Venues needing multi-call joins (like HL) still need adapters.
- **Evidence:** read reduce.rs; rg -n -i 'venues\.' src/config/ → 0.

### `kg-wiki-notes` — Entity / relationship notes in the LLM Wiki

M8 · S · skill · partial · skill · PRD §5 §11 §33
After: `kg-graph-edges` (M5)

- **Have:** agentic_memory ops capture/recall/ingest_source/promote/compile_wiki/lint (src/adapters/outbound/tools/agentic_memory/mod.rs:218,415-456); wiki root .tengu/agentic-memory/wiki (mod.rs:38). Feature postgres_memory only.
- **Need:** Skill step: the architect captures evidence → promote → compile_wiki(title = "xmarket/<entity_id>"). Optional deterministic `tengu xm export-wiki` renders DB edges with provenance into <workspace>/.tengu/agentic-memory/wiki/xmarket/. Markdown is never read back as truth; the DB stays the system of record.
- **No-Rust path:** Yes: existing agentic_memory ops through a skill. Trade-off: needs postgres_memory + Postgres, and pages are LLM-owned (may paraphrase).
- **Evidence:** read agentic_memory/mod.rs:1-17,210-250,415-456; docs/agentic-memory-implementation-2026-05-13.md:56-66 (wiki is compiled, not a source of truth).

