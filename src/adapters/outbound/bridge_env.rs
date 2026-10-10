//! Env contract between the CLI engines — Claude Code (writes it into the
//! CLI's `--mcp-config`) and Codex (`-c mcp_servers.tengu-tools.env`) — and
//! the `tengu mcp-bridge` subprocess the CLI spawns (reads it,
//! `adapters/inbound/mcp_bridge.rs`); [`bridge_env`] builds it for both.
//! Claude Code merges that `env` block over its own inherited env
//! (`docs/mcp-bridge.md` § Env); Codex forwards the inherited names
//! (`env_vars`, `engines/codex.rs`) — so vault secrets,
//! `OPENROUTER_API_KEY` and the vars `[[mcp_servers]]` `$VAR`s name reach the
//! bridge by inheritance — the engine never writes a secret value (nor a
//! `[[mcp_servers]]` config) into the file. Besides the names below: `TENGU_BRIDGE_WORKSPACE`,
//! `TENGU_BRIDGE_TOOLS`, `TENGU_BRIDGE_MAX_RESULT_CHARS`, `TENGU_EGRESS`,
//! `TENGU_SECRETS_LOADED` (names only), `TENGU_SESSION_ID` and `TENGU_CONFIG`
//! (`config::paths::TENGU_CONFIG_ENV`, absolute: the bridge loads it).

// The builder is used by the CLI engines only (features `claude_code`,
// `codex`); the bridge reads the names in every build.
#![cfg_attr(not(any(feature = "claude_code", feature = "codex")), allow(dead_code))]

/// The calling agent's per-tool scope map (JSON `HashMap<String, ToolScope>`).
/// Used only when the bridge cannot resolve the agent from `TENGU_CONFIG`.
pub(crate) const TENGU_BRIDGE_SCOPES_ENV: &str = "TENGU_BRIDGE_SCOPES";

/// The `[[mcp_servers]]` whose tools appear in `TENGU_BRIDGE_TOOLS` as
/// `{server}__{tool}`: a JSON array. The Claude Code engine writes their
/// names; the bridge takes each named server from the config it loads
/// (`TENGU_CONFIG`) — so no config value, a `${VAR}`-expanded secret
/// included, sits in the temp `--mcp-config`. A full `McpServerConfig`
/// object is accepted too (a standalone bridge, tests). Absent = none.
pub(crate) const TENGU_BRIDGE_MCP_SERVERS_ENV: &str = "TENGU_BRIDGE_MCP_SERVERS";

/// The run's conversation for bridged tools (`ToolCtx.conversation`): a
/// JSON array of `Message` the Claude Code engine keeps current while the
/// CLI runs (`engines/claude_code.rs::Transcript`, mode 0600, removed with
/// the run); the bridge reads it per call. Absent = no conversation (a tool
/// that needs one, `skill_distill`, refuses).
pub(crate) const TENGU_BRIDGE_TRANSCRIPT_FILE_ENV: &str = "TENGU_BRIDGE_TRANSCRIPT_FILE";

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

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::config::McpServerConfig;
use crate::domain::message::ToolDef;
use crate::domain::scope::ToolScope;

/// A `run-agent` step's bridge options (the Claude Code and Codex engines
/// write them into the bridge env, [`bridge_env`]). Explicit, never
/// inherited from the process env.
#[derive(Debug, Clone, Default)]
pub(crate) struct StepBridge {
    /// [`TENGU_BRIDGE_GRANT_WORKSPACE_ENV`]`=1`: every configured scope also
    /// gets the workspace as an fs root — what the step's own executor does
    /// (`bootstrap::tools::grant_workspace_root`).
    pub grant_workspace: bool,
    /// [`TENGU_BRIDGE_SUMMARY_FILE_ENV`]: the bridge serves
    /// `compress_and_store` by writing the summary here; the step reads it
    /// back as its IPC summary. `None` = the bridge refuses the call with
    /// the reason.
    pub summary_file: Option<PathBuf>,
}

/// What one CLI run hands its `tengu mcp-bridge`.
pub(crate) struct BridgeRun<'a> {
    pub workspace: &'a Path,
    /// The allow-list the bridge serves (`TENGU_BRIDGE_TOOLS`).
    pub tools: &'a [ToolDef],
    pub max_result_chars: u32,
    /// The calling agent's scopes ([`TENGU_BRIDGE_SCOPES_ENV`], the fallback).
    pub scopes: &'a HashMap<String, ToolScope>,
    /// `[agents.<id>]` + its absolute config file ([`TENGU_BRIDGE_AGENT_ENV`]
    /// + `TENGU_CONFIG`); `None` = the bridge's standalone fallback.
    pub agent: Option<(&'a str, &'a Path)>,
    pub step: &'a StepBridge,
    /// The run's transcript file ([`TENGU_BRIDGE_TRANSCRIPT_FILE_ENV`]).
    pub transcript: Option<&'a Path>,
    /// The config's `[[mcp_servers]]`; only those with a `{server}__{tool}`
    /// entry in `tools` are named ([`TENGU_BRIDGE_MCP_SERVERS_ENV`]).
    pub mcp_servers: &'a [McpServerConfig],
}

/// The env block of the bridge (the module contract): no secret value —
/// the bridge inherits the CLI's env for those (Claude Code merges this
/// block over it; Codex forwards the inherited names, `engines/codex.rs`).
pub(crate) fn bridge_env(run: &BridgeRun<'_>) -> Map<String, Value> {
    let s = |v: String| Value::String(v);
    let mut env = Map::new();
    env.insert(
        "TENGU_BRIDGE_WORKSPACE".into(),
        s(run.workspace.to_string_lossy().into_owned()),
    );
    env.insert(
        "TENGU_BRIDGE_TOOLS".into(),
        s(serde_json::to_string(run.tools).unwrap_or_else(|_| "[]".into())),
    );
    env.insert(
        "TENGU_BRIDGE_MAX_RESULT_CHARS".into(),
        s(run.max_result_chars.to_string()),
    );
    // Per-tool scopes cross the process boundary as JSON — the bridge's
    // fallback when it cannot resolve the agent from the config.
    env.insert(
        TENGU_BRIDGE_SCOPES_ENV.into(),
        s(serde_json::to_string(run.scopes).unwrap_or_else(|_| "{}".into())),
    );
    // Agent + config file: the bridge builds its tools from that
    // `[agents.<id>]` block (sandbox sections, scopes, `no_shell`).
    if let Some((agent, config)) = run.agent {
        env.insert(TENGU_BRIDGE_AGENT_ENV.into(), s(agent.to_string()));
        env.insert(
            crate::config::paths::TENGU_CONFIG_ENV.into(),
            s(config.to_string_lossy().into_owned()),
        );
    }
    if run.step.grant_workspace {
        env.insert(TENGU_BRIDGE_GRANT_WORKSPACE_ENV.into(), s("1".into()));
    }
    if let Some(file) = &run.step.summary_file {
        env.insert(
            TENGU_BRIDGE_SUMMARY_FILE_ENV.into(),
            s(file.to_string_lossy().into_owned()),
        );
    }
    if let Some(file) = run.transcript {
        env.insert(
            TENGU_BRIDGE_TRANSCRIPT_FILE_ENV.into(),
            s(file.to_string_lossy().into_owned()),
        );
    }
    // External `[[mcp_servers]]` with a `{server}__{tool}` entry in `tools`:
    // their NAMES only — the bridge takes each server from the config it
    // loads (`TENGU_CONFIG`, `${VAR}` expanded there from its inherited
    // env), so no `${VAR}`-expanded value reaches the CLI's MCP config.
    use crate::adapters::outbound::mcp_client::is_server_tool;
    let servers: Vec<&str> = run
        .mcp_servers
        .iter()
        .filter(|srv| run.tools.iter().any(|t| is_server_tool(&srv.name, &t.name)))
        .map(|srv| srv.name.as_str())
        .collect();
    if !servers.is_empty() {
        env.insert(
            TENGU_BRIDGE_MCP_SERVERS_ENV.into(),
            s(serde_json::to_string(&servers).unwrap_or_else(|_| "[]".into())),
        );
    }
    // The bridge runs the tools — it must apply the parent's egress policy.
    env.insert(
        crate::adapters::outbound::egress::EGRESS_ENV.into(),
        s(crate::adapters::outbound::egress::policy().child_env()),
    );
    // Process settings the bridge's tools read: persistent store chunking;
    // the session id (`agentic_memory` `capture` stamps it when the LLM
    // omits it); the vault var names (names only: the bridge registers their
    // values for redaction, and a `tengu` a bridge tool runs does not
    // re-prompt for the vault password).
    for name in [
        "TENGU_PERSISTENT_STORE_CHUNK_SIZE",
        "TENGU_PERSISTENT_STORE_CHUNK_OVERLAP",
        "TENGU_SESSION_ID",
        crate::adapters::outbound::secrets::SECRETS_LOADED_ENV,
    ] {
        if let Ok(v) = std::env::var(name) {
            env.insert(name.into(), s(v));
        }
    }
    env
}
