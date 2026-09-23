//! Stdio MCP bridge — exposes Tengu tools to Claude Code via the MCP protocol.
//!
//! Runs as `tengu mcp-bridge` subprocess. Claude CLI spawns this as an MCP stdio
//! server and routes tool calls through it. The bridge builds its own
//! `ToolRegistry` via the plugin architecture (same plugins the channel runtime
//! uses) and dispatches each `tools/call` through a `PluginToolExecutor`.
//!
//! Protocol: JSON-RPC 2.0 over stdin/stdout (newline-delimited).

use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{info, warn};

use crate::adapters::memory::manager::MemoryManager;
use crate::adapters::memory::vector::{DiskVectorStore, Embedder};
use crate::config::Config;
use crate::domain::memory::DEFAULT_EMBEDDING_MODEL;
use crate::ports::engine::ToolExecutor;
use crate::ports::memory::VectorStore;
// Phase 7.7 — plugin imports removed; bridge delegates to
// `channel_runtime::register_core_plugins` which has its own local imports.
// Keeps the bridge file focused on stdio JSON-RPC + executor wiring rather
// than re-listing the plugin set.
use crate::adapters::secret_builder::SecretRegistry;
use crate::adapters::shell_executor::LocalShellExecutor;
use crate::adapters::tool_builder::build_tool_activity_text;
use crate::application::tools::registry::{PluginToolExecutor, ToolRegistry};
use crate::domain::message::{ToolCall, ToolDef};
use crate::domain::scope::ToolScope;
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
// Bridge entry point
// ---------------------------------------------------------------------------

/// Run the MCP bridge stdio server. Uses the ambient tokio runtime (the bridge
/// subprocess is launched from within Tengu's `#[tokio::main]`, so creating a
/// second runtime here would panic). Returns when stdin closes.
pub async fn run_mcp_bridge() -> Result<()> {
    // Parent's policy arrives as TENGU_EGRESS (wins inside `install`). A
    // standalone bridge (registered directly in a user's Claude Code) follows
    // the operator's own config chain — `$TENGU_CONFIG` / `<TENGU_HOME>/config.toml`
    // — and only falls back to the built-in default (`network = "tor"`) when
    // there is no config file at all.
    let standalone_egress = {
        let path = crate::config::paths::default_config_path();
        if path.is_file() {
            crate::config::Config::load(&path)
                .with_context(|| format!("mcp-bridge: load [egress] from {}", path.display()))?
                .egress
        } else {
            crate::config::egress::EgressConfig::default()
        }
    };
    crate::adapters::egress::install(&standalone_egress)?;
    let workspace = std::env::var("TENGU_BRIDGE_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());

    let tools: Vec<ToolDef> = match std::env::var("TENGU_BRIDGE_TOOLS") {
        Ok(json) => serde_json::from_str(&json)
            .map_err(|e| anyhow::anyhow!("Failed to parse TENGU_BRIDGE_TOOLS: {}", e))?,
        Err(_) => vec![],
    };

    let max_result_chars: usize = std::env::var("TENGU_BRIDGE_MAX_RESULT_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_MCP_RESULT_CHARS);

    serve_mcp_stdio(&workspace, tools, max_result_chars, "tengu-tools").await
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
    let workspace = std::env::var("TENGU_BRIDGE_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());

    let max_result_chars: usize = std::env::var("TENGU_BRIDGE_MAX_RESULT_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(MAX_MCP_RESULT_CHARS);

    let tools = crate::adapters::plugins::agentic_memory::tool_defs();
    serve_mcp_stdio(&workspace, tools, max_result_chars, "tengu-agentic-memory").await
}

/// Shared stdio JSON-RPC serve loop. Builds a `PluginToolExecutor` over `tools`
/// and dispatches `initialize` / `tools/list` / `tools/call` / `ping` until
/// stdin closes. Used by both `run_mcp_bridge` and
/// `run_agentic_memory_mcp_server`.
async fn serve_mcp_stdio(
    workspace: &Path,
    tools: Vec<ToolDef>,
    max_result_chars: usize,
    server_name: &'static str,
) -> Result<()> {
    let executor = build_bridge_executor(workspace, &tools).await?;
    let mcp_tools: Vec<McpToolDef> = tools.iter().map(McpToolDef::from).collect();

    info!(
        workspace = %workspace.display(),
        tool_count = tools.len(),
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
                handle_tools_call(id, &request.params, &executor, max_result_chars).await
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

async fn handle_tools_call(
    id: serde_json::Value,
    params: &serde_json::Value,
    executor: &PluginToolExecutor,
    max_result_chars: usize,
) -> JsonRpcResponse {
    let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    let call = ToolCall {
        id: uuid::Uuid::new_v4().to_string(),
        name: tool_name.to_string(),
        arguments,
    };

    let (title, detail) = build_tool_activity_text(&call);
    let started_at = std::time::Instant::now();
    info!(
        tool = %call.name,
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
            warn!(
                tool = %call.name,
                elapsed_ms = started_at.elapsed().as_millis(),
                error = %e,
                "MCP tool call failed"
            );
            JsonRpcResponse::success(
                id,
                serde_json::json!({
                    "content": [{ "type": "text", "text": format!("ERROR: {}", e) }],
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
// Tool executor construction — mirrors `channel_runtime::build_tool_executor`
// but is tailored for the bridge (no skill registry, no cancel flag, no
// channel-specific activity port).
// ---------------------------------------------------------------------------

/// Env var carrying the calling agent's per-tool scope map into the bridge
/// subprocess. Written by `ClaudeCodeEngine::build_mcp_config_json`.
pub(crate) const TENGU_BRIDGE_SCOPES_ENV: &str = "TENGU_BRIDGE_SCOPES";

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

async fn build_bridge_executor(workspace: &Path, tools: &[ToolDef]) -> Result<PluginToolExecutor> {
    let allowed_names: HashSet<String> = tools.iter().map(|t| t.name.clone()).collect();
    let allowed_list: Vec<String> = allowed_names.iter().cloned().collect();

    // Bridge-side defaults: use the default main agent config for plugin
    // gating decisions (e.g. `workspace_tools` opt-ins). The bridge runs as a
    // subprocess and has no agent-specific config, so using defaults matches
    // the pre-A9 bridge behaviour.
    //
    // Phase 7.6 — synthesize workspace_tools from the incoming `tools`
    // allow-list. The LLM-advertised tools (passed via TENGU_BRIDGE_TOOLS)
    // ARE the authoritative allow-list for the bridge process; defaulting
    // to an empty workspace_tools causes plugins like MemoryPlugin to skip
    // registering persistent_store even when memory is available, because
    // the plugin gates on `ctx.config.workspace_tools.contains(...)`.
    let config = Config::default();
    let mut agent_config = config
        .agents
        .get("main")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("default config missing 'main' agent"))?;
    // Phase 7.7 refactor #5 — same allowlist `channel_runtime::subagent_config` uses.
    agent_config.workspace_tools = crate::adapters::channel_runtime::WORKSPACE_TOOLS_ALLOWLIST
        .iter()
        .filter(|t| allowed_names.contains(**t))
        .map(|t| t.to_string())
        .collect();

    let shell: Arc<dyn crate::ports::shell::ShellExecutionPort> =
        Arc::new(LocalShellExecutor::new());

    let http_client =
        crate::adapters::egress::policy().tool_client(std::time::Duration::from_secs(60))?;

    let secret_registry = Arc::new(SecretRegistry::new());

    // Memory manager: only built when memory tools are requested and the
    // API key is present. Errors fall through to None so other tools still
    // work. Wraps the shared `Embedder` + `DiskVectorStore` pair so
    // `memory_ingest` / `memory_search` / `persistent_store` all talk to
    // the same backing store.
    let needs_memory = allowed_names.contains("memory_ingest")
        || allowed_names.contains("memory_search")
        || allowed_names.contains(crate::adapters::plugins::memory::PERSISTENT_STORE_TOOL_NAME);
    let memory_manager_handle: Option<Arc<MemoryManager>> = if needs_memory {
        match std::env::var("OPENROUTER_API_KEY") {
            Ok(api_key) => {
                let memory_dir = workspace.join("memory");
                match DiskVectorStore::new(&memory_dir) {
                    Ok(store) => {
                        let store: Arc<dyn VectorStore> = Arc::new(store);
                        let embedder =
                            Arc::new(Embedder::new(api_key, DEFAULT_EMBEDDING_MODEL.to_string()));

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
            Err(_) => {
                warn!("Memory tools requested but OPENROUTER_API_KEY not set — skipping");
                None
            }
        }
    } else {
        None
    };

    let plugin_ctx = PluginCtx {
        workspace,
        config: &agent_config,
        http: http_client.clone(),
        shell: Arc::clone(&shell),
        memory_manager: memory_manager_handle.clone(),
        secret_registry: Arc::clone(&secret_registry),
    };

    let mut registry = ToolRegistry::new();

    // NOTE: the outbound bridge deliberately does NOT register the inbound
    // `McpPlugin` or `SkillPlugin`. External Claude Code clients are their
    // own host with their own MCP server access; re-advertising tengu's
    // inbound MCP manifest here would cause name collisions and double-hop
    // routing. SkillPlugin needs a `SkillRegistry` that the bridge's
    // standalone subprocess context can't sensibly construct.

    // Phase 7.7 — register the shared plugin set via the consolidated
    // helper. Adding a new shared plugin only requires editing
    // `register_core_plugins` in channel_runtime; this bridge picks it up
    // automatically. Bug B/C wouldn't have happened if this had been
    // consolidated from day one.
    crate::adapters::channel_runtime::register_core_plugins(
        &mut registry,
        &plugin_ctx,
        &allowed_names,
        &allowed_list,
        crate::adapters::channel_runtime::CoreRegistrationOpts {
            cancel: None,
            memory_config: Some(&crate::config::MemoryConfig::default()),
        },
    )
    .await;

    // Per-tool scopes — ENFORCED. The parent's `ClaudeCodeEngine` exports the
    // agent's scope map as `TENGU_BRIDGE_SCOPES` (serde_json of
    // `HashMap<String, ToolScope>`); tools without an entry fall back to
    // `permissive_scope`, exactly like `channel_runtime::build_tool_executor`.
    // A missing or unparsable env var degrades to all-permissive with a warn.
    let configured = bridge_scopes_from_env();
    let scopes = crate::adapters::channel_runtime::resolve_tool_scopes(
        workspace,
        &configured,
        registry.tool_names(),
    );

    Ok(PluginToolExecutor {
        registry,
        workspace: workspace.to_path_buf(),
        shell: Arc::clone(&shell),
        http: http_client,
        memory_manager: memory_manager_handle,
        secret_registry,
        activity: Arc::new(BridgeActivity),
        scopes,
        // Stream M — bridge has only the synthesized default-main config;
        // hand it to ToolCtx so distill (if invoked through the bridge)
        // sees the engine/model the bridge inherited rather than nothing.
        agent_config: Some(agent_config),
    })
}
