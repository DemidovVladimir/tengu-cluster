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
| `crypto/` | `sign_and_send_transaction`, `sign_message`, `get_wallet_address`, `abi_encode`, `hex_to_uint256` | always |
| `skill_resource/`, `view_skill/` | `skill_resource`, `view_skill` | always |
| `cache/` | `shared_cache` | opt-in |
| `agentic_memory/` | `agentic_memory` | opt-in, feature `postgres_memory` |
| `skill_lifecycle/` | `skill_distill`, `apply_improver_proposal` (+ implicit `compress_and_store`) | opt-in |
| `manage_skill/` | `manage_skill` | opt-in |
| `solana/` | reads: `sol_price`, `dlmm_pools`, `dlmm_pool`, `dlmm_positions`, `jup_perps`, `solana_wallet`, `solana_tx`, `lp_snapshot`, `hedge_decide`, `lp_decide` — typed, cached; writes: `solana_close_token_accounts`, `jupiter_swap`, `dlmm_open_position`, `dlmm_close_position`, `jup_perps_order` — `mode = simulate` (default) \| `send` (args / keys / hosts / send rules: `docs/typed-observations-2026-09-24.md`) | opt-in, one row per name |
| `hyperliquid/` | `hl_ctx` — typed, cached market context (`mkt_ctx/1`, `mkt_instrument/1`, `hl_sweep/1`); `hl_book` — L2 book, depth, slippage per notional (`hl_book/1`); args / keys / weights: `docs/typed-observations-2026-09-24.md` § Hyperliquid tools | opt-in, one row per name |

The list is `catalog()` in `tools/mod.rs` — one `ToolEntry` row per group. That row drives in-process registration, the MCP bridge (Claude Code subagents), and the tool list the model sees.

## Add a Rust tool

1. `src/adapters/outbound/tools/<name>/mod.rs`: `impl Tool` (from `ports::tool`), a `ToolPlugin`, `tool_defs()`. First line of `execute` = `ctx.scope.check_*(..)` or `// scope: pure-compute` (`tests/scope_lint.rs`).
2. `pub(crate) mod <name>;` + one `ToolEntry` in `catalog()`.
3. Opt-in only: add the name to `src/domain/tools.rs::WORKSPACE_TOOLS` (`catalog_tests` fail otherwise).
4. `cargo test --bin tengu -- catalog schema_lint && cargo test --test scope_lint && cargo test --test bridge_conformance`.
5. Every engine — no exceptions (operator rule 2026-09-30): the tool must work the same under `engine = "openrouter"` and `"local"` (in-process) and `"claude_code"` (through `tengu mcp-bridge`). Keep the input schema in the subset all three accept (§ Tool schema subset — the lint covers every catalog row automatically), keep results within a local model's context window, and add:
   - a bridge conformance case — one `case("<tool>", json!({..}))` row in `tests/bridge_conformance.rs::cases()` with its scope TOML, mock replies (fixtures under `tests/fixtures/<area>/`) and expected outcome; `bridge_conformance` fails for a catalog tool without one (`docs/mcp-bridge.md` § Testing);
   - its engine-matrix smoke case (milestone E0 in `docs/xmarket-tracker-2026-09-29.md`).

   Open parity gaps: `docs/mcp-bridge.md` § Parity rule.

## Tool schema subset

`cargo test --bin tengu schema_lint` (`tools/schema_lint.rs`) checks every definition an engine can receive — every catalog row (opt-ins, memory, `agentic_memory` under `postgres_memory`), `compress_and_store`, `[[mcp_servers]]` tools (fixture server) and shell-skill tools (fixture SKILL.md) — and lists every violation at once.

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
| `tools` | Allow-list for plan-step runs (`tengu run-agent`). Empty = every always-on tool plus configured opt-ins. Opt-in names listed here are switched on. |
| `workspace_tools` | Older way to switch on opt-in tools; merged with `tools`. |
| `scopes.<tool>` | `fs_roots`, `net_hosts`, `env_reads`, `shell_bins`, `wallets`. Per-agent entry replaces `default_scopes` wholesale. |
| Solana tools | need `fs_roots` = the workspace (observation store), `net_hosts` per tool, `env_reads = ["SOLANA_RPC_URL"]` (else the public RPC is used silently). Working example: `sandboxes/lping/config.toml` |
| Hyperliquid tools | need `fs_roots` = the workspace (observation store), `net_hosts = ["api.hyperliquid.xyz"]`, `env_reads = ["HL_API_URL"]` (testnet override; unset = mainnet). Request weight budgets against `[rate_limits.hyperliquid]`. Example: `config.example.toml` |
| Solana write tools, `mode = "send"` | `[solana] signer_key_file` + `wallets = ["<full pubkey>"]` in that ONE agent's own scope for the tool (never `[default_scopes]`; the agent has no `description`); signing-sandbox rules in `src/config/solana.rs`. Without both they only simulate |
| `compress_and_store` | Added to every subagent automatically — never list it. |

## MCP servers

```toml
[[mcp_servers]]
name = "github"                  # tools appear as "github__<tool>"
transport = "stdio"              # or "http" + url = "..." + auth = { type = "bearer", token = "$TOKEN" }
command = ["npx", "-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "$GITHUB_TOKEN" }
```

List them in an agent's `tools` like any other tool (`tools = ["github__create_issue"]`); an empty `tools` gets every one.

| Agent kind | Sees `[[mcp_servers]]` tools? | How |
|---|---|---|
| Plan-step subagent, `engine = "openrouter"` | yes | `run-agent` executor connects, advertises them |
| Plan-step subagent, `engine = "claude_code"` | yes | engine passes the servers to the tengu bridge (`TENGU_BRIDGE_MCP_SERVERS`), which proxies them under the egress policy — as `mcp__tengu-tools__<server>__<tool>` |
| In-process OpenRouter agent (TUI, Telegram) | yes | same executor |
| In-process Claude Code agent (TUI, Telegram) | yes | servers listed once at agent setup and added to the bridge list; bridge proxies them |
| Webhook agent, `engine = "claude_code"` | yes | webhook turn hands its executor's tool list + servers to the bridge (`webhooks.rs::bridge_inputs`) |

Names use `__` because model APIs reject `.` in tool names.
