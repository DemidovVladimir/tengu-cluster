# MCP Bridge

The MCP bridge is a stdio subprocess that exposes Tengu-native tools to engines that manage their own workspace (currently [[engine-backends#Claude Code|Claude Code]]). It implements the [Model Context Protocol](https://modelcontextprotocol.io) (JSON-RPC 2.0 over stdin/stdout).

**File:** `src/adapters/mcp_bridge.rs`
**Subcommand:** `tengu mcp-bridge`

## Why a Bridge?

When `manages_own_workspace() = true`, the engine handles file reads, writes, and shell commands natively. But Tengu's platform tools — HTTP requests, crypto signing, shared cache, memory — live inside Tengu's runtime. The MCP bridge makes these tools available to the external engine process.

The bridge runs as a child subprocess of the Claude CLI:

```
Tengu process
  └─ Claude CLI subprocess (via SDK)
       └─ tengu mcp-bridge subprocess (MCP stdio server)
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

## Dispatch

As of Phase A, the bridge builds its own `ToolRegistry` populated from the same plugin list as the main runtime (workspace, memory, cache, http, crypto) and wraps it in a `PluginToolExecutor`. Incoming `tools/call` requests are dispatched through `rt.block_on(executor.execute(&call))` using a single-threaded tokio runtime local to the bridge process.

The bridge does **not** register:
- The `skill` plugin — skill tools run in the main Tengu process, not through the bridge
- The `subagents` plugin — subagent spawning requires full engine/channel wiring not available in the bridge subprocess
- The `mcp` plugin — the external Claude Code client has its own MCP server access; surfacing Tengu's inbound MCP manifest through the outbound bridge would cause name collisions

## Configuration

The bridge is configured via environment variables set by the Claude Code engine:

| Variable | Description |
|----------|-------------|
| `TENGU_BRIDGE_WORKSPACE` | Workspace directory path |
| `TENGU_BRIDGE_TOOLS` | JSON array of `ToolDef` objects to expose |

The Claude Code engine passes these when configuring the MCP server in `ClaudeAgentOptions.mcp_servers`:

```rust
McpServerConfig::Stdio(McpStdioServerConfig {
    command: "tengu",
    args: ["mcp-bridge"],
    env: { TENGU_BRIDGE_WORKSPACE: "/path", TENGU_BRIDGE_TOOLS: "[...]" },
})
```

## Testing

Manual test with stdin:
```bash
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | tengu mcp-bridge
```

## Known Limitations

- Bridge constructs a fresh registry per invocation (no shared state with parent Tengu)
- Memory tools (`remember`, embedding-backed search) require an embedding API key; they work through the bridge when the key is in env
- Secret redaction is applied by the `PluginToolExecutor` path — same code path as the main runtime, so redaction parity is automatic
- Bridge uses `Config::default()` for its `PluginCtx.config`, so per-agent opt-ins like `persistent_store` do not flow through; a future extension can surface these via a richer `TENGU_BRIDGE_*` env-var contract

## Related
- [[engine-backends#Claude Code]] — the engine that spawns the bridge
- [[architecture#Plugin Architecture]] — how the bridge reuses plugin code
- [[architecture#Tool Assembly]] — how tools are advertised to the bridge
- [[skills]] — skill tools are not bridged (they run in the main Tengu process)
