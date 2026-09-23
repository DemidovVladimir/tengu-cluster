//! Env contract between the Claude Code engine (writes it into the CLI's
//! `--mcp-config`) and the `tengu mcp-bridge` subprocess it spawns (reads it,
//! `adapters/inbound/mcp_bridge.rs`).

/// The calling agent's per-tool scope map (JSON `HashMap<String, ToolScope>`).
pub(crate) const TENGU_BRIDGE_SCOPES_ENV: &str = "TENGU_BRIDGE_SCOPES";

/// `[[mcp_servers]]` entries (JSON array of `McpServerConfig`) whose tools
/// appear in `TENGU_BRIDGE_TOOLS` as `{server}__{tool}`. Absent for a
/// standalone bridge.
pub(crate) const TENGU_BRIDGE_MCP_SERVERS_ENV: &str = "TENGU_BRIDGE_MCP_SERVERS";
