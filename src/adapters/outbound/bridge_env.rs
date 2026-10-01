//! Env contract between the Claude Code engine (writes it into the CLI's
//! `--mcp-config`) and the `tengu mcp-bridge` subprocess it spawns (reads it,
//! `adapters/inbound/mcp_bridge.rs`). The CLI merges that `env` block over
//! its own inherited env (`docs/mcp-bridge.md` § Env), so vault secrets,
//! `OPENROUTER_API_KEY` and the vars `[[mcp_servers]]` `$VAR`s name reach the
//! bridge by inheritance — the engine never writes a secret value into the
//! file. Besides the names below: `TENGU_BRIDGE_WORKSPACE`,
//! `TENGU_BRIDGE_TOOLS`, `TENGU_BRIDGE_MAX_RESULT_CHARS`, `TENGU_EGRESS`,
//! `TENGU_SECRETS_LOADED` (names only), `TENGU_SESSION_ID` and `TENGU_CONFIG`
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

/// `1` = a `run-agent` step's bridge (or the doctor's smoke turn): every
/// configured scope also gets the workspace as an fs root, like the step's
/// own executor. Set explicitly by the engine (`StepBridge`), never inherited.
pub(crate) const TENGU_BRIDGE_GRANT_WORKSPACE_ENV: &str = "TENGU_BRIDGE_GRANT_WORKSPACE";

/// A `run-agent` step's summary file: the bridge serves `compress_and_store`
/// by writing the `summary` here; the step reads it back as its IPC summary.
/// Absent = the bridge refuses `compress_and_store` with the reason.
pub(crate) const TENGU_BRIDGE_SUMMARY_FILE_ENV: &str = "TENGU_BRIDGE_SUMMARY_FILE";
