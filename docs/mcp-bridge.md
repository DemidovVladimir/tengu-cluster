# MCP Bridge

The MCP bridge is a stdio subprocess that exposes Tengu-native tools to engines that manage their own workspace (currently [[engine-backends#Claude Code|Claude Code]]). It implements the [Model Context Protocol](https://modelcontextprotocol.io) (JSON-RPC 2.0 over stdin/stdout).

**File:** `src/adapters/mcp_bridge.rs`
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

The bridge builds its own `ToolRegistry` through `channel_runtime::register_core_plugins` — the same helper the in-process executor uses — and wraps it in a `PluginToolExecutor`. Registered (each gated by the `TENGU_BRIDGE_TOOLS` allow-list): workspace, memory (`memory_ingest`, `memory_search`, `persistent_store`), cache, skill_lifecycle, http, crypto, skill_resource, view_skill, manage_skill, and `agentic_memory` under `postgres_memory`. `compress_and_store` is advertised only — the `run-agent` child intercepts it out-of-band. `run_mcp_bridge` is async on the ambient tokio runtime; `tools/call` awaits `executor.execute(&call)` directly.

The bridge does **not** register:
- The `skill` plugin — shell-skill tools need a `SkillRegistry` the bridge cannot construct; they run in the main Tengu process only
- The `mcp` plugin — the external Claude Code client has its own MCP server access; surfacing Tengu's inbound MCP manifest through the outbound bridge would cause name collisions and double-hop routing

## Configuration

The bridge is configured via environment variables set by the Claude Code engine (`ClaudeCodeEngine::build_mcp_config_json`):

| Variable | Description |
|----------|-------------|
| `TENGU_BRIDGE_WORKSPACE` | Workspace directory path |
| `TENGU_BRIDGE_TOOLS` | JSON array of `ToolDef` objects to expose (the allow-list) |
| `TENGU_BRIDGE_MAX_RESULT_CHARS` | Result cap per call (`[limits] max_mcp_result_chars`, default 50 000) |
| `TENGU_BRIDGE_SCOPES` | JSON `HashMap<String, ToolScope>` — the agent's per-tool scopes, **enforced** by the bridge; missing/unparsable → all permissive with a warn |
| `TENGU_EGRESS` | The parent's **resolved** `[egress]` policy (proxy, allow/deny hosts, audit path); wins over any config the bridge would load itself |
| `TENGU_SESSION_ID`, `OPENROUTER_API_KEY`, `TENGU_PERSISTENT_STORE_CHUNK_SIZE`, `TENGU_PERSISTENT_STORE_CHUNK_OVERLAP` | Forwarded from the parent env when set |

The engine treats the MCP `env` block as a replaced environment — anything the bridge needs must be in the list above. Shape of the temp `--mcp-config` file:

```json
{ "mcpServers": { "tengu-tools": {
    "command": "<path to current tengu binary>",
    "args": ["mcp-bridge"],
    "env": { "TENGU_BRIDGE_WORKSPACE": "/path", "TENGU_BRIDGE_TOOLS": "[...]", "TENGU_BRIDGE_SCOPES": "{...}", "TENGU_EGRESS": "{...}" }
} } }
```

A standalone `tengu mcp-bridge` (no `TENGU_EGRESS`) installs `EgressConfig::default()` — `network = "tor"`, i.e. `socks5h://127.0.0.1:9050` (`TENGU_TOR_PROXY` overrides).

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

**Env forwarding matters.** MCP clients typically launch the server with a
*replaced* environment (only the keys in the `env` block), not the inherited
shell env — same trap `claude_code_engine.rs` documents for the bridge. So:

- `TENGU_MEMORY_DATABASE_URL` — required; without it every tool call errors.
- `OPENROUTER_API_KEY` — without it `recall` falls back to FTS-only,
  `ingest_source` stores chunks text-only, and `compile_wiki` writes the
  deterministic fallback page instead of an LLM-synthesised one. All fail-soft,
  but silently degraded.
- `TENGU_WIKI_COMPILER_MODEL` — optional; defaults to `anthropic/claude-sonnet-4-6`.

Both entry points share `serve_mcp_stdio` in `src/adapters/mcp_bridge.rs`. The
server starts even without these — tool calls just error or degrade.

## Testing

Manual test with stdin:
```bash
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | tengu mcp-bridge
```

## Known Limitations

- Bridge constructs a fresh registry per invocation (no shared state with parent Tengu)
- Memory tools (`memory_ingest`, `memory_search`, `persistent_store`) need `OPENROUTER_API_KEY` for embeddings (forwarded by the engine) and use a disk `DiskVectorStore` under `<workspace>/memory`; missing key → skipped with a warn
- `agentic_memory` needs `TENGU_MEMORY_DATABASE_URL` per call; the engine does not put it in the MCP `env` block, so through the bridge the tool errors unless the CLI passes the variable through
- No secret redaction in the bridge: `SanitizedToolExecutor` wraps only the TUI / Telegram executors, and the bridge's `SecretRegistry` starts empty
- The bridge has no agent config: `PluginCtx.config` is `Config::default()`'s `main` agent with `workspace_tools` synthesized as `TENGU_BRIDGE_TOOLS ∩ WORKSPACE_TOOLS_ALLOWLIST`, so opt-ins (`persistent_store`, `shared_cache`, `agentic_memory`, `skill_distill`, `apply_improver_proposal`, `manage_skill`) do flow through; other per-agent fields do not

## Related
- [[engine-backends#Claude Code]] — the engine that spawns the bridge
- [[architecture#Plugin Architecture]] — how the bridge reuses plugin code
- [[architecture#Tool Assembly]] — how tools are advertised to the bridge
- [[skills]] — skill tools are not bridged (they run in the main Tengu process)
