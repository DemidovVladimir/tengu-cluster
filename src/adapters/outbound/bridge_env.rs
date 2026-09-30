//! Env contract between the Claude Code engine (writes it into the CLI's
//! `--mcp-config`) and the `tengu mcp-bridge` subprocess it spawns (reads it,
//! `adapters/inbound/mcp_bridge.rs`). The CLI merges that `env` block over
//! its own inherited env (`docs/mcp-bridge.md` § Env), so vault secrets and
//! `TENGU_SECRETS_LOADED` also reach the bridge. Besides the names below:
//! `TENGU_BRIDGE_WORKSPACE`, `TENGU_BRIDGE_TOOLS`,
//! `TENGU_BRIDGE_MAX_RESULT_CHARS`, `TENGU_EGRESS` and `TENGU_CONFIG`
//! (`config::paths::TENGU_CONFIG_ENV`, absolute: the bridge loads it).

/// The calling agent's per-tool scope map (JSON `HashMap<String, ToolScope>`).
/// Used only when the bridge cannot resolve the agent from `TENGU_CONFIG`.
pub(crate) const TENGU_BRIDGE_SCOPES_ENV: &str = "TENGU_BRIDGE_SCOPES";

/// `[[mcp_servers]]` entries (JSON array of `McpServerConfig`) whose tools
/// appear in `TENGU_BRIDGE_TOOLS` as `{server}__{tool}`. Absent for a
/// standalone bridge.
pub(crate) const TENGU_BRIDGE_MCP_SERVERS_ENV: &str = "TENGU_BRIDGE_MCP_SERVERS";

/// The calling agent's name: the bridge builds its tools from
/// `[agents.<name>]` of the config in `TENGU_CONFIG` (scopes, sandbox
/// sections, `no_shell_fallback`, `workspace_tools`). Absent = standalone.
pub(crate) const TENGU_BRIDGE_AGENT_ENV: &str = "TENGU_BRIDGE_AGENT";
