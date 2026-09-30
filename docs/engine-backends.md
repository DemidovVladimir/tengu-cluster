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
- Timeouts from `[agents.<id>.limits]`: `request_timeout_secs`, `stream_event_timeout_secs`, `max_output_tokens_per_turn`
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
| Typed rows | `Observation::compact_text`: line 1, features, errors and any note the tool appended stay; `data` → `data: <n> bytes in observation <key>` (full key) |
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

Live check: `x-engine-matrix-smoke` (E0, `docs/xmarket-tracker-2026-09-29.md`).

## Claude Code (`engine = "claude_code"`)

**Transport:** `claude` CLI subprocess, `-p --output-format stream-json` (NDJSON on stdout)
**Feature flag:** `claude_code` (opt-in: `cargo build --features claude_code`)
**File:** `src/adapters/outbound/engines/claude_code.rs`

Runs agents through the local Claude Code CLI. Uses the operator's Claude subscription instead of API tokens (`ANTHROPIC_API_KEY` is removed from the child env).

### How it works
1. `Engine::run()` formats the conversation into one prompt and spawns `claude -p --output-format stream-json --verbose --dangerously-skip-permissions --no-session-persistence --strict-mcp-config [--model <bare slug>] --system-prompt <sp> --tools <profile list>` with `current_dir` = workspace (args: `claude_code.rs::cli_args`)
2. `[egress]`: with `route_llm_api` (default under Tor) the child gets `HTTPS_PROXY` / `HTTP_PROXY` = HTTP CONNECT form of the proxy (`socks5h://h:p` → `http://h:p`, Arti serves CONNECT on 9050) and `NO_PROXY=localhost,127.0.0.1,::1` (`egress::claude_cli_env`)
3. Claude CLI loads its own CLAUDE.md and native tools. MCP: `--strict-mcp-config` on every run — only the `--mcp-config` servers (the tengu bridge); the operator's user / project / plugin MCP servers are never loaded (no bridge ⇒ no MCP server)
4. Tengu tools (`EngineContext.bridge_tools`) are written to a temp `--mcp-config` that launches `tengu mcp-bridge` ([[mcp-bridge]]); each is allow-listed as `--allowedTools mcp__tengu-tools__<name>`; the sandbox's `[[mcp_servers]]` reach Claude only through the bridge
5. Claude executes the full prompt internally (may use many tools across multiple turns)
6. NDJSON `assistant` text → `StreamEvent::TextDelta`; `result` → `StreamEvent::Done`
7. Tengu's outer tool loop sees no tool calls — passes through immediately

### Key properties
- `manages_own_workspace()` = `true` — Claude handles Read/Write/Edit/Bash natively
- `supports_tool_use()` = `true` — tools handled internally
- Each turn spawns a fresh CLI process (~1-2s overhead)
- Conversation history formatted into the prompt (stateless sessions)
- Idle timeout between stream events = `[agents.<id>.limits] stream_event_timeout_secs`; `[claude_code] cli_path` locates the binary
- `model` is the bare slug (`claude-sonnet-4-6`), not the OpenRouter form
- Uses Claude subscription, no per-token cost

### Builtin Tools Profiles

Configured via `[agents.<id>.claude_code].builtin_tools_profile` (default `editor_shell`). Applies to subagents too — `run-agent` builds the child engine from the parent's `[agents.<name>]` block (`bootstrap::tools::subagent_config` → `adapters::outbound::engines::build_engine`).

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
| Tengu tools | `--allowedTools mcp__tengu-tools__<name>` for each bridged tool only |
| Hardened sandbox (`src/config/hardening.rs`: a `[solana]` signer) | load refuses a `claude_code` agent unless `builtin_tools_profile = "none"` (no block = `editor_shell` = refused); every agent's fallback scope runs no shell (`no_shell_fallback`) |
| Per-tool scopes | `[default_scopes.*]` / `[agents.<id>.scopes.*]` exported as `TENGU_BRIDGE_SCOPES`; the bridge enforces them (`fs_roots` includes the child workspace) |
| Network | CLI API traffic via `HTTPS_PROXY` (advisory); bridge tools via `TENGU_EGRESS` (enforced); builtin Bash dropped under a proxy |

Not enforced by the engine: destructive-Bash patterns, `skills/` write denial, workspace containment for builtin tools (the CLI is only started with `current_dir` = workspace).

### Operator rules (2026-09-30)

| Rule | State |
|---|---|
| Every tengu tool works 100 % under all three engines — `openrouter`, `local` (in-process) and `claude_code` (through the bridge, exactly as in-process) — no exceptions | Rule in CLAUDE.md / AGENTS.md step 4; milestone E0 in `docs/xmarket-tracker-2026-09-29.md` (schema lint, bridge parity + conformance, local context fit, live engine-matrix smoke on OpenRouter, Ollama `gemma4:latest` and the Claude CLI); the bridge's parity gaps are listed in [[mcp-bridge]] § Known Limitations |
| Where money or signing is involved (a `[risk]` sandbox, a Solana signer), `claude_code` agents are allowed only hardened: `builtin_tools_profile = "none"` + `--strict-mcp-config` | Done for a Solana signer (`x-claude-code-hardening`): `--strict-mcp-config` on every run; load rule in `src/config/hardening.rs` (replaced the signer's blanket `claude_code` refusal). `[risk]` joins the predicate with its section |

### MCP Bridge

See [[mcp-bridge]] for how Tengu-native tools (http_request, crypto, cache, skills) are exposed to Claude.

## Adding a New Backend

To add a new engine backend:

1. Create `src/adapters/my_engine.rs` implementing the `Engine` trait
2. Add a feature flag in `Cargo.toml`
3. Register in `src/adapters/mod.rs` (feature-gated)
4. Add dispatch in `adapters/outbound/engines/mod.rs` `build_engine()` (the planner uses the `[orchestrator] agent`'s engine; `build_planner_engine()` is unused)
5. Add engine name to config validation in `config.rs` `validate_agent()`
6. If the engine manages its own workspace, set `manages_own_workspace() = true` and use `bridge_tools` from `EngineContext`
7. Build its HTTP client with `egress::policy().llm_api_client` (or pass `claude_cli_env()` to a subprocess) — a bare `reqwest::Client` bypasses `[egress]`

## Related
- [[architecture]] — system overview
- [[configuration]] — config reference
- [[mcp-bridge]] — tool bridging for self-managing engines
