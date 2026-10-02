# Engine Backends

Tengu supports three engine backends (`openrouter`, `local`, `claude_code`). Each `[agents.<name>]` block selects its backend via `engine = "..."` in [[configuration]]; the planner runs on the `[orchestrator] agent`'s engine, subagents on their own block's engine (`run-agent` reads the same block). Backends are plug-and-play — switching an agent between backends requires only a config change. All LLM traffic follows `[egress]` (`docs/egress-2026-09-16.md`): Tor by default, `network = "open"` for direct.

## OpenRouter (`engine = "openrouter"`)

**Transport:** HTTP JSON to OpenRouter API
**Feature flag:** `openrouter` (default)
**File:** `src/adapters/outbound/engines/mod.rs`

The default backend. Sends chat completions to OpenRouter, which proxies to any supported model (Anthropic, OpenAI, Google, Meta, DeepSeek, etc.).

### How it works
1. `Engine::run()` sends an HTTP request to OpenRouter `/v1/chat/completions`
2. Response is parsed into `StreamEvent::TextDelta` and `StreamEvent::ToolCallStart/Delta/End`
3. Tengu's outer tool loop (`collect_engine_response`) executes tool calls and feeds results back
4. Loop continues until no more tool calls or max rounds exceeded

### Key properties
- `manages_own_workspace()` = `false` — Tengu handles all workspace operations
- `supports_tool_use()` = `true` — tool definitions passed to model
- One API key (`OPENROUTER_API_KEY`) for all models; `OPENROUTER_BASE_URL` overrides the endpoint
- HTTP client from `egress::policy().llm_api_client` — proxied iff `[egress] route_llm_api` (default `true` under `network = "tor"`)
- The system prompt goes out once: a system message equal to the context's `system_prompt` (`run-agent`, `tool turn`, doctor, webhooks send both) is skipped, as on `local`
- Timeouts from `[agents.<id>.limits]`: `request_timeout_secs`, `stream_event_timeout_secs`, `max_output_tokens_per_turn`
- `finish_reason` + the provider's `native_finish_reason` logged at info per turn
- A failed turn — no tool call and no text, an `error` finish (Gemini `MALFORMED_FUNCTION_CALL`, whatever junk text came with it), or a 200 carrying only an `error` object — is logged with the raw body (redacted) and retried once (after an `error` finish with a line saying the malformed call was discarded); a repeated `error` is `StreamEvent::Error`, never its text. Why and the exact rule: `openrouter.rs` module doc
- Pay-per-token pricing
- Browse models at [openrouter.ai/models](https://openrouter.ai/models)

## Local (`engine = "local"`)

**Transport:** HTTP JSON to a locally hosted OpenAI-compatible server (`POST /v1/chat/completions`, OpenAI `tools`)
**Feature flag:** none (always built)
**File:** `src/adapters/outbound/engines/local.rs` (`LocalEngine`)

| Server | Start | `base_url` | Key |
|---|---|---|---|
| Unsloth (default) | `unsloth run --model unsloth/gemma-4-26B-A4B-it-GGUF:UD-Q4_K_XL` | `http://127.0.0.1:8888` | `sk-unsloth-…` in `$UNSLOTH_API_KEY` (Settings → API) |
| Ollama | `ollama serve` | `http://127.0.0.1:11434/v1` (a trailing `/v1` is dropped; the root works too) | none (`api_key_env = ""`) |
| llama.cpp | `llama-server -m model.gguf --jinja` | `http://127.0.0.1:8080` | none |
| vLLM / LM Studio | `vllm serve …` / LM Studio server | `:8000` / `:1234` | none |

```toml
[agents.local]
engine = "local"
model = "unsloth/gemma-4-26B-A4B-it-GGUF:UD-Q4_K_XL"   # verbatim; Unsloth: GET /v1/models
description = "Private/offline work on the local model."
[agents.local.limits]
context_window = 32000          # REQUIRED in practice — default is 1_000_000 (load warns)
request_timeout_secs = 900      # local generation is slow
[agents.local.local]            # optional; defaults shown
base_url = "http://127.0.0.1:8888"
api_key_env = "UNSLOTH_API_KEY" # unset/empty env → no Authorization header
```

| Property | Value |
|---|---|
| Network | Direct connection (`no_proxy`), not part of `[egress]` — works with Tor on or down. Keep `base_url` on this host / LAN |
| Tool calling | model/server dependent — needs a tool-capable chat template (llama.cpp: `--jinja`) |
| Planner role | OK (plain chat completions, tools stripped like OpenRouter) — small models may emit bad plan JSON (3 retries) |
| `unsloth start <agent>` | Not used — that launches *external* agent CLIs (Claude Code, Codex, Hermes…) against Unsloth. Tengu talks to the server directly |
| Verified | 2026-09-23: `run-agent` step on Ollama `gemma4:latest`, Tor-default policy with Tor down → `compress_and_store` called, `status=ok` |

### Tool results fit the window (`x-local-model-fit`)

| Mechanism | Rule |
|---|---|
| Per-result cap | `LocalEngine::tool_result_char_cap` = 1/8 of `limits.context_window` at 4 chars/token (`domain::token::tool_result_char_budget`): 16 384 → 8 192 chars; never above `max_tool_result_chars`; the `[truncated — showing X of Y chars]` footer counts inside it |
| Typed rows | A row that fits the cap arrives whole — its `data` holds the ids a follow-up call targets (a position's instrument, size, exit deadline) and no tool reads a row back by key. Above the cap, `Observation::compact_text`: line 1, features, errors and any note the tool appended stay; `data` → `data: <n> bytes in observation <key>` (full key) |
| Where | Both loops that feed tool results to a model: `chat/tool_loop.rs::collect_engine_response` (TUI, Telegram, webhooks, eval) and `run-agent`, which for local agents also cuts older rounds to line 1 (≤ `compact_result_limit`) like the in-process loop. OpenRouter and Claude Code are unchanged; decision loops feed no tool text to a model |
| System prompt | A system message repeating `EngineContext.system_prompt` is sent once (`run-agent` passes both) |
| Load warning | `Config::load` warns when a `local` agent keeps the 1 000 000 default `context_window` |

### Ollama `gemma4:latest` — working settings

| Setting | Value |
|---|---|
| Model | `gemma4:latest`: 8.0B, Q4_K_M, 9.6 GB, tools + thinking (`ollama show gemma4:latest`, Ollama 0.24.0) |
| `base_url` | `http://127.0.0.1:11434/v1`, `api_key_env = ""` |
| Model max window | 131 072 tokens (`ollama show` → context length) |
| Served window | `num_ctx` = `OLLAMA_CONTEXT_LENGTH`, else 4k / 32k / 256k by VRAM (`ollama serve --help`). The OpenAI endpoint cannot set it per request, and a longer prompt is truncated without an error (older messages dropped first — the goal and earlier results vanish) |
| Raise it | Server-wide: `OLLAMA_CONTEXT_LENGTH=16384 ollama serve` (app: Settings → Context length). One model: a Modelfile `FROM gemma4:latest` + `PARAMETER num_ctx 16384`, `ollama create gemma4-16k -f Modelfile`, `model = "gemma4-16k"`. Check `ollama ps` (CONTEXT) after the first call |
| `limits.context_window` | The served `num_ctx`: start at `16384` on a 16 GB Mac (`ollama ps`: CONTEXT 16384, PROCESSOR ideally 100% GPU), `32768` with memory to spare |
| Tool-result cap | Automatic: 8 192 chars at 16 384 (16 384 at 32 768) |
| `limits.max_tool_rounds` | `10` (default 70 is sized for hosted models; every local round re-reads the prompt) |
| `limits.request_timeout_secs` | `900` |

```toml
[agents.gemma]
engine = "local"
model = "gemma4:latest"
description = "Offline work on the local gemma4 model."
[agents.gemma.limits]
context_window = 16384          # = the num_ctx Ollama serves
max_tool_rounds = 10
request_timeout_secs = 900
[agents.gemma.local]
base_url = "http://127.0.0.1:11434/v1"
api_key_env = ""
```

Live check: § Engine matrix below — the local legs run against the operator's PC over the LAN, never a model on the Mac.

## Claude Code (`engine = "claude_code"`)

**Transport:** `claude` CLI subprocess, `-p --output-format stream-json` (NDJSON on stdout)
**Feature flag:** `claude_code` (opt-in: `cargo build --features claude_code`)
**File:** `src/adapters/outbound/engines/claude_code.rs`

Runs agents through the local Claude Code CLI. Uses the operator's Claude subscription instead of API tokens (`ANTHROPIC_API_KEY` is removed from the child env). A parent Claude Code session's env is removed too (`claude_code.rs::parent_session_env`: `CLAUDECODE`, `CLAUDE_PID`, `CLAUDE_EFFORT`, every `CLAUDE_CODE_*` but auth / provider), so a nested `claude` — tengu run from inside Claude Code — starts like one from the operator's terminal.

### How it works
1. `Engine::run()` formats the conversation into one prompt and spawns `claude -p --output-format stream-json --verbose --dangerously-skip-permissions --no-session-persistence --strict-mcp-config [--model <bare slug>] --system-prompt <sp> --tools <profile list>` with `current_dir` = workspace (args: `claude_code.rs::cli_args`); the system prompt rides on `--system-prompt` only — a system message equal to it is left out of the stdin prompt (`format_prompt`); profile `none` adds `--setting-sources "" --disable-slash-commands --settings {"autoMemoryEnabled":false,"disableAllHooks":true}` (§ Safety Policy). A `run-agent` step builds it with `engines::build_step_engine` (`StepOpts`): its bridge grants the workspace (`TENGU_BRIDGE_GRANT_WORKSPACE`) and serves `compress_and_store` into the step's summary file (`TENGU_BRIDGE_SUMMARY_FILE`, the step's IPC summary) — [[mcp-bridge]] § Dispatch. A step always has a workspace, so the bridge always starts: the agent's, else a `tengu-step-*` temp dir for that step, removed after it (`bootstrap::tools::workspace_or_temp`; the executor uses the same dir, the memory store then sits at `[memory] store_path`; a webhook or `tengu tool turn` one-shot likewise, per turn). `tengu eval` sets `StepOpts.config_file` instead: the bridge loads the expanded eval config (`TENGU_CONFIG`) and runs tools as that eval agent
2. `[egress]`: with `route_llm_api` (default under Tor) the child gets `HTTPS_PROXY` / `HTTP_PROXY` = HTTP CONNECT form of the proxy (`socks5h://h:p` → `http://h:p`, Arti serves CONNECT on 9050) and `NO_PROXY=localhost,127.0.0.1,::1` (`egress::claude_cli_env`)
3. Claude CLI loads its own CLAUDE.md and native tools — not under profile `none`, which loads no settings file, hook, plugin, skill, CLAUDE.md / AGENTS.md or auto-memory. MCP: `--strict-mcp-config` on every run — only the `--mcp-config` servers (the tengu bridge); the operator's user / project / plugin MCP servers are never loaded (no bridge ⇒ no MCP server)
4. Tengu tools (`EngineContext.bridge_tools`) are written to a temp `--mcp-config` that launches `tengu mcp-bridge` ([[mcp-bridge]]); each is allow-listed as `--allowedTools mcp__tengu-tools__<name>`; the sandbox's `[[mcp_servers]]` reach Claude only through the bridge — named in the file, never their config (a `${VAR}` value `Config::load` expanded stays out of it): the bridge takes them from the config it loads
5. Claude executes the full prompt internally (may use many tools across multiple turns). The run's conversation — the given messages, then every streamed assistant message and tool result — sits in a 0600 temp file the bridge reads per call (`TENGU_BRIDGE_TRANSCRIPT_FILE`, `claude_code.rs::Transcript`): a bridged tool sees what an in-process one sees (`skill_distill` fixtures)
6. NDJSON `assistant` text → `StreamEvent::TextDelta`; `result` → `StreamEvent::Done`; each `tool_use` → `tool_result` pair → `StreamEvent::ToolRan` (name without `mcp__tengu-tools__`, `ok = !is_error`) — the activity record (`EngineResponse.tool_runs`, IPC `AgentIpcOutput.tools`). A successful `compress_and_store` (the bridge answers `stored — stop now`) ends the run once the round's other calls are answered — the CLI is stopped, as the in-process loop stops after its round
7. Tengu's outer tool loop sees no tool calls — passes through immediately

| `limits.max_tool_rounds` | Counts |
|---|---|
| `openrouter`, `local` (chat, `run-agent`, webhooks, `tool turn`, doctor) | engine turns (one may call several tools) |
| `claude_code` | tool calls (`tool_use` blocks, bridged and built-in) of one CLI run; past the cap the CLI is killed with an error |

### Key properties
- `manages_own_workspace()` = `true` — Claude handles Read/Write/Edit/Bash natively
- `supports_tool_use()` = `true` — tools handled internally
- Each turn spawns a fresh CLI process (~1-2s overhead)
- Conversation history formatted into the prompt (stateless sessions)
- Idle timeout between stream events = `[agents.<id>.limits] stream_event_timeout_secs`; `[claude_code] cli_path` locates the binary
- `model` is the bare slug (`claude-sonnet-4-6`), not the OpenRouter form
- Uses Claude subscription, no per-token cost

### Builtin Tools Profiles

Configured via `[agents.<id>.claude_code].builtin_tools_profile` (default `editor_shell`; read trimmed — `" none"` is `none` — and an unknown value is a load error, never a silent `editor_shell`). Applies to subagents too — `run-agent` builds the child engine from the parent's `[agents.<name>]` block (`bootstrap::tools::subagent_config` → `adapters::outbound::engines::build_step_engine`).

| Profile | Claude Native Tools (`--tools`) |
|---------|-------------------|
| `none` | (none) |
| `read_only` | Read, Glob, Grep |
| `editor` | Read, Glob, Grep, Edit, Write, MultiEdit |
| `editor_shell` | Read, Glob, Grep, Edit, Write, MultiEdit, Bash |

`[egress]`: while a proxy is set (default under Tor), `editor_shell` launches as `editor` — builtin Bash has no egress control (`egress::claude_code_profile`, warn logged).

### Safety Policy

The CLI runs with `--dangerously-skip-permissions`; there is no per-call permission callback. What is enforced:

| Layer | Mechanism |
|---|---|
| Builtin tools | `--tools` per profile (above) |
| MCP servers | `--strict-mcp-config`: the tengu bridge only, never the operator's own |
| Settings, hooks, plugins, instructions (profile `none`) | `--setting-sources ""` (no user / project / local settings files: their hooks, installed plugins, `env`; no project CLAUDE.md / AGENTS.md discovery), `--disable-slash-commands` (no skills / custom commands), `--settings {"autoMemoryEnabled":false,"disableAllHooks":true}` (no auto-memory, no hook from any source). Probed on CLI 2.1.286 (`system/init`: no hook ran, only built-in plugins, 0 skills, project memory off, bridge `connected`) and live (`claude_code_workspace` leg, subscription OAuth). Rejected: `--safe-mode` (drops the `--mcp-config` bridge too), `--bare` (reads no OAuth / keychain) |
| Tengu tools | `--allowedTools mcp__tengu-tools__<name>` for each bridged tool only |
| Hardened sandbox (`src/config/hardening.rs`: a `[solana]` signer or `[risk]`, one code path) | load refuses a `claude_code` agent unless `builtin_tools_profile = "none"` (no block = `editor_shell` = refused), any `[[mcp_servers]]`, any scope granting `shell_bins`, and an fs root / workspace reaching the signer key, `<TENGU_HOME>/state`, the kill-switch file or the config file; every agent's fallback scope runs no shell (`no_shell_fallback`); a plan step's `compose` only narrows its base agent (`bootstrap::tools::compose_agent`) |
| Per-tool scopes | `[default_scopes.*]` / `[agents.<id>.scopes.*]` exported as `TENGU_BRIDGE_SCOPES`; the bridge enforces them (`fs_roots` includes the child workspace, except on a deny-all scope) |
| Network | CLI API traffic via `HTTPS_PROXY` (advisory); bridge tools via `TENGU_EGRESS` (enforced); builtin Bash dropped under a proxy |

Not enforced by the engine: destructive-Bash patterns, `skills/` write denial, workspace containment for builtin tools (the CLI is only started with `current_dir` = workspace).

### Operator rules (2026-09-30)

| Rule | State |
|---|---|
| Every tengu tool works 100 % under all three engines — `openrouter`, `local` (in-process) and `claude_code` (through the bridge, exactly as in-process) — no exceptions | Rule in CLAUDE.md / AGENTS.md step 4; milestone E0 in `docs/xmarket-tracker-2026-09-29.md` closed 2026-10-01 (`x-engine-parity-audit`: every catalog tool, a shell skill and an `[[mcp_servers]]` proxy live on OpenRouter and the Claude CLI; the `local` legs wait for the operator's PC) — what stays open: [[mcp-bridge]] § Parity rule |
| Where money or signing is involved (a `[risk]` sandbox, a Solana signer), `claude_code` agents are allowed only hardened: `builtin_tools_profile = "none"` + `--strict-mcp-config` | Done (`x-claude-code-hardening`, `risk-gate-enforcement`): `--strict-mcp-config` on every run; load rules in `src/config/hardening.rs` for a signer and `[risk]` alike (replaced the signer's blanket `claude_code` refusal); 2026-10-01: profile `none` also runs without settings files, hooks, plugins, skills, CLAUDE.md and auto-memory |

### MCP Bridge

See [[mcp-bridge]] for how Tengu-native tools (http_request, crypto, cache, skills) are exposed to Claude.

## Engine matrix (`x-engine-matrix-smoke`)

One scripted turn per engine × model × tool set (`tests/engine_matrix.rs`). Every catalog tool, a shell skill and an `[[mcp_servers]]` proxy sit in a set — `every_catalog_tool_has_a_live_leg` fails CI for a catalog tool in no set. Two fixture families, each `{openrouter,claude_code,local}.toml` with identical sections but `[agents.*]`: `tests/fixtures/engine_matrix/` (hardened: `[risk]` + `[xmarket]` + `[paper]`) and `tests/fixtures/engine_matrix/open/` (memory on, a shell, the `matrix` `[[mcp_servers]]` server `token_mcp_server.sh`, no signer). Sets run through `tengu run-agent` on the routable agent of each engine × model (the set via IPC `compose`, the shell skill via `compose.skills`); configured scopes exclude the workspace, so calls also prove the `run-agent` workspace grant (the bridge's for Claude Code). The xm set holds exec tools, which only a private agent may hold (no `description`) and `run-agent` never runs — it runs through the hidden `tengu tool turn` (one in-process engine turn as any agent, the `@<agent>` chat path, `chat:` call ids; Claude Code through its bridge) on the private `xm_*` agent, with scopes naming the workspace. A leg passes when every tool of the set ran without error (`tools` activity; `privy_off` must be refused), no `compress_and_store` failed, no registered secret reached the output, and the results reached the answer.

| Tool set (fixture) | Calls | Result read = |
|---|---|---|
| workspace (hardened) | `list_directory`, `read_file` (token file), `write_file` `answer.txt`, `read_file` (a registered secret, `TENGU_SECRETS_LOADED`) | the token in the answer and in `answer.txt`; `REDACTED` in the answer, the value nowhere in stdout / stderr; Claude Code: the bridge's result logged as `[REDACTED]` |
| hyperliquid (hardened) | `hl_ctx` `{"coins": ["xyz:TSLA"]}`, `hl_book` `{"coin": "xyz:TSLA"}` — live, read-only | a number of each stored row's headline (`mkt_ctx/1`, `hl_book/1`) |
| xm (hardened) | `hl_ctx` `xyz:TSLA` (live) → `paper_order` $15 market buy naming a seeded opportunity row → `paper_positions` → `paper_close` → a second $15 buy with `exit_at_ms` in the past → `xm_exits` → `risk_status` → `xm_weekend_fade` (one step of rule W on `xyz:TSLA`: `waiting` outside the weekend) — `[xmarket]` + `[risk]` + `[paper]` + `[xmarket.weekend_fade]` + the recorder, a new ledger in a temp `TENGU_HOME`; paper only | the first buy's `avg_px` in the answer; the ledger holds both filled buys, the filled `paper_close` sell and the filled `xm_exits` sell under `exit:matrix:hyperliquid:xyz:TSLA:deadline:<opened_ms>`, each keyed on its `chat:` / `mcp:` call id, no open position, and the fade's `matrix-shadow` account; `logs/risk.jsonl` has the exit's verdict |
| shell (open) | `run_command` `cat shell-token.txt`, the shell skill `matrix_cat` (`tests/fixtures/skills/matrix_cat`), the proxy `matrix__token` | three tokens (the MCP one only in the server's env: `$TENGU_MATRIX_MCP_VALUE`, resolved by the child / the bridge) |
| memory (open) | `memory_ingest`, `memory_search`, `persistent_store` `store` + `search` — embeddings via `OPENROUTER_API_KEY` | `memo.txt`'s token |
| skills (open) | `view_skill`, `skill_resource` (workspace skill `matrix-doc`), `manage_skill` `create`, `skill_distill` (`from_message_index` 1), `apply_improver_proposal` | the doc + resource tokens; the new skills under `<ws>/.tengu/skills/`, a fixture from the goal in the distilled skill's `evals/prompts.yaml` (Claude Code: from the engine's transcript), the improved body |
| util (open) | `shared_cache` `put` + `get`, `http_request` (a loopback endpoint), `abi_encode`, `hex_to_uint256` | the endpoint's token, the decimal of a random hex |
| privy_off (open) | `sign_message`, `sign_and_send_transaction` — scopes without wallets | both refused before any request; the refusal quoted. Never signs |
| privy (open) | `get_wallet_address` — only with `PRIVY_APP_ID` / `_SECRET` / `_WALLET_ID` (env or the repo's `.env`), else skipped | the address (`PRIVY_WALLET_ADDRESS`); the app secret (registered) nowhere in the output |
| solana_read (open) | `sol_price`, `dlmm_pools`, `dlmm_pool`, `dlmm_positions`, `jup_perps`, `solana_wallet`, `solana_tx` — public mainnet RPC (`SOLANA_RPC_URL` cleared) | every tool stored a row; numbers of the `sol_price` and pool rows (within 1e-5) |
| solana_decide (open) | `lp_snapshot`, `hedge_decide`, `lp_decide` (all knobs; `commit` off) | the snapshot's oracle price (`price_usd`, `cycle_price`) |
| solana_write (open) | `solana_close_token_accounts`, `jupiter_swap`, `dlmm_open_position`, `dlmm_close_position` (a live position of the LP owner), `jup_perps_order` — `simulate` only (no signer, no wallet grant) | `simulated` in the answer. Never sends |
| agentic_memory (open) | `agentic_memory` `capture` → `recall` — only with `--features postgres_memory` + `TENGU_MEMORY_DATABASE_URL`, else skipped | the captured token |

| Command | Runs |
|---|---|
| `cargo test --features claude_code --test engine_matrix -- --ignored --nocapture --test-threads 1` | every live leg (52: 13 sets × 4 targets); one `engine_matrix \|` line each (secs, tokens, Claude CLI cost, or why it skipped); local legs skip without `TENGU_MATRIX_LOCAL_BASE_URL`. Run it in chunks of ≤ 13 legs (e.g. the `openrouter_gemini_` filter) to stay under a 10-minute call |
| `cargo test --test engine_matrix` | offline, in CI: the local path against a scripted OpenAI-compatible mock (workspace and shell sets via `run-agent`; `risk_status` + `paper_positions` via `tool turn`), fixture checks, the catalog-coverage check |
| `tengu -c tests/fixtures/engine_matrix/<engine>.toml doctor --engines` | per agent: `list_directory` + `read_file` in a temp workspace → agent · engine · model · ok · tools called · secs; non-zero exit on a failure; on macOS a `local` agent with a loopback `base_url` prints `skipped (local models run on the operator's PC)` and is never contacted |

Results 2026-10-01, final pass of `x-engine-parity-audit` (Mac; Claude CLI 2.1.286; OpenRouter list prices; secs per leg):

| Set | gemini-2.5-flash-lite | claude-haiku-4.5 (OpenRouter) | claude_code `claude-haiku-4-5`, built-ins off |
|---|---|---|---|
| workspace | ✅ 2.9 | ✅ 8.7 | ✅ 14.2 |
| hyperliquid | ✅ 6.4 | ✅ 7.6 | ✅ 14.0 |
| xm (`tool turn`) | ✅ 9.2 | ✅ 18.9 | ✅ 24.6 |
| shell | ✅ 3.7 | ✅ 6.9 | ✅ 12.9 |
| memory | ✅ 17.2 | ✅ 8.5 | ✅ 17.6 |
| skills | ✅ 3.8 | ✅ 10.8 | ✅ 19.2 |
| util | ✅ 5.3 | ✅ 9.7 | ✅ 15.0 |
| privy_off | ✅ 2.2 | ✅ 4.3 | ✅ 10.3 |
| privy | ✅ 1.9 | ✅ 2.7 | ✅ 13.9 |
| solana_read | ✅ 7.9 | ✅ 21.2 | ✅ 40.2 |
| solana_decide | ✅ 5.7 | ✅ 11.9 | ✅ 25.3 |
| solana_write | ✅ 4.5 | ✅ 24.4 | ✅ 29.3 |
| agentic_memory | skipped (needs `--features postgres_memory`) | skipped | skipped |
| Tokens in / out · cost | 85 735 / 7 126 · ≈ $0.011 | 257 348 / 7 126 · ≈ $0.29 | subscription (CLI: $0.371 API-equivalent) |

| Also | Result |
|---|---|
| local · `gemma4:latest` | 13 legs skipped (`TENGU_MATRIX_LOCAL_BASE_URL` not set) — run on the operator's PC (below); offline mock legs pass |
| `tengu doctor --engines` | `claude_code.toml`: claude ✅ 5.6 s, xm_claude ✅ 6.2 s (bridge grant via `StepOpts`, no process-wide env); `openrouter.toml`: gemini ✅ 1.8 s, haiku ✅ 4.9 s, xm_gemini ✅ 1.5 s, xm_haiku ✅ 3.3 s |

| Seen | Detail |
|---|---|
| gemini-2.5-flash-lite flakes (building the audit; 0 in the final pass) | invented tool names (`native_finish_reason` `UNEXPECTED_TOOL_CALL`; now recovers from the "Its tools: …" error), `MALFORMED_FUNCTION_CALL` with `<ctrl46>` text (now retried with a hint, an error if it repeats — never the answer), an empty first turn then an invented summary without tool calls, a dropped required argument (`manage_skill` `action`). Rerun the leg |
| Claude Code `compress_and_store` | served by the step's bridge since 2026-10-01: no failed call left; the IPC `summary` is the bridged one |

Local leg — the operator's Windows PC over the LAN:

| Where | Step |
|---|---|
| PC | Ollama serving on the LAN with a 16k window: user env `OLLAMA_HOST=0.0.0.0:11434`, `OLLAMA_CONTEXT_LENGTH=16384`, restart Ollama; `ollama pull gemma4:latest`; allow TCP 11434 inbound on the private network |
| Mac (matrix) | `TENGU_MATRIX_LOCAL_BASE_URL=http://<windows-pc>:11434/v1 CARGO_TARGET_DIR=$HOME/.cache/tengu-xm.noindex/main CARGO_BUILD_JOBS=2 cargo test --test engine_matrix local_ -- --ignored --nocapture --test-threads 1` |
| Mac (doctor) | `TENGU_MATRIX_LOCAL_BASE_URL=http://<windows-pc>:11434/v1 CARGO_TARGET_DIR=$HOME/.cache/tengu-xm.noindex/main CARGO_BUILD_JOBS=2 cargo run -- -c tests/fixtures/engine_matrix/local.toml doctor --engines` |

## Adding a New Backend

To add a new engine backend:

1. Create `src/adapters/my_engine.rs` implementing the `Engine` trait
2. Add a feature flag in `Cargo.toml`
3. Register in `src/adapters/mod.rs` (feature-gated)
4. Add dispatch in `adapters/outbound/engines/mod.rs` `build_engine()` (the planner uses the `[orchestrator] agent`'s engine; `build_planner_engine()` is unused)
5. Add engine name to config validation in `config.rs` `validate_agent()`
6. If the engine manages its own workspace, set `manages_own_workspace() = true` and use `bridge_tools` from `EngineContext`
7. Build its HTTP client with `egress::policy().llm_api_client` (or pass `claude_cli_env()` to a subprocess) — a bare `reqwest::Client` bypasses `[egress]`
8. Engine matrix: a fixture under `tests/fixtures/engine_matrix/` + its legs in `tests/engine_matrix.rs` (§ Engine matrix); an engine that runs tools itself emits `StreamEvent::ToolRan` per call

## Related
- [[architecture]] — system overview
- [[configuration]] — config reference
- [[mcp-bridge]] — tool bridging for self-managing engines
