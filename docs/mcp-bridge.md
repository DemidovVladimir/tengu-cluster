# MCP Bridge

The MCP bridge is a stdio subprocess that exposes Tengu-native tools to engines that manage their own workspace (currently [[engine-backends#Claude Code|Claude Code]]). It implements the [Model Context Protocol](https://modelcontextprotocol.io) (JSON-RPC 2.0 over stdin/stdout).

**File:** `src/adapters/mcp_bridge.rs`
**Subcommand:** `tengu mcp-bridge`

## Why a Bridge?

When `manages_own_workspace() = true`, the engine handles file reads, writes, and shell commands natively. But Tengu's platform tools — HTTP requests, crypto signing, shared cache — live inside Tengu's runtime. The MCP bridge makes these tools available to the external engine process.

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

The bridge creates its own tool executors from environment variables — it does not share memory with the parent Tengu process:

| Executor | Tools |
|----------|-------|
| `WorkspaceToolExecutionAdapter` | read_file, write_file, list_directory, run_command |
| `HttpToolExecutionAdapter` | http_request |
| `CryptoToolExecutionAdapter` | sign_and_send_transaction, sign_message, get_wallet_address, abi_encode, hex_to_uint256 |
| `CacheToolExecutionAdapter` | shared_cache |

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

- Bridge re-creates executors per invocation (no shared state with parent)
- Memory tools (`remember`, `memory_search`) not bridged (require embedding API)
- `memory_write` and `memory_get` could be bridged (file-based) but are not in v1
- Secret redaction not yet applied to tool results in the bridge path

## Related
- [[engine-backends#Claude Code]] — the engine that spawns the bridge
- [[architecture#Tool Assembly]] — how tools are assembled
- [[skills]] — skill tools exposed through the bridge
