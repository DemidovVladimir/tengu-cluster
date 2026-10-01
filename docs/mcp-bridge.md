# MCP Bridge

The MCP bridge is a stdio subprocess that exposes Tengu-native tools to engines that manage their own workspace (currently [[engine-backends#Claude Code|Claude Code]]). It implements the [Model Context Protocol](https://modelcontextprotocol.io) (JSON-RPC 2.0 over stdin/stdout).

**File:** `src/adapters/inbound/mcp_bridge.rs`
**Subcommand:** `tengu mcp-bridge`

## Why a Bridge?

When `manages_own_workspace() = true`, the engine handles file reads, writes, and shell commands natively. But Tengu's platform tools — HTTP requests, crypto signing, shared cache, memory — live inside Tengu's runtime. The MCP bridge makes these tools available to the external engine process.

The bridge runs as a child subprocess of the Claude CLI:

```
Tengu process (parent, or a `tengu run-agent` child for subagents)
  └─ claude -p subprocess (--mcp-config <temp file>)
       └─ tengu mcp-bridge subprocess (MCP stdio server, serverInfo.name = tengu-tools)
```

## Protocol

Standard MCP over stdio (newline-delimited JSON-RPC 2.0):

| Method | Description |
|--------|-------------|
| `initialize` | Returns server info and capabilities |
| `notifications/initialized` | Acknowledged, no response |
| `tools/list` | Returns Tengu tool definitions in MCP format |
| `tools/call` | Executes a tool and returns the result |
| `ping` | Health check |

## Tool Mapping

Tengu `ToolDef` is converted to MCP `Tool` format:

```
ToolDef { name, description, parameters }
   ↓
McpToolDef { name, description, inputSchema: parameters }
```

Claude sees each tool as `mcp__tengu-tools__<name>` (e.g. `mcp__tengu-tools__persistent_store`); the engine allow-lists exactly those names with `--allowedTools`.

## Dispatch

The bridge builds its own `ToolRegistry` through `adapters::outbound::tools::register_catalog` — the same helper the in-process executor uses — and wraps it in a `PluginToolExecutor` inside a `SanitizedToolExecutor`. Registered (each gated by the `TENGU_BRIDGE_TOOLS` allow-list): the whole catalog (workspace, memory, cache, skill lifecycle, http, crypto, skill tools, Solana, Hyperliquid, xm, `agentic_memory` under `postgres_memory`), the agent's shell skills, and the `[[mcp_servers]]` the engine passed. `run_mcp_bridge` is async on the ambient tokio runtime; `tools/call` awaits the executor directly.

| Tool kind | In the bridge |
|---|---|
| Catalog rows | `register_catalog`, as in-process |
| Shell skills | `bootstrap::tools::agent_skill_registry` over `TENGU_BRIDGE_WORKSPACE`, as `run-agent` loads them: `skill_packages` of the agent **plus every requested name** (a composed plan step's `skills` reach the bridge only through its parent's tool list); none when the agent runs no shell (`no_shell_fallback`: a `[risk]` / signer sandbox) |
| `[[mcp_servers]]` | only the servers the Claude Code engine passes in `TENGU_BRIDGE_MCP_SERVERS` (their `{server}__{tool}` names in the allow-list); a standalone bridge proxies none. The CLI runs with `--strict-mcp-config`, so the bridge is its only MCP server |
| `compress_and_store` | `StepSummary` (no plugin): with `TENGU_BRIDGE_SUMMARY_FILE` (a `run-agent` step) the `summary` is written there → `stored`, and the step uses it as its IPC summary (+ the `agentic_memory` write); without it the call is refused naming why ("give your summary as plain text") |

## Configuration

Environment set by the Claude Code engine (`ClaudeCodeEngine::build_mcp_config_json`; names in `adapters/outbound/bridge_env.rs`):

| Variable | Description |
|----------|-------------|
| `TENGU_CONFIG` | Absolute path of the config file in effect — the sandbox file (`load_sandbox_or` pins it) or the base config. The bridge loads it once with `Config::load` (validated + folded) |
| `TENGU_BRIDGE_AGENT` | The calling `[agents.<name>]` (the base block for a composed plan step); the bridge runs tools as that agent |
| `TENGU_BRIDGE_WORKSPACE` | Workspace directory path |
| `TENGU_BRIDGE_TOOLS` | JSON array of `ToolDef` objects to expose (the allow-list) |
| `TENGU_BRIDGE_MAX_RESULT_CHARS` | Result cap per call (`[limits] max_mcp_result_chars`, default 50 000) |
| `TENGU_BRIDGE_SCOPES` | JSON `HashMap<String, ToolScope>` — the agent's scope map; used only by the fallback below (missing/unparsable → all permissive with a warn) |
| `TENGU_BRIDGE_MCP_SERVERS` | The `[[mcp_servers]]` behind requested `{server}__{tool}` names — their `$VAR` references as written, resolved by the bridge from its inherited env |
| `TENGU_BRIDGE_GRANT_WORKSPACE` | `1` = a `run-agent` step's bridge (and the doctor's smoke turn): every configured scope also gets the workspace as an fs root, like the step's executor — but a deny-all scope (every field empty) stays a deny. Set by the engine (`engines::build_step_engine` → `StepBridge`), never read from an inherited `TENGU_AGENT_IPC` |
| `TENGU_BRIDGE_SUMMARY_FILE` | A `run-agent` step's summary file: `compress_and_store` writes there (§ Dispatch) |
| `TENGU_EGRESS` | The parent's **resolved** `[egress]` policy (proxy, allow/deny hosts, audit path); wins over the loaded config's `[egress]` |
| `TENGU_SECRETS_LOADED` | Names of the vault vars the parent loaded (names only); the bridge registers their inherited values for redaction |
| `TENGU_SESSION_ID`, `TENGU_PERSISTENT_STORE_CHUNK_SIZE`, `TENGU_PERSISTENT_STORE_CHUNK_OVERLAP` | Forwarded from the parent env when set. No secret value is ever written (`OPENROUTER_API_KEY`, vault values, `$VAR` values arrive by inheritance; unit test: the file's keys ⊆ this table) |

Shape of the temp `--mcp-config` file:

```json
{ "mcpServers": { "tengu-tools": {
    "command": "<path to current tengu binary>",
    "args": ["mcp-bridge"],
    "env": { "TENGU_CONFIG": "/abs/sandboxes/<name>/config.toml", "TENGU_BRIDGE_AGENT": "<agent>", "TENGU_BRIDGE_WORKSPACE": "/path", "TENGU_BRIDGE_TOOLS": "[...]", "TENGU_BRIDGE_SCOPES": "{...}", "TENGU_EGRESS": "{...}" }
} } }
```

### Env: merged, not replaced (verified 2026-09-30)

| Check | Result |
|---|---|
| Probe | `claude -p "reply ok" --strict-mcp-config --mcp-config <tmp.json>` (CLI 2.1.285, subscription); the stdio server was a throwaway `sh -c 'env > $TMPDIR/…'` with one var in its `env` block and one set only in the CLI's env |
| Seen by the server | both vars, plus the CLI's whole env (71 vars: `PATH`, `HOME`, `TMPDIR`, …) |
| Meaning | the `env` block is **merged over** the inherited env. Vault secrets (loaded into the parent's env), `OPENROUTER_API_KEY`, the vars `[[mcp_servers]]` `$VAR`s name, `TENGU_HOME`, `TENGU_MEMORY_DATABASE_URL` reach the bridge by inheritance; the temp file holds no secret value (since 2026-10-01 also no `OPENROUTER_API_KEY` / `$VAR` values) |
| Parent session env | the engine removes a parent Claude Code session's env before spawning the CLI (`claude_code.rs::parent_session_env`): `CLAUDECODE`, `CLAUDE_PID`, `CLAUDE_EFFORT`, every `CLAUDE_CODE_*` but auth / provider (`CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CODE_USE_*`, `CLAUDE_CODE_SKIP_*_AUTH`, client cert / key) — a nested `claude` starts like one from the operator's terminal |
| If a CLI release switches to replace | the explicit keys above still arrive; other secret values would not (not redacted, but not visible to the bridge's tools either) — the live check is the engine matrix's `claude_code_workspace` leg (2026-09-30, CLI 2.1.286: a value named in `TENGU_SECRETS_LOADED` came back `[REDACTED]` through the bridge) |

### Agent + config resolution

| `TENGU_CONFIG` file | `[agents.<TENGU_BRIDGE_AGENT>]` | Tools run as |
|---|---|---|
| present | present | that block as `Config::load` folded it: scopes (with `[default_scopes]`), `sandbox` sections (`xm_state_dir`, …), `no_shell_fallback`, signer; `[memory]` as in-process. With `TENGU_BRIDGE_GRANT_WORKSPACE=1` (a `run-agent` step's engine) every configured scope but a deny-all one also gets the workspace as an fs root — the step's own executor does the same (`grant_workspace_root`) |
| present | absent / unset | `Config::default()`'s `main` + `TENGU_BRIDGE_SCOPES`, warn; shell-free when the loaded config's agents are |
| absent | — | standalone bridge: default `main` + `TENGU_BRIDGE_SCOPES`, warn; disk memory under `<workspace>/memory` |
| present but invalid | — | the bridge exits with the load error |

Then every `WORKSPACE_TOOLS` name in `TENGU_BRIDGE_TOOLS` joins the agent's `workspace_tools` (the merge `subagent_config` applies to `tools`), and every requested name counts as listed in `skill_packages` (shell skills).

### Parity with in-process tools

| Topic | Bridge |
|---|---|
| Scopes | `resolve_tool_scopes(workspace, agent.scopes, …, agent.no_shell_fallback)` — the in-process call |
| `no_shell` | from the agent (`no_shell_fallback`: a signer or `[risk]` sandbox); the permissive fallback then has no `shell_bins` and no shell skill loads |
| Tools list | `TENGU_BRIDGE_TOOLS` = what the parent advertised, which follows the agent's `tools` on every surface (`bootstrap::tools::agent_base_tools`; `[[mcp_servers]]` tools too) — a tool outside it runs nowhere |
| Secrets + redaction | `process_secret_registry(None)`: values named in `TENGU_SECRETS_LOADED` + `TENGU_MASTER_PASSWORD`, never prompts (same function as the CLI and `run-agent`); `SanitizedToolExecutor` redacts text, typed observations and errors — the same wrapper in-process (chat, `run-agent`, webhooks), so an error text reaches no model unredacted |
| Call id | `ToolCall.id` = `mcp:<process nonce>:<JSON-RPC tools/call id>` (the id a string verbatim, a number in decimal; none → no id) → `ToolCtx.call_id`. The nonce (a uuid, 32 hex digits, `mcp_bridge::call_nonce`) is minted once per bridge process: JSON-RPC ids restart with every Claude CLI session, and every turn or plan step starts a new CLI + bridge — without it an exec tool's idempotency key (`client_order_id` = call id) would replay an earlier session's order. `tengu tool call` maps `--call-id` / batch `call_id` the same way (its own nonce) |
| Egress | the loaded config's `[egress]`, overridden by `TENGU_EGRESS` |

A standalone `tengu mcp-bridge` (no `TENGU_EGRESS`, no config file) installs `EgressConfig::default()` — `network = "tor"`, i.e. `socks5h://127.0.0.1:9050` (`TENGU_TOR_PROXY` overrides).

## Standalone `agentic-memory` MCP server

`tengu agentic-memory-server` runs the same stdio JSON-RPC loop but exposes
**only** the `agentic_memory` tool — so non-Tengu agents (ChatGPT, Codex,
Claude) can read+write the same Open Brain memory without a full Tengu sandbox.
Build with `--features postgres_memory`.

| | `tengu mcp-bridge` | `tengu agentic-memory-server` |
|---|---|---|
| Tool set | `TENGU_BRIDGE_TOOLS` (caller-supplied) | fixed: `agentic_memory` only |
| Spawned by | the Claude Code engine | any MCP client config |
| `serverInfo.name` | `tengu-tools` | `tengu-agentic-memory` |
| Agent | `[agents.<TENGU_BRIDGE_AGENT>]` of `TENGU_CONFIG` | default `main` (no config load) |
| Needs | — | `TENGU_MEMORY_DATABASE_URL` (per call) |

Workspace (where `.tengu/agentic-memory/{raw,wiki}/` live) defaults to the cwd,
overridable via `TENGU_BRIDGE_WORKSPACE`. Wire it into an external MCP client:

```jsonc
{
  "mcpServers": {
    "tengu-agentic-memory": {
      "command": "tengu",
      "args": ["agentic-memory-server"],
      "env": {
        "TENGU_MEMORY_DATABASE_URL": "postgres://tengu:tengu@localhost:5432/tengu_memory",
        "OPENROUTER_API_KEY": "sk-or-...",
        "TENGU_WIKI_COMPILER_MODEL": "anthropic/claude-sonnet-4-6"
      }
    }
  }
}
```

**Env forwarding matters.** Some MCP clients launch the server with a
*replaced* environment (only the keys in the `env` block), not the inherited
shell env (the Claude CLI merges — § Env above). Put these in the `env` block:

- `TENGU_MEMORY_DATABASE_URL` — required; without it every tool call errors.
- `OPENROUTER_API_KEY` — without it `recall` falls back to FTS-only,
  `ingest_source` stores chunks text-only, and `compile_wiki` writes the
  deterministic fallback page instead of an LLM-synthesised one. All fail-soft,
  but silently degraded.
- `TENGU_WIKI_COMPILER_MODEL` — optional; defaults to `anthropic/claude-sonnet-4-6`.

Both entry points share `serve_mcp_stdio` in `src/adapters/inbound/mcp_bridge.rs`. The
server starts even without these — tool calls just error or degrade.

## Testing

| Test | Covers |
|---|---|
| `cargo test --bin tengu mcp_bridge` | agent config from a fixture sandbox file (scopes, `xm_state_dir`, workspace grant with `TENGU_BRIDGE_GRANT_WORKSPACE`), fallback to `TENGU_BRIDGE_SCOPES`, `no_shell`, redaction of text and errors, request id → `ToolCtx.call_id` (`mcp:<nonce>:<id>`), shell skills (`skill_packages`, a requested name, none without a shell), `compress_and_store` into the summary file or refused |
| `cargo test --bin tengu --features claude_code engines::claude_code` | the engine writes `TENGU_CONFIG` (absolute) + `TENGU_BRIDGE_AGENT`, the step options, no secret value (keys ⊆ the contract); `tool_use` → `tool_result` pairs become `StreamEvent::ToolRan`; the parent session env it strips; the profile parse |
| `cargo test --test mcp_bridge_external` | a real `tengu mcp-bridge` proxies `[[mcp_servers]]`, runs tools as the configured agent; two bridge processes give the same JSON-RPC id two call ids (`mcp:<nonce>:<id>`) |
| `cargo test --test bridge_conformance` | every catalog tool in-process vs through a real bridge (below); fails for a catalog tool without a case (tracker convention 20); two bridge sessions on one sandbox place two paper orders with the same JSON-RPC id (`two_bridge_sessions_place_two_orders`) |
| `cargo test --features claude_code --test engine_matrix -- --ignored claude_code_` | live: the Claude CLI runs every tool set through `run-agent` + a real bridge (workspace grant, a registered secret back as `[REDACTED]`, the shell skill and the `[[mcp_servers]]` proxy, `compress_and_store` served) and the xm set through `tengu tool turn` on the private `xm_claude` agent (`docs/engine-backends.md` § Engine matrix) |
| `tengu tool call --agent <a> (--tool <t> --args '<json>' [--call-id <id>] \| --batch)` (hidden) | in-process half of the conformance harness: the executor a `run-agent` child or a decision loop builds (`build_subprocess_tool_executor` + `SanitizedToolExecutor`); call ids mapped like the bridge's (`mcp:<process nonce>:<id>`); prints `{text, observation, is_error}` per call |
| `tengu tool turn --agent <a> --goal <text>` (hidden) | one engine turn as `[agents.<a>]` in this process — the `@<agent>` chat path, so a private agent (no `description`, the only kind that may hold exec tools) runs too; its configured scopes as they are (no workspace grant); `claude_code` tools through the real bridge (`TENGU_BRIDGE_AGENT` = that agent); prints `{status, output, tools, metrics}` (the `run-agent` IPC fields). The engine matrix's xm legs (`src/adapters/inbound/cli/tool.rs`) |

Manual test with stdin:
```bash
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | tengu mcp-bridge
```

### Conformance harness (`x-bridge-conformance-test`)

`tests/bridge_conformance.rs` runs each case on two identical fixture sandboxes: in-process through the hidden `tengu tool call --batch` (the executor a `run-agent` child or a decision loop builds: `build_subprocess_tool_executor` + `SanitizedToolExecutor`, `src/adapters/inbound/cli/tool.rs`) and through a real `tengu mcp-bridge` (`TENGU_CONFIG`, `TENGU_BRIDGE_AGENT`, `TENGU_BRIDGE_GRANT_WORKSPACE=1`). 57 cases on 8 threads, < 10 s — besides the catalog: the shell skill `tests/fixtures/skills/matrix_cat` (`.skill(..)`, and refused under `[risk]`), a `[[mcp_servers]]` tool and one outside the agent's `tools`, the Privy egress / scope gate.

| Must match (both sides normalised) | How |
|---|---|
| `is_error` + text per step, and the case's expected outcome | bridge `result.content[0].text` vs `tengu tool call` `text` |
| Files left under the workspace and `TENGU_HOME` | SQLite files dumped as sorted rows per table (`observations` without `observed_at_ms`; history day files, `solana-writes.db`, `cache.db` …), text files as content |
| Upstream requests | a loopback mock logs every request; sorted lists compared |
| Completeness | every `tengu tool list` name has a case; a case for a tool the build lacks fails unless `.gated()` (feature) |

| Fixture | Rule |
|---|---|
| Network | `[egress] network = "open"`, `allow_hosts = ["127.0.0.1"]`: hard-coded upstream hosts are refused (deterministic error), never reached |
| Upstreams | mock routes: method + path + substrings → inline JSON, a `tests/fixtures/…` file, `getMultipleAccounts` built from captured accounts, or an `l2Book` capture stamped now (`.book(..)`: a live book the paper gate accepts); base-URL overrides via `.scoped("SOLANA_RPC_URL")` / `.scoped("HL_API_URL")` (scope with the workspace, `127.0.0.1`, the env var → the mock) |
| Store rows | `.row(..)` seeds `<ws>/.tengu/observations.db` stamped now (the opportunity row a paper entry names) |
| Secrets | `TENGU_SECRETS_LOADED` names a test secret on both sides; it must come back `[REDACTED]` |
| Result size | cases stay under the bridge's cap (`TENGU_BRIDGE_MAX_RESULT_CHARS`, 50 000): only the bridge truncates, the in-process executor does not (engines cap later) |
| Normaliser | temp root → `<ROOT>`, durations, observation ages, `*age_s/ms/secs` (not `max_*` / `min_*`), `next_*_s` countdowns, ISO times, epoch ms / s within 2 days of now, `YYYYMMDD.db`, JSON-RPC ids, the call-id nonce `mcp:<32 hex>:` — table in the test's module doc |
| Debug | `TENGU_CONFORMANCE_VERBOSE=1 cargo test --test bridge_conformance -- --nocapture` prints each case's text, files and requests |

Add a case — one row in `cases()`, e.g. a Hyperliquid read (`info(<type>)` = `POST /info` with that body `type`; `hl_xyz` adds the xyz ctx, at-cap and perp-meta replies):

```rust
hl_xyz(case("hl_ctx", json!({"coins": ["xyz:TSLA"]})))
    .scoped("HL_API_URL")
    .ok("mkt hyperliquid:xyz:TSLA mark=347.19"),
```

Tools whose upstream host is hard-coded (no base-URL override) run their deterministic path: `sol_price` + `lp_snapshot` oracle (`lite-api.jup.ag`), `dlmm_pools` (`dlmm.datapi.meteora.ag`), `jupiter_swap` simulate (Jupiter Ultra), Privy crypto tools (no `PRIVY_*` env; with it, `api.privy.io` is outside `allow_hosts`), memory tools (no embeddings key), `agentic_memory` (no `TENGU_MEMORY_DATABASE_URL`, `--features postgres_memory`).

## Parity rule (operator, 2026-09-30)

Every tool must work under every engine — `openrouter`, `local` and, through this bridge, `claude_code` — and behave the same through the bridge as in-process, no exceptions (CLAUDE.md / AGENTS.md "How to add a new tool" step 4, `docs/tools.md` step 5). E0 closed it (`docs/xmarket-tracker-2026-09-29.md`): `x-bridge-parity` (config, secrets, `no_shell`, call id), `x-claude-code-hardening` (`--strict-mcp-config`, built-ins off in hardened sandboxes), `x-bridge-conformance-test` (every catalog tool in CI), `x-engine-matrix-smoke` (live legs), `x-engine-parity-audit` (2026-10-01: shell skills and `compress_and_store` in the bridge, `[[mcp_servers]]` tools follow `tools` in-process too, errors redacted on every surface, no secret in the temp file, explicit step options, every catalog tool + a shell skill + an `[[mcp_servers]]` proxy live on every engine — gap list in the tracker's W1 notes).

| Still open | Why |
|---|---|
| The live `local` legs | run on the operator's PC (`TENGU_MATRIX_LOCAL_BASE_URL`), never on the dev Mac; offline mock legs cover the local path meanwhile |
| `agentic_memory` live | needs `--features postgres_memory` + Postgres (`TENGU_MEMORY_DATABASE_URL`) |
| Privy signing, Solana `send` | never run (operator rule): the refusal and `simulate` paths are what the matrix runs |

## Known Limitations

- Bridge constructs a fresh registry per invocation (no shared state with parent Tengu)
- Memory tools (`memory_ingest`, `memory_search`, `persistent_store`) need `OPENROUTER_API_KEY` for embeddings; with a config they follow `[memory]` (off → not registered, as in-process), standalone they use a disk `DiskVectorStore` under `<workspace>/memory`
- `agentic_memory` needs `TENGU_MEMORY_DATABASE_URL` per call; the bridge inherits it from the parent env
- `tengu eval` builds engines from the eval config while `TENGU_CONFIG` names the base config: a `claude_code` eval agent's bridge resolves the agent there or falls back (warn)
- The engine passes `--strict-mcp-config` on every run (`x-claude-code-hardening`): the bridge is the Claude CLI's only MCP server — the user's own / plugin MCP servers are not loaded
- Profile `none` (the hardened one) also passes `--setting-sources "" --disable-slash-commands --settings {"autoMemoryEnabled":false,"disableAllHooks":true}`: no settings files, hooks, installed plugins, skills, CLAUDE.md / AGENTS.md or auto-memory, and the `--mcp-config` bridge still loads (CLI 2.1.286, subscription OAuth — [[engine-backends#Claude Code]] § Safety Policy). `--safe-mode` would drop the bridge with the rest

## Related
- [[engine-backends#Claude Code]] — the engine that spawns the bridge
- [[architecture#Plugin Architecture]] — how the bridge reuses plugin code
- [[architecture#Tool Assembly]] — how tools are advertised to the bridge
- [[skills]] — shell-skill tools are bridged (the agent's `skill_packages` + the requested names; none without a shell)
