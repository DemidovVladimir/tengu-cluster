//! Stdio MCP bridge — exposes Tengu tools to Claude Code via the MCP protocol.
//!
//! Runs as `tengu mcp-bridge` subprocess. Claude CLI spawns this as an MCP stdio
//! server and routes tool calls through it. The bridge builds its own
//! `ToolRegistry` via the plugin architecture (same plugins the channel runtime
//! uses) and dispatches each `tools/call` through a `PluginToolExecutor`
//! wrapped in `SanitizedToolExecutor`.
//!
//! Parity with in-process tools (tracker convention 20): the bridge loads the
//! config in `TENGU_CONFIG` once (`Config::load`: validated + folded) and runs
//! tools as `[agents.<TENGU_BRIDGE_AGENT>]` — its scopes, sandbox sections,
//! `no_shell_fallback`, `[memory]` — redacts the process secrets, and hands
//! each tool the JSON-RPC request id as `ToolCtx.call_id`. Without that file
//! or agent (a standalone bridge in someone's own Claude Code) it falls back
//! to `Config::default()`'s `main` agent + `TENGU_BRIDGE_SCOPES`, with a warn.
//! Env contract: `adapters/outbound/bridge_env.rs`; doc: `docs/mcp-bridge.md`.
//!
//! Protocol: JSON-RPC 2.0 over stdin/stdout (newline-delimited).

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{info, warn};

use crate::adapters::outbound::bridge_env::{
    TENGU_BRIDGE_AGENT_ENV, TENGU_BRIDGE_MCP_SERVERS_ENV, TENGU_BRIDGE_SCOPES_ENV,
};
use crate::adapters::outbound::memory::disk_vector::DiskVectorStore;
use crate::adapters::outbound::memory::embedder::Embedder;
use crate::adapters::outbound::secrets::{process_secret_registry, SanitizedToolExecutor};
use crate::application::memory::manager::MemoryManager;
use crate::config::{AgentConfig, Config, McpServerConfig};
use crate::domain::memory::DEFAULT_EMBEDDING_MODEL;
use crate::ports::engine::ToolExecutor;
use crate::ports::memory::VectorStore;
// The plugin set comes from `outbound::tools::register_catalog`; this file
// stays focused on stdio JSON-RPC + executor wiring.
use crate::adapters::inbound::activity::build_tool_activity_text;
use crate::adapters::outbound::shell::LocalShellExecutor;
use crate::application::tools::registry::{PluginToolExecutor, ToolRegistry};
use crate::domain::message::{ToolCall, ToolDef};
use crate::domain::scope::ToolScope;
use crate::domain::secrets::SecretRegistry;
use crate::ports::tool::PluginCtx;
use crate::ports::tool_activity::ToolActivityPort;

// ---------------------------------------------------------------------------
// JSON-RPC types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<serde_json::Value>,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

impl JsonRpcResponse {
    fn success(id: serde_json::Value, result: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    fn error(id: serde_json::Value, code: i32, message: String) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(JsonRpcError { code, message }),
        }
    }
}

// ---------------------------------------------------------------------------
// MCP tool schema (what we advertise to Claude)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct McpToolDef {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

impl From<&ToolDef> for McpToolDef {
    fn from(td: &ToolDef) -> Self {
        Self {
            name: td.name.clone(),
            description: td.description.clone(),
            input_schema: td.parameters.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge activity port — no-op (logs go through the tracing subscriber).
// ---------------------------------------------------------------------------

struct BridgeActivity;

impl ToolActivityPort for BridgeActivity {
    fn publish_tool_activity(&self, _call: &ToolCall) {}
}

// ---------------------------------------------------------------------------
// What the bridge runs with
// ---------------------------------------------------------------------------

/// The bridge's inputs: its env (`bridge_env.rs` contract) and the config
/// file it loaded. `from_env` in production; tests build it directly.
struct BridgeSetup {
    /// `TENGU_BRIDGE_WORKSPACE`, else the cwd.
    workspace: PathBuf,
    /// `TENGU_BRIDGE_TOOLS`: the allow-list, and what `tools/list` returns.
    tools: Vec<ToolDef>,
    /// `TENGU_CONFIG` (else `<TENGU_HOME>/config.toml`), loaded; `None` = no file.
    config: Option<Config>,
    /// `TENGU_BRIDGE_AGENT`.
    agent: Option<String>,
    /// `TENGU_AGENT_IPC=1`, inherited from a `run-agent` child: like that
    /// child's executor, every configured scope also gets the workspace as
    /// an fs root (`bootstrap::tools::grant_workspace_root`).
    subagent: bool,
    /// `TENGU_BRIDGE_SCOPES` — used only by the default-`main` fallback.
    env_scopes: HashMap<String, ToolScope>,
    /// `TENGU_BRIDGE_MCP_SERVERS`.
    mcp_servers: Vec<McpServerConfig>,
}

impl BridgeSetup {
    fn from_env(tools: Vec<ToolDef>, config: Option<Config>) -> Self {
        Self {
            workspace: std::env::var("TENGU_BRIDGE_WORKSPACE")
                .map(PathBuf::from)
                .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default()),
            tools,
            config,
            agent: std::env::var(TENGU_BRIDGE_AGENT_ENV)
                .ok()
                .filter(|a| !a.is_empty()),
            subagent: std::env::var("TENGU_AGENT_IPC").is_ok_and(|v| v == "1"),
            env_scopes: bridge_scopes_from_env(),
            mcp_servers: bridge_mcp_servers_from_env(),
        }
    }
}

// ---------------------------------------------------------------------------
// Bridge entry point
// ---------------------------------------------------------------------------

/// Run the MCP bridge stdio server. Uses the ambient tokio runtime (the bridge
/// subprocess is launched from within Tengu's `#[tokio::main]`, so creating a
/// second runtime here would panic). Returns when stdin closes.
pub async fn run_mcp_bridge() -> Result<()> {
    // One load serves the agent block and `[egress]`. The parent's policy
    // arrives as TENGU_EGRESS (wins inside `install`); a standalone bridge
    // follows the loaded config, else the built-in default (`network = "tor"`).
    let config = load_bridge_config()?;
    let egress = config
        .as_ref()
        .map(|c| c.egress.clone())
        .unwrap_or_default();
    crate::adapters::outbound::egress::install(&egress)?;

    let tools: Vec<ToolDef> = match std::env::var("TENGU_BRIDGE_TOOLS") {
        Ok(json) => serde_json::from_str(&json)
            .map_err(|e| anyhow::anyhow!("Failed to parse TENGU_BRIDGE_TOOLS: {}", e))?,
        Err(_) => vec![],
    };

    serve_mcp_stdio(
        BridgeSetup::from_env(tools, config),
        max_result_chars_from_env(),
        "tengu-tools",
    )
    .await
}

/// The config file in effect — `TENGU_CONFIG` (absolute, from the Claude Code
/// engine) or `<TENGU_HOME>/config.toml` — validated + folded exactly like
/// in-process. Absent = `None`; present but invalid = error.
fn load_bridge_config() -> Result<Option<Config>> {
    let path = crate::config::paths::default_config_path();
    if !path.is_file() {
        return Ok(None);
    }
    Config::load(&path)
        .map(Some)
        .with_context(|| format!("mcp-bridge: load config {}", path.display()))
}

fn max_result_chars_from_env() -> usize {
    std::env::var("TENGU_BRIDGE_MAX_RESULT_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_MCP_RESULT_CHARS)
}

/// Standalone MCP stdio server exposing ONLY the `agentic_memory` tool, so
/// non-Tengu agents (ChatGPT / Codex / Claude) can read+write the same Open
/// Brain memory. Entry point for `tengu agentic-memory-server`.
///
/// Needs `TENGU_MEMORY_DATABASE_URL` for any tool call to succeed; the server
/// itself starts fine without it (calls just return an error). The workspace —
/// where `.tengu/agentic-memory/{raw,wiki}/` live — defaults to the cwd and is
/// overridable via `TENGU_BRIDGE_WORKSPACE`.
#[cfg(feature = "postgres_memory")]
pub async fn run_agentic_memory_mcp_server() -> Result<()> {
    let tools = crate::adapters::outbound::tools::agentic_memory::tool_defs();
    serve_mcp_stdio(
        BridgeSetup::from_env(tools, None),
        max_result_chars_from_env(),
        "tengu-agentic-memory",
    )
    .await
}

/// Shared stdio JSON-RPC serve loop. Builds the sanitized executor over
/// `setup.tools` and dispatches `initialize` / `tools/list` / `tools/call` /
/// `ping` until stdin closes. Used by both `run_mcp_bridge` and
/// `run_agentic_memory_mcp_server`.
async fn serve_mcp_stdio(
    setup: BridgeSetup,
    max_result_chars: usize,
    server_name: &'static str,
) -> Result<()> {
    // Inherited vault values + master password; the bridge never prompts.
    let secrets = Arc::new(process_secret_registry(None));
    let executor = sanitized(build_bridge_executor(&setup, &secrets).await?, &secrets);
    let mcp_tools: Vec<McpToolDef> = setup.tools.iter().map(McpToolDef::from).collect();

    info!(
        workspace = %setup.workspace.display(),
        tool_count = setup.tools.len(),
        server = server_name,
        "MCP stdio server started"
    );

    let mut reader = BufReader::new(tokio::io::stdin()).lines();
    let stdout = io::stdout();

    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "MCP server received invalid JSON");
                let resp = JsonRpcResponse::error(
                    serde_json::Value::Null,
                    -32700,
                    format!("Parse error: {}", e),
                );
                write_response(&stdout, &resp);
                continue;
            }
        };

        if request.jsonrpc != "2.0" {
            let resp = JsonRpcResponse::error(
                request.id.unwrap_or(serde_json::Value::Null),
                -32600,
                "Invalid JSON-RPC version".into(),
            );
            write_response(&stdout, &resp);
            continue;
        }

        let id = request.id.unwrap_or(serde_json::Value::Null);

        let resp = match request.method.as_str() {
            "initialize" => handle_initialize(id, server_name),
            "notifications/initialized" => continue, // notification, no response
            "tools/list" => handle_tools_list(id, &mcp_tools),
            "tools/call" => {
                handle_tools_call(
                    id,
                    &request.params,
                    executor.as_ref(),
                    max_result_chars,
                    &secrets,
                )
                .await
            }
            "ping" => JsonRpcResponse::success(id, serde_json::json!({})),
            _ => {
                JsonRpcResponse::error(id, -32601, format!("Method not found: {}", request.method))
            }
        };

        write_response(&stdout, &resp);
    }

    Ok(())
}

/// Text and typed observations come back redacted, as in-process.
fn sanitized(executor: PluginToolExecutor, secrets: &Arc<SecretRegistry>) -> Arc<dyn ToolExecutor> {
    Arc::new(SanitizedToolExecutor::new(
        Arc::new(executor),
        Arc::clone(secrets),
    ))
}

fn write_response(stdout: &io::Stdout, resp: &JsonRpcResponse) {
    let mut out = stdout.lock();
    let _ = serde_json::to_writer(&mut out, resp);
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

// ---------------------------------------------------------------------------
// MCP method handlers
// ---------------------------------------------------------------------------

fn handle_initialize(id: serde_json::Value, server_name: &str) -> JsonRpcResponse {
    JsonRpcResponse::success(
        id,
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": server_name,
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
    )
}

fn handle_tools_list(id: serde_json::Value, tools: &[McpToolDef]) -> JsonRpcResponse {
    JsonRpcResponse::success(id, serde_json::json!({ "tools": tools }))
}

/// `ToolCall.id` — and so `ToolCtx.call_id` — for a `tools/call` request: its
/// JSON-RPC id, a string verbatim, a number in decimal. No id = empty = no
/// call id (never a random one). `tengu tool call --batch` maps a line's
/// `call_id` with it too (`cli/tool.rs`).
pub(crate) fn call_id(id: &serde_json::Value) -> String {
    match id {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

async fn handle_tools_call(
    id: serde_json::Value,
    params: &serde_json::Value,
    executor: &dyn ToolExecutor,
    max_result_chars: usize,
    secrets: &SecretRegistry,
) -> JsonRpcResponse {
    let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    let call = ToolCall {
        id: call_id(&id),
        name: tool_name.to_string(),
        arguments,
    };

    let (title, detail) = build_tool_activity_text(&call);
    let started_at = std::time::Instant::now();
    info!(
        tool = %call.name,
        call_id = %call.id,
        title = %title,
        detail = detail.as_deref().unwrap_or(""),
        "MCP tool call started"
    );

    match executor.execute(&call, &[]).await {
        Ok(result) => {
            let truncated = truncate_mcp_result(&result, max_result_chars);
            info!(
                tool = %call.name,
                elapsed_ms = started_at.elapsed().as_millis(),
                result_len = result.len(),
                truncated = truncated.len() < result.len(),
                "MCP tool call completed"
            );
            JsonRpcResponse::success(
                id,
                serde_json::json!({
                    "content": [{ "type": "text", "text": truncated }],
                    "isError": false
                }),
            )
        }
        Err(e) => {
            // Errors bypass `SanitizedToolExecutor`; redact them here.
            let text = secrets.redact(&format!("ERROR: {}", e));
            warn!(
                tool = %call.name,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = %text,
                "MCP tool call failed"
            );
            JsonRpcResponse::success(
                id,
                serde_json::json!({
                    "content": [{ "type": "text", "text": text }],
                    "isError": true
                }),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// MCP result truncation — prevents unbounded context growth in Claude Code.
// ---------------------------------------------------------------------------

/// Maximum characters per MCP tool result returned to Claude CLI.
/// The Claude Code engine has no per-turn compaction (unlike the OpenRouter
/// engine's 2-phase pruning), so every byte here stays in context for the
/// entire session.  50 000 chars ≈ ~12 500 tokens — enough for any useful
/// response while preventing schema-introspection blowup.
const MAX_MCP_RESULT_CHARS: usize = 50_000;

fn truncate_mcp_result(result: &str, max_chars: usize) -> String {
    match crate::domain::token::truncate_at_boundary(result, max_chars) {
        None => result.to_string(),
        Some((prefix, end)) => format!(
            "{}\n\n[truncated — showing {} of {} chars]",
            prefix,
            end,
            result.len()
        ),
    }
}

// ---------------------------------------------------------------------------
// Tool executor construction — mirrors `crate::bootstrap::tools::build_tool_executor`
// but is tailored for the bridge (no skill registry, no cancel flag, no
// channel-specific activity port).
// ---------------------------------------------------------------------------

/// Parse `TENGU_BRIDGE_SCOPES`. Unset → empty map (every tool permissive).
/// Unparsable → warn + empty map, so a malformed export never bricks the
/// bridge (fail-soft on config plumbing; the per-call `check_*` gates are
/// what actually enforce).
fn bridge_scopes_from_env() -> HashMap<String, ToolScope> {
    match std::env::var(TENGU_BRIDGE_SCOPES_ENV) {
        Ok(json) => match serde_json::from_str::<HashMap<String, ToolScope>>(&json) {
            Ok(map) => map,
            Err(e) => {
                warn!(error = %e, "bridge: failed to parse {TENGU_BRIDGE_SCOPES_ENV}; all tools permissive");
                HashMap::new()
            }
        },
        Err(_) => HashMap::new(),
    }
}

/// `[[mcp_servers]]` passed by the Claude Code engine as
/// `TENGU_BRIDGE_MCP_SERVERS`. Absent or unparsable = none (warns on the latter).
fn bridge_mcp_servers_from_env() -> Vec<McpServerConfig> {
    match std::env::var(TENGU_BRIDGE_MCP_SERVERS_ENV) {
        Ok(json) => serde_json::from_str(&json).unwrap_or_else(|e| {
            warn!(error = %e, "bridge: unparsable {TENGU_BRIDGE_MCP_SERVERS_ENV}; no external MCP tools");
            Vec::new()
        }),
        Err(_) => Vec::new(),
    }
}

/// The agent the bridge runs tools as.
///
/// | Config file + `[agents.<TENGU_BRIDGE_AGENT>]` | Agent |
/// |---|---|
/// | both present | that block as `Config::load` folded it (scopes with `[default_scopes]`, `sandbox` sections, `no_shell_fallback`, signer); under a `run-agent` child each configured scope also gets the workspace root, like the child's own executor |
/// | either absent (warn) | `Config::default()`'s `main` with `TENGU_BRIDGE_SCOPES`; shell-free when a loaded config's agents are |
///
/// Then every `WORKSPACE_TOOLS` name in the `TENGU_BRIDGE_TOOLS` allow-list
/// joins `workspace_tools` — the merge `bootstrap::tools::subagent_config`
/// applies to an agent's `tools`.
fn bridge_agent_config(setup: &BridgeSetup, allowed: &HashSet<String>) -> Result<AgentConfig> {
    let found = setup
        .config
        .as_ref()
        .zip(setup.agent.as_deref())
        .and_then(|(config, name)| config.agents.get(name));
    let mut agent = match found {
        Some(agent) => {
            let mut agent = agent.clone();
            agent.workspace = agent
                .workspace
                .as_ref()
                .map(|p| crate::config::paths::expand_tilde(p));
            if setup.subagent {
                crate::bootstrap::tools::grant_workspace_root(&mut agent.scopes, &setup.workspace);
            }
            info!(agent = %setup.agent.as_deref().unwrap_or_default(), "mcp-bridge: tools run as the configured agent");
            agent
        }
        None => {
            warn!(
                config = %crate::config::paths::default_config_path().display(),
                config_loaded = setup.config.is_some(),
                agent = setup.agent.as_deref().unwrap_or("(unset)"),
                "mcp-bridge: no [agents.<{TENGU_BRIDGE_AGENT_ENV}>] in the config — tools run as the default `main` agent with {TENGU_BRIDGE_SCOPES_ENV}"
            );
            let mut agent = Config::default()
                .agents
                .remove("main")
                .ok_or_else(|| anyhow::anyhow!("default config missing 'main' agent"))?;
            agent.scopes = setup.env_scopes.clone();
            if let Some(config) = &setup.config {
                agent.no_shell_fallback = config.agents.values().any(|a| a.no_shell_fallback);
            }
            agent
        }
    };
    for name in crate::domain::tools::WORKSPACE_TOOLS {
        if allowed.contains(*name) && !agent.workspace_tools.iter().any(|w| w == name) {
            agent.workspace_tools.push(name.to_string());
        }
    }
    Ok(agent)
}

/// Memory for `memory_ingest` / `memory_search` / `persistent_store`, built
/// only when one is requested. With a config: exactly as a `run-agent` child
/// (`[memory] enabled` → `build_memory_manager_async`). Without: the disk
/// store under `<workspace>/memory` when `OPENROUTER_API_KEY` is set.
async fn bridge_memory(
    setup: &BridgeSetup,
    allowed: &HashSet<String>,
) -> Option<Arc<MemoryManager>> {
    let needs_memory = allowed.contains("memory_ingest")
        || allowed.contains("memory_search")
        || allowed.contains(crate::adapters::outbound::tools::memory::PERSISTENT_STORE_TOOL_NAME);
    if !needs_memory {
        return None;
    }
    if let Some(config) = &setup.config {
        if !config.memory.enabled {
            return None;
        }
        return Some(
            crate::bootstrap::memory::build_memory_manager_async(
                &config.memory,
                Some(&setup.workspace),
            )
            .await,
        );
    }
    let Ok(api_key) = std::env::var("OPENROUTER_API_KEY") else {
        warn!("Memory tools requested but OPENROUTER_API_KEY not set — skipping");
        return None;
    };
    match DiskVectorStore::new(&setup.workspace.join("memory")) {
        Ok(store) => {
            let store: Arc<dyn VectorStore> = Arc::new(store);
            let embedder = Arc::new(Embedder::new(api_key, DEFAULT_EMBEDDING_MODEL.to_string()));
            let manager = Arc::new(MemoryManager::new());
            manager.set_vector_backend(embedder, store).await;
            Some(manager)
        }
        Err(e) => {
            warn!(error = %e, "bridge failed to init disk memory store");
            None
        }
    }
}

async fn build_bridge_executor(
    setup: &BridgeSetup,
    secret_registry: &Arc<SecretRegistry>,
) -> Result<PluginToolExecutor> {
    let workspace = setup.workspace.as_path();
    let allowed_names: HashSet<String> = setup.tools.iter().map(|t| t.name.clone()).collect();
    let allowed_list: Vec<String> = allowed_names.iter().cloned().collect();

    let agent_config = bridge_agent_config(setup, &allowed_names)?;

    let shell: Arc<dyn crate::ports::shell::ShellExecutionPort> =
        Arc::new(LocalShellExecutor::new());

    let http_client = crate::adapters::outbound::egress::policy()
        .tool_client(std::time::Duration::from_secs(60))?;

    let memory_manager_handle = bridge_memory(setup, &allowed_names).await;
    let memory_config = setup
        .config
        .as_ref()
        .map(|c| c.memory.clone())
        .unwrap_or_default();

    let plugin_ctx = PluginCtx {
        workspace,
        config: &agent_config,
        http: http_client.clone(),
        shell: Arc::clone(&shell),
        memory_manager: memory_manager_handle
            .clone()
            .map(|m| m as Arc<dyn crate::ports::memory::MemoryService>),
        secret_registry: Arc::clone(secret_registry),
    };

    let mut registry = ToolRegistry::new();

    // `SkillPlugin` is never registered here — it needs a `SkillRegistry` the
    // bridge's subprocess context can't construct. `McpPlugin` only for the
    // `[[mcp_servers]]` the Claude Code engine passed in (see below); a
    // standalone bridge registered in someone's own Claude Code gets none, so
    // their MCP manifest isn't re-advertised.

    // Same catalog as the in-process executor (`build_tool_executor`).
    crate::adapters::outbound::tools::register_catalog(
        &mut registry,
        &plugin_ctx,
        &allowed_names,
        &allowed_list,
        crate::adapters::outbound::tools::CatalogOpts {
            cancel: None,
            memory_config: Some(&memory_config),
        },
    )
    .await;

    // External MCP servers whose `{server}__{tool}` names were requested —
    // proxied here so the calls go through this process's egress policy.
    if !setup.mcp_servers.is_empty() {
        let plugin =
            crate::adapters::outbound::mcp_client::McpPlugin::new(setup.mcp_servers.clone());
        if let Err(e) = registry
            .register_plugin(&plugin, &plugin_ctx, &allowed_list)
            .await
        {
            warn!(error = %e, "bridge: mcp plugin failed — external MCP tools unavailable");
        }
    }

    // Per-tool scopes — ENFORCED, resolved exactly like
    // `crate::bootstrap::tools::build_tool_executor`: the agent's configured
    // entry (see `bridge_agent_config`), else `permissive_scope` — minus the
    // shell when the agent has `no_shell_fallback` (signing sandbox).
    let scopes = crate::bootstrap::tools::resolve_tool_scopes(
        workspace,
        &agent_config.scopes,
        registry.tool_names(),
        agent_config.no_shell_fallback,
    );

    Ok(PluginToolExecutor {
        registry,
        workspace: workspace.to_path_buf(),
        shell: Arc::clone(&shell),
        http: http_client,
        memory_manager: memory_manager_handle
            .map(|m| m as Arc<dyn crate::ports::memory::MemoryService>),
        secret_registry: Arc::clone(secret_registry),
        activity: Arc::new(BridgeActivity),
        scopes,
        // Borrowed into every `ToolCtx.agent_config`: the same block the
        // in-process executor hands its tools.
        agent_config: Some(agent_config),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::tool::{Tool, ToolCtx, ToolOutput};
    use serde_json::{json, Value};
    use tempfile::TempDir;

    /// A sandbox file: `[xmarket]` + an agent with a scoped tool.
    const SANDBOX: &str = r#"
[xmarket]
state = "bridge-test"

[agents.main]
default = true
engine = "openrouter"
model = "anthropic/claude-haiku-4.5"

[agents.bridged]
engine = "claude_code"
model = "claude-haiku-4-5"
tools = ["http_request", "read_file"]

[agents.bridged.scopes.http_request]
net_hosts = ["api.hyperliquid.xyz"]
"#;

    fn setup(
        workspace: &std::path::Path,
        tools: &[&str],
        config: Option<Config>,
        agent: Option<&str>,
    ) -> BridgeSetup {
        BridgeSetup {
            workspace: workspace.to_path_buf(),
            tools: tools
                .iter()
                .map(|n| ToolDef::new(*n, "d", json!({"type": "object"})))
                .collect(),
            config,
            agent: agent.map(str::to_string),
            subagent: false,
            env_scopes: HashMap::new(),
            mcp_servers: Vec::new(),
        }
    }

    fn no_secrets() -> Arc<SecretRegistry> {
        Arc::new(SecretRegistry::new())
    }

    /// Echoes what a tool sees: `call_id` and the agent's `xm_state_dir`.
    struct Probe(ToolDef);

    #[async_trait::async_trait]
    impl Tool for Probe {
        fn definition(&self) -> &ToolDef {
            &self.0
        }
        async fn execute(&self, _args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
            // scope: pure-compute
            let state = ctx
                .agent_config
                .and_then(|a| a.sandbox.xm_state_dir.clone());
            Ok(ToolOutput::from(
                json!({"call_id": ctx.call_id, "xm_state_dir": state}).to_string(),
            ))
        }
    }

    fn with_probe(mut exec: PluginToolExecutor) -> PluginToolExecutor {
        exec.registry
            .register_tool(Arc::new(Probe(ToolDef::new("probe", "d", json!({})))));
        exec
    }

    async fn call(exec: &dyn ToolExecutor, id: Value, tool: &str, args: Value) -> Value {
        let resp = handle_tools_call(
            id,
            &json!({"name": tool, "arguments": args}),
            exec,
            MAX_MCP_RESULT_CHARS,
            &SecretRegistry::new(),
        )
        .await;
        serde_json::to_value(&resp).unwrap()["result"].clone()
    }

    fn text(result: &Value) -> &str {
        result["content"][0]["text"].as_str().unwrap()
    }

    /// Built from a sandbox file: the agent's scopes (not TENGU_BRIDGE_SCOPES)
    /// and sandbox sections reach the tools; a `run-agent` child's bridge
    /// also grants the workspace root, like the child's executor.
    #[tokio::test]
    async fn bridge_uses_the_sandbox_agent_config() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, SANDBOX).unwrap();
        let ws = TempDir::new().unwrap();
        let mut s = setup(
            ws.path(),
            &["http_request", "read_file"],
            Some(Config::load(&file).unwrap()),
            Some("bridged"),
        );
        s.env_scopes = HashMap::from([(
            "http_request".to_string(),
            ToolScope {
                net_hosts: vec!["*".to_string()],
                ..Default::default()
            },
        )]);

        let exec = build_bridge_executor(&s, &no_secrets()).await.unwrap();
        let http = &exec.scopes["http_request"];
        assert_eq!(http.net_hosts, ["api.hyperliquid.xyz"]);
        assert!(http.check_net_host("evil.example").is_err());
        assert!(http.fs_roots.is_empty(), "not a run-agent child: no grant");
        assert!(exec.scopes["read_file"].check_fs_read(ws.path()).is_ok());

        let exec = sanitized(with_probe(exec), &no_secrets());
        let seen: Value = serde_json::from_str(text(
            &call(exec.as_ref(), json!(1), "probe", json!({})).await,
        ))
        .unwrap();
        let state = seen["xm_state_dir"].as_str().unwrap();
        assert!(state.ends_with("state/bridge-test"), "{state}");

        s.subagent = true;
        let exec = build_bridge_executor(&s, &no_secrets()).await.unwrap();
        assert_eq!(exec.scopes["http_request"].fs_roots, [ws.path()]);
    }

    /// No config file or no such agent: the default `main` agent with the
    /// engine's TENGU_BRIDGE_SCOPES (today's standalone behaviour).
    #[tokio::test]
    async fn unresolved_agent_falls_back_to_env_scopes() {
        let ws = TempDir::new().unwrap();
        let mut s = setup(ws.path(), &["http_request"], None, Some("ghost"));
        s.env_scopes = HashMap::from([(
            "http_request".to_string(),
            ToolScope {
                net_hosts: vec!["api.example.com".to_string()],
                ..Default::default()
            },
        )]);
        let exec = build_bridge_executor(&s, &no_secrets()).await.unwrap();
        assert_eq!(exec.scopes["http_request"].net_hosts, ["api.example.com"]);
        let agent = exec.agent_config.as_ref().unwrap();
        assert_eq!(agent.engine, "openrouter");
        assert!(agent.sandbox.xm_state_dir.is_none());
    }

    /// `no_shell_fallback` (signing sandbox) drops the shell from the
    /// fallback scope — also when the agent does not resolve.
    #[tokio::test]
    async fn no_shell_fallback_removes_the_fallback_shell() {
        let ws = TempDir::new().unwrap();
        let mut signing = Config::default();
        signing.solana.signer_key_file = Some("/keys/signer.json".into());
        signing.fold_default_scopes();
        let shell_ok =
            |exec: &PluginToolExecutor| exec.scopes["run_command"].check_shell_bin("ls").is_ok();

        let s = setup(
            ws.path(),
            &["run_command"],
            Some(signing.clone()),
            Some("main"),
        );
        assert!(!shell_ok(
            &build_bridge_executor(&s, &no_secrets()).await.unwrap()
        ));
        let s = setup(ws.path(), &["run_command"], Some(signing), Some("ghost"));
        assert!(!shell_ok(
            &build_bridge_executor(&s, &no_secrets()).await.unwrap()
        ));

        let mut plain = Config::default();
        plain.fold_default_scopes();
        let s = setup(ws.path(), &["run_command"], Some(plain), Some("main"));
        assert!(shell_ok(
            &build_bridge_executor(&s, &no_secrets()).await.unwrap()
        ));
    }

    /// A registered secret never leaves the bridge — tool text or error.
    #[tokio::test]
    async fn bridge_redacts_registered_secrets() {
        const KEY: &str = "sk-bridge-0123456789abcdef";
        let ws = TempDir::new().unwrap();
        std::fs::write(ws.path().join("notes.txt"), format!("api_key={KEY}\n")).unwrap();
        let mut reg = SecretRegistry::new();
        reg.register(KEY.to_string());
        let secrets = Arc::new(reg);
        let s = setup(ws.path(), &["read_file"], None, None);
        let exec = sanitized(build_bridge_executor(&s, &secrets).await.unwrap(), &secrets);

        let resp = |id: Value, path: String| {
            let exec = Arc::clone(&exec);
            let secrets = Arc::clone(&secrets);
            async move {
                let resp = handle_tools_call(
                    id,
                    &json!({"name": "read_file", "arguments": {"path": path}}),
                    exec.as_ref(),
                    MAX_MCP_RESULT_CHARS,
                    &secrets,
                )
                .await;
                serde_json::to_value(&resp).unwrap()["result"].clone()
            }
        };
        let ok = resp(json!(1), "notes.txt".into()).await;
        assert_eq!(ok["isError"], false);
        assert!(text(&ok).contains("api_key=[REDACTED]"), "{ok}");
        assert!(!ok.to_string().contains(KEY));

        let err = resp(json!(2), format!("{KEY}.txt")).await;
        assert_eq!(err["isError"], true);
        assert!(!err.to_string().contains(KEY), "{err}");
    }

    /// The JSON-RPC request id is the tool's `ToolCtx.call_id`.
    #[tokio::test]
    async fn request_id_is_the_call_id() {
        let ws = TempDir::new().unwrap();
        let s = setup(ws.path(), &["read_file"], None, None);
        let exec = sanitized(
            with_probe(build_bridge_executor(&s, &no_secrets()).await.unwrap()),
            &no_secrets(),
        );
        let seen = |r: Value| -> Value { serde_json::from_str(text(&r)).unwrap() };
        let n = seen(call(exec.as_ref(), json!(42), "probe", json!({})).await);
        assert_eq!(n["call_id"], "42");
        let s = seen(call(exec.as_ref(), json!("req-7"), "probe", json!({})).await);
        assert_eq!(s["call_id"], "req-7");
        let none = seen(call(exec.as_ref(), Value::Null, "probe", json!({})).await);
        assert!(none["call_id"].is_null());
    }
}
