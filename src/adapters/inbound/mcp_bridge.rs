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
//! `no_shell_fallback`, `[memory]`, its shell skills (`skill_packages`) —
//! redacts the process secrets, and hands each tool the JSON-RPC request id
//! as `ToolCtx.call_id`. Without that file or agent (a standalone bridge in
//! someone's own Claude Code) it falls back to `Config::default()`'s `main`
//! agent + `TENGU_BRIDGE_SCOPES`, with a warn. A `run-agent` step's bridge
//! (`TENGU_BRIDGE_SUMMARY_FILE`) serves `compress_and_store` into the step's
//! summary file and answers `stored — stop now`; any other bridge refuses it
//! with the reason. Each tool gets the run's conversation
//! (`call_conversation`: the engine's `TENGU_BRIDGE_TRANSCRIPT_FILE`, ending
//! in the call), as in-process tools get the loop's messages; the
//! `[[mcp_servers]]` it proxies are the ones `TENGU_BRIDGE_MCP_SERVERS`
//! names, from the loaded config.
//! Env contract: `adapters/outbound/bridge_env.rs`; doc: `docs/mcp-bridge.md`.
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

use crate::adapters::outbound::bridge_env::{
    TENGU_BRIDGE_AGENT_ENV, TENGU_BRIDGE_GRANT_WORKSPACE_ENV, TENGU_BRIDGE_MCP_SERVERS_ENV,
    TENGU_BRIDGE_SCOPES_ENV, TENGU_BRIDGE_SUMMARY_FILE_ENV, TENGU_BRIDGE_TRANSCRIPT_FILE_ENV,
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
use crate::domain::message::{Message, Role, ToolCall, ToolDef};
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
    /// `TENGU_BRIDGE_TOOLS`: the allow-list, and what `tools/list` returns —
    /// minus what the agent's `[generation]` refuses (`bridge_tools`).
    tools: Vec<ToolDef>,
    /// `TENGU_CONFIG` (else `<TENGU_HOME>/config.toml`), loaded; `None` = no file.
    config: Option<Config>,
    /// `TENGU_BRIDGE_AGENT`.
    agent: Option<String>,
    /// `TENGU_BRIDGE_GRANT_WORKSPACE=1`, set by a `run-agent` step's engine
    /// (and the doctor's smoke turn): like the step's own executor, every
    /// configured scope also gets the workspace as an fs root
    /// (`bootstrap::tools::grant_workspace_root`).
    grant_workspace: bool,
    /// `TENGU_BRIDGE_SUMMARY_FILE`: where `compress_and_store` writes the
    /// step's summary (`StepSummary`); `None` = refused with the reason.
    summary_file: Option<PathBuf>,
    /// `TENGU_BRIDGE_TRANSCRIPT_FILE`: the run's conversation, read per call
    /// (`call_conversation`); `None` = none.
    transcript_file: Option<PathBuf>,
    /// `TENGU_BRIDGE_SCOPES` — used only by the default-`main` fallback.
    env_scopes: HashMap<String, ToolScope>,
    /// `TENGU_BRIDGE_MCP_SERVERS`, names resolved against `config`.
    mcp_servers: Vec<McpServerConfig>,
}

impl BridgeSetup {
    fn from_env(tools: Vec<ToolDef>, config: Option<Config>) -> Self {
        let mcp_servers = bridge_mcp_servers_from_env(config.as_ref());
        Self {
            workspace: std::env::var("TENGU_BRIDGE_WORKSPACE")
                .map(PathBuf::from)
                .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default()),
            tools,
            config,
            agent: std::env::var(TENGU_BRIDGE_AGENT_ENV)
                .ok()
                .filter(|a| !a.is_empty()),
            grant_workspace: std::env::var(TENGU_BRIDGE_GRANT_WORKSPACE_ENV)
                .is_ok_and(|v| v == "1"),
            summary_file: std::env::var_os(TENGU_BRIDGE_SUMMARY_FILE_ENV)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            transcript_file: std::env::var_os(TENGU_BRIDGE_TRANSCRIPT_FILE_ENV)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            env_scopes: bridge_scopes_from_env(),
            mcp_servers,
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
    let executor: Arc<dyn ToolExecutor> = Arc::new(StepSummary {
        inner: sanitized(build_bridge_executor(&setup, &secrets).await?, &secrets),
        file: setup.summary_file.clone(),
    });
    let listed = bridge_tools(&setup)?;
    let mcp_tools: Vec<McpToolDef> = listed.iter().map(McpToolDef::from).collect();

    info!(
        workspace = %setup.workspace.display(),
        tool_count = listed.len(),
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
                    setup.transcript_file.as_deref(),
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

/// The `run-agent` step protocol tool (`skill_lifecycle::compress_and_store`).
const COMPRESS_AND_STORE: &str =
    crate::adapters::outbound::tools::skill_lifecycle::compress_and_store::NAME;

/// The reply to a stored step summary: the step is over. The Claude Code
/// engine ends the CLI run after this call (once the round's other calls
/// are answered), as the in-process loop stops after its round.
const STORED_STOP_NOW: &str = "stored — stop now";

/// `compress_and_store` through the bridge — what the `run-agent` loop does
/// for in-process engines (`cli/run_agent.rs`), for a Claude Code step:
///
/// | `file` (`TENGU_BRIDGE_SUMMARY_FILE`) | Call |
/// |---|---|
/// | set (a `run-agent` step) | `summary` written to it (replacing an earlier one) → `stored — stop now`; the step reads it back as its IPC summary |
/// | unset | error naming why: no step takes a summary here — answer in plain text |
///
/// Every other tool goes to `inner`.
struct StepSummary {
    inner: Arc<dyn ToolExecutor>,
    file: Option<PathBuf>,
}

impl StepSummary {
    fn store(&self, call: &ToolCall) -> Result<String> {
        let Some(file) = &self.file else {
            anyhow::bail!(
                "compress_and_store ends a `tengu run-agent` plan step, and this bridge serves \
                 none (no {TENGU_BRIDGE_SUMMARY_FILE_ENV}) — give your summary as plain text instead"
            );
        };
        let summary = call
            .arguments
            .get("summary")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("compress_and_store: `summary` (a string) is required")
            })?;
        std::fs::write(file, summary)
            .with_context(|| format!("compress_and_store: write {}", file.display()))?;
        info!(
            chars = summary.chars().count(),
            "compress_and_store: step summary stored"
        );
        Ok(STORED_STOP_NOW.to_string())
    }
}

#[async_trait::async_trait]
impl ToolExecutor for StepSummary {
    async fn execute(
        &self,
        call: &ToolCall,
        messages: &[crate::domain::message::Message],
    ) -> Result<String> {
        Ok(self.execute_typed(call, messages).await?.text)
    }

    async fn execute_typed(
        &self,
        call: &ToolCall,
        messages: &[crate::domain::message::Message],
    ) -> Result<crate::ports::tool::ToolOutput> {
        if call.name == COMPRESS_AND_STORE {
            return self.store(call).map(crate::ports::tool::ToolOutput::from);
        }
        self.inner.execute_typed(call, messages).await
    }
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

/// This process's call-id nonce: a uuid (32 hex digits) minted once. The
/// Claude CLI numbers its JSON-RPC requests from the start again in every
/// session, and each turn or plan step runs a new CLI and so a new bridge;
/// the nonce keeps an exec tool's idempotency key (`client_order_id` =
/// `ToolCtx.call_id`) from matching an earlier process's order, which would
/// replay that order's stored fill instead of placing this one.
pub(crate) fn call_nonce() -> &'static str {
    static NONCE: once_cell::sync::Lazy<String> =
        once_cell::sync::Lazy::new(|| uuid::Uuid::new_v4().simple().to_string());
    NONCE.as_str()
}

/// `ToolCall.id` — and so `ToolCtx.call_id` — for a `tools/call` request:
/// `mcp:<call_nonce>:<JSON-RPC id>` (the id a string verbatim, a number in
/// decimal). No id = empty = no call id (never a random one). `tengu tool
/// call` maps `--call-id` and a batch line's `call_id` with it too
/// (`cli/tool.rs`).
pub(crate) fn call_id(id: &serde_json::Value) -> String {
    let raw = match id {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    if raw.is_empty() {
        return raw;
    }
    format!("mcp:{}:{raw}", call_nonce())
}

/// The conversation a tool call sees (`ToolCtx.conversation` — what
/// `skill_distill` seeds fixtures from), as the in-process loops hand over
/// their messages: the transcript (`TENGU_BRIDGE_TRANSCRIPT_FILE`, a JSON
/// array of `Message`, or `tengu tool call --transcript`) ending in an
/// assistant message that holds this call.
///
/// | Transcript | Conversation |
/// |---|---|
/// | its last assistant message holds the call, unanswered (only tool messages after it) | as read |
/// | otherwise — the engine had not read that stream line yet, or the file is static | as read + an assistant message with the call |
/// | none, or unreadable (warned) | empty: a tool that needs one refuses |
pub(crate) fn call_conversation(transcript: Option<&Path>, call: &ToolCall) -> Vec<Message> {
    let Some(path) = transcript else {
        return Vec::new();
    };
    let read = std::fs::read(path)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| serde_json::from_slice::<Vec<Message>>(&bytes).map_err(Into::into));
    let mut messages = match read {
        Ok(messages) => messages,
        Err(e) => {
            warn!(file = %path.display(), error = %e, "transcript unreadable — the call gets no conversation");
            return Vec::new();
        }
    };
    if !ends_in_call(&messages, call) {
        messages.push(Message {
            role: Role::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_calls: Some(vec![call.clone()]),
        });
    }
    messages
}

/// The last assistant message of `messages` holds `call` (same name and
/// arguments), only tool messages follow it, and none answers that call.
fn ends_in_call(messages: &[Message], call: &ToolCall) -> bool {
    let Some(i) = messages
        .iter()
        .rposition(|m| matches!(m.role, Role::Assistant))
    else {
        return false;
    };
    let after = &messages[i + 1..];
    if !after.iter().all(|m| matches!(m.role, Role::Tool)) {
        return false;
    }
    let answered: HashSet<&str> = after
        .iter()
        .filter_map(|m| m.tool_call_id.as_deref())
        .collect();
    messages[i].tool_calls.iter().flatten().any(|c| {
        c.name == call.name && c.arguments == call.arguments && !answered.contains(c.id.as_str())
    })
}

async fn handle_tools_call(
    id: serde_json::Value,
    params: &serde_json::Value,
    executor: &dyn ToolExecutor,
    max_result_chars: usize,
    secrets: &SecretRegistry,
    transcript: Option<&Path>,
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
    let conversation = call_conversation(transcript, &call);

    let (title, detail) = build_tool_activity_text(&call);
    let started_at = std::time::Instant::now();
    info!(
        tool = %call.name,
        call_id = %call.id,
        title = %title,
        detail = detail.as_deref().unwrap_or(""),
        "MCP tool call started"
    );

    match executor.execute(&call, &conversation).await {
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
// but is tailored for the bridge (skills from `agent_skill_registry`, no
// cancel flag, no channel-specific activity port).
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
/// `TENGU_BRIDGE_MCP_SERVERS` (`resolve_mcp_servers`). Absent = none.
fn bridge_mcp_servers_from_env(config: Option<&Config>) -> Vec<McpServerConfig> {
    match std::env::var(TENGU_BRIDGE_MCP_SERVERS_ENV) {
        Ok(json) => resolve_mcp_servers(&json, config),
        Err(_) => Vec::new(),
    }
}

/// One `TENGU_BRIDGE_MCP_SERVERS` entry: a server name (what the Claude
/// Code engine writes) or a whole server config (a standalone bridge, tests).
#[derive(Deserialize)]
#[serde(untagged)]
enum ServerRef {
    Name(String),
    Config(McpServerConfig),
}

/// The servers `json` names: a name is taken from `config`'s
/// `[[mcp_servers]]` (`Config::load`: its `${VAR}`s expanded here, from the
/// env this bridge inherited), an object as given. Unparsable `json`, or a
/// name the config lacks, warns and leaves those tools unavailable.
fn resolve_mcp_servers(json: &str, config: Option<&Config>) -> Vec<McpServerConfig> {
    let refs: Vec<ServerRef> = match serde_json::from_str(json) {
        Ok(refs) => refs,
        Err(e) => {
            warn!(error = %e, "bridge: unparsable {TENGU_BRIDGE_MCP_SERVERS_ENV}; no external MCP tools");
            return Vec::new();
        }
    };
    refs.into_iter()
        .filter_map(|r| match r {
            ServerRef::Config(server) => Some(server),
            ServerRef::Name(name) => {
                let found = config
                    .and_then(|c| c.mcp_servers.iter().find(|s| s.name == name))
                    .cloned();
                if found.is_none() {
                    warn!(
                        server = %name,
                        config_loaded = config.is_some(),
                        "bridge: [[mcp_servers]] `{name}` is not in the loaded config — its tools are unavailable"
                    );
                }
                found
            }
        })
        .collect()
}

/// The agent the bridge runs tools as.
///
/// | Config file + `[agents.<TENGU_BRIDGE_AGENT>]` | Agent |
/// |---|---|
/// | both present | that block as `Config::load` folded it (scopes with `[default_scopes]`, `sandbox` sections, `no_shell_fallback`, signer); with `TENGU_BRIDGE_GRANT_WORKSPACE=1` (a `run-agent` step) each configured scope also gets the workspace root, like the step's own executor |
/// | either absent (warn) | `Config::default()`'s `main` with `TENGU_BRIDGE_SCOPES`; shell-free when a loaded config's agents are; bound to a loaded config's `[generation]` |
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
            if setup.grant_workspace {
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
                // The loaded config's `[generation]` still bounds what runs.
                let mut sections = (*agent.sandbox).clone();
                sections.generation = config.generation_scope.clone();
                agent.sandbox = Arc::new(sections);
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

/// `setup.tools` minus the ones the agent's bound generation refuses
/// (`[generation]`, `bootstrap::tools::within_generation`): neither listed
/// nor registered — as in-process executors.
fn bridge_tools(setup: &BridgeSetup) -> Result<Vec<ToolDef>> {
    let all: HashSet<String> = setup.tools.iter().map(|t| t.name.clone()).collect();
    let agent = bridge_agent_config(setup, &all)?;
    Ok(crate::bootstrap::tools::within_generation(
        &agent,
        &setup.tools,
    ))
}

async fn build_bridge_executor(
    setup: &BridgeSetup,
    secret_registry: &Arc<SecretRegistry>,
) -> Result<PluginToolExecutor> {
    let workspace = setup.workspace.as_path();
    let allowed_names: HashSet<String> = bridge_tools(setup)?.into_iter().map(|t| t.name).collect();
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

    // Shell skills: loaded as `run-agent` loads them (`agent_skill_registry`
    // — none when the agent runs no shell), kept to the requested names like
    // every other tool. A requested name counts as listed in
    // `skill_packages`: a composed plan step's skills (IPC `compose`) reach
    // the bridge only through the tool list its `run-agent` parent
    // advertised — as workspace-tool opt-ins do (`bridge_agent_config`).
    let mut skill_agent = agent_config.clone();
    skill_agent
        .skill_packages
        .extend(allowed_list.iter().cloned());
    let skills = crate::bootstrap::tools::agent_skill_registry(
        workspace,
        &skill_agent,
        memory_config.enabled,
    );
    let skill_plugin = crate::adapters::outbound::tools::skill::SkillPlugin::from_registry(&skills);
    if let Err(e) = registry
        .register_plugin(&skill_plugin, &plugin_ctx, &allowed_list)
        .await
    {
        warn!(error = %e, "bridge: skill plugin failed — shell skills unavailable");
    }

    // `McpPlugin` only for the `[[mcp_servers]]` the Claude Code engine
    // passed in; a standalone bridge registered in someone's own Claude Code
    // gets none, so their MCP manifest isn't re-advertised.

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
            grant_workspace: false,
            summary_file: None,
            transcript_file: None,
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
        call_with(exec, id, tool, args, None).await
    }

    /// [`call`] with the run's transcript file.
    async fn call_with(
        exec: &dyn ToolExecutor,
        id: Value,
        tool: &str,
        args: Value,
        transcript: Option<&Path>,
    ) -> Value {
        let resp = handle_tools_call(
            id,
            &json!({"name": tool, "arguments": args}),
            exec,
            MAX_MCP_RESULT_CHARS,
            &SecretRegistry::new(),
            transcript,
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

        s.grant_workspace = true;
        let exec = build_bridge_executor(&s, &no_secrets()).await.unwrap();
        assert_eq!(exec.scopes["http_request"].fs_roots, [ws.path()]);
    }

    /// Review #14: a bridge over a sandbox bound to W1 registers no tool
    /// the generation refuses (`hl_ctx`: an opt-in tool no fixture
    /// capability binds) — with the configured agent, and with the
    /// default-`main` fallback (`TENGU_BRIDGE_AGENT` unset), which keeps the
    /// config's generation.
    #[tokio::test]
    async fn the_bridge_registers_no_tool_outside_the_generation() {
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/lineage/sandboxes/w1/config.toml");
        let ws = TempDir::new().unwrap();
        for agent in [Some("architect"), None] {
            let s = setup(
                ws.path(),
                &["read_file", "hl_ctx"],
                Some(Config::load(&file).unwrap()),
                agent,
            );
            assert_eq!(
                bridge_tools(&s)
                    .unwrap()
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["read_file"],
                "{agent:?}"
            );
            let exec = build_bridge_executor(&s, &no_secrets()).await.unwrap();
            let names = exec.registry.tool_names();
            assert!(
                names.iter().any(|n| n == "read_file"),
                "{agent:?}: {names:?}"
            );
            assert!(!names.iter().any(|n| n == "hl_ctx"), "{agent:?}: {names:?}");
            let gen = exec
                .agent_config
                .as_ref()
                .and_then(|a| a.sandbox.generation.as_ref().map(|g| g.id.clone()));
            assert_eq!(gen.as_deref(), Some("W1"), "{agent:?}");
        }
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
                    None,
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

    /// The JSON-RPC request id, behind this process's nonce, is the tool's
    /// `ToolCtx.call_id`.
    #[tokio::test]
    async fn request_id_is_the_call_id() {
        let ws = TempDir::new().unwrap();
        let s = setup(ws.path(), &["read_file"], None, None);
        let exec = sanitized(
            with_probe(build_bridge_executor(&s, &no_secrets()).await.unwrap()),
            &no_secrets(),
        );
        let nonce = call_nonce();
        assert!(
            nonce.len() == 32 && nonce.chars().all(|c| c.is_ascii_hexdigit()),
            "{nonce}"
        );
        assert_eq!(call_nonce(), nonce, "one nonce per process");
        let seen = |r: Value| -> Value { serde_json::from_str(text(&r)).unwrap() };
        let n = seen(call(exec.as_ref(), json!(42), "probe", json!({})).await);
        assert_eq!(n["call_id"], format!("mcp:{nonce}:42"));
        let s = seen(call(exec.as_ref(), json!("req-7"), "probe", json!({})).await);
        assert_eq!(s["call_id"], format!("mcp:{nonce}:req-7"));
        let none = seen(call(exec.as_ref(), Value::Null, "probe", json!({})).await);
        assert!(none["call_id"].is_null());
        assert_eq!(call_id(&json!("")), "", "an empty id is no id");
    }

    /// The fixture shell skill (`tests/fixtures/skills/matrix_cat`).
    const MATRIX_CAT: &str = include_str!("../../../tests/fixtures/skills/matrix_cat/SKILL.md");

    /// A shell skill in the agent's `skill_packages` runs through the bridge
    /// like in-process; an agent that runs no shell (signer / `[risk]`)
    /// loads none.
    #[tokio::test]
    async fn shell_skills_run_through_the_bridge_unless_the_agent_runs_no_shell() {
        let ws = TempDir::new().unwrap();
        let skill = ws.path().join("skills/matrix_cat");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(skill.join("SKILL.md"), MATRIX_CAT).unwrap();
        std::fs::write(ws.path().join("note.txt"), "skill note\n").unwrap();
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(
            &file,
            "[agents.main]\ndefault = true\nengine = \"openrouter\"\nmodel = \"m\"\nskill_packages = [\"matrix_cat\"]\n",
        )
        .unwrap();
        let config = Config::load(&file).unwrap();

        let s = setup(
            ws.path(),
            &["matrix_cat"],
            Some(config.clone()),
            Some("main"),
        );
        let exec = sanitized(
            build_bridge_executor(&s, &no_secrets()).await.unwrap(),
            &no_secrets(),
        );
        let r = call(
            exec.as_ref(),
            json!(1),
            "matrix_cat",
            json!({"path": "note.txt"}),
        )
        .await;
        assert_eq!(r["isError"], false, "{r}");
        assert!(text(&r).contains("skill note"), "{r}");

        // A composed plan step's skill reaches the bridge only through the
        // requested tool list: it loads without `skill_packages` too; an
        // unrequested one does not.
        let mut composed = config.clone();
        composed
            .agents
            .get_mut("main")
            .unwrap()
            .skill_packages
            .clear();
        let s = setup(
            ws.path(),
            &["matrix_cat"],
            Some(composed.clone()),
            Some("main"),
        );
        let exec = build_bridge_executor(&s, &no_secrets()).await.unwrap();
        assert!(exec.registry.get("matrix_cat").is_some());
        let s = setup(ws.path(), &["read_file"], Some(composed), Some("main"));
        let exec = build_bridge_executor(&s, &no_secrets()).await.unwrap();
        assert!(exec.registry.get("matrix_cat").is_none());

        let mut signing = config;
        signing.solana.signer_key_file = Some("/keys/signer.json".into());
        signing.fold_default_scopes();
        let s = setup(ws.path(), &["matrix_cat"], Some(signing), Some("main"));
        let exec = sanitized(
            build_bridge_executor(&s, &no_secrets()).await.unwrap(),
            &no_secrets(),
        );
        let r = call(
            exec.as_ref(),
            json!(2),
            "matrix_cat",
            json!({"path": "note.txt"}),
        )
        .await;
        assert_eq!(r["isError"], true, "{r}");
        assert!(text(&r).contains("not available to this agent"), "{r}");
    }

    /// `compress_and_store`: a `run-agent` step's bridge writes the summary
    /// to the step's file (the last call wins) and says the step is over;
    /// any other bridge refuses it with the reason; other tools pass through.
    #[tokio::test]
    async fn compress_and_store_writes_the_step_summary_or_says_why_not() {
        let ws = TempDir::new().unwrap();
        std::fs::write(ws.path().join("a.txt"), "alpha").unwrap();
        let summary = ws.path().join("summary.txt");
        let s = setup(ws.path(), &["read_file", "compress_and_store"], None, None);
        let inner = sanitized(
            build_bridge_executor(&s, &no_secrets()).await.unwrap(),
            &no_secrets(),
        );
        let step = StepSummary {
            inner: Arc::clone(&inner),
            file: Some(summary.clone()),
        };
        for (id, words) in [(1, "first"), (2, "done: 42")] {
            let r = call(
                &step,
                json!(id),
                "compress_and_store",
                json!({"summary": words}),
            )
            .await;
            assert_eq!(
                (r["isError"].clone(), text(&r)),
                (json!(false), "stored — stop now")
            );
        }
        assert_eq!(std::fs::read_to_string(&summary).unwrap(), "done: 42");
        let r = call(&step, json!(3), "compress_and_store", json!({})).await;
        assert!(
            r["isError"] == true && text(&r).contains("`summary`"),
            "{r}"
        );
        let r = call(&step, json!(4), "read_file", json!({"path": "a.txt"})).await;
        assert!(r["isError"] == false && text(&r).contains("alpha"), "{r}");

        let plain = StepSummary { inner, file: None };
        let r = call(
            &plain,
            json!(5),
            "compress_and_store",
            json!({"summary": "x"}),
        )
        .await;
        assert_eq!(r["isError"], true);
        assert!(text(&r).contains("plain text"), "{r}");
    }

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn tool_call(id: &str, name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments: args,
        }
    }

    fn assistant_calling(calls: Vec<ToolCall>) -> Message {
        Message {
            role: Role::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_calls: Some(calls),
        }
    }

    fn write_transcript(dir: &Path, messages: &[Message]) -> PathBuf {
        let file = dir.join("transcript.json");
        std::fs::write(&file, serde_json::to_vec(messages).unwrap()).unwrap();
        file
    }

    /// The call's conversation: the transcript as read when it already ends
    /// in this call (the engine wrote the stream line), else with the call
    /// appended; no transcript or an unreadable one = none.
    #[test]
    fn call_conversation_ends_in_the_call() {
        let dir = TempDir::new().unwrap();
        let call = tool_call("mcp:n:7", "skill_distill", json!({"from_message_index": 1}));
        assert!(call_conversation(None, &call).is_empty());
        let missing = dir.path().join("nope.json");
        assert!(call_conversation(Some(&missing), &call).is_empty());
        std::fs::write(dir.path().join("bad.json"), "{").unwrap();
        assert!(call_conversation(Some(&dir.path().join("bad.json")), &call).is_empty());

        let base = vec![msg(Role::System, "sys"), msg(Role::User, "goal")];
        let file = write_transcript(dir.path(), &base);
        let conv = call_conversation(Some(&file), &call);
        assert_eq!(conv.len(), 3, "the call appended");
        let last = conv[2].tool_calls.as_ref().unwrap();
        assert_eq!(
            (last[0].id.as_str(), last[0].name.as_str()),
            ("mcp:n:7", "skill_distill")
        );

        // Written by the engine already (its id, a sibling call answered).
        let mut streamed = base.clone();
        streamed.push(assistant_calling(vec![
            tool_call("toolu_a", "read_file", json!({"path": "a"})),
            tool_call("toolu_b", "skill_distill", json!({"from_message_index": 1})),
        ]));
        streamed.push(Message {
            tool_call_id: Some("toolu_a".into()),
            ..msg(Role::Tool, "alpha")
        });
        let file = write_transcript(dir.path(), &streamed);
        let conv = call_conversation(Some(&file), &call);
        assert_eq!(conv.len(), streamed.len(), "as read");

        // The same call answered earlier, or other arguments: appended.
        let mut answered = streamed.clone();
        answered.push(Message {
            tool_call_id: Some("toolu_b".into()),
            ..msg(Role::Tool, "made")
        });
        let file = write_transcript(dir.path(), &answered);
        assert_eq!(
            call_conversation(Some(&file), &call).len(),
            answered.len() + 1
        );
        let other = tool_call("mcp:n:8", "skill_distill", json!({"from_message_index": 0}));
        let file = write_transcript(dir.path(), &streamed);
        assert_eq!(
            call_conversation(Some(&file), &other).len(),
            streamed.len() + 1
        );
    }

    /// `skill_distill` through the bridge seeds fixtures from the run's
    /// transcript, `from_message_index ≥ 1` included — as in-process; with
    /// no transcript it refuses (never a silent `fixtures: []`).
    #[tokio::test]
    async fn skill_distill_reads_the_runs_transcript() {
        let ws = TempDir::new().unwrap();
        let s = setup(ws.path(), &["skill_distill"], None, None);
        let exec = sanitized(
            build_bridge_executor(&s, &no_secrets()).await.unwrap(),
            &no_secrets(),
        );
        let args = |name: &str| {
            json!({"name": name, "description": "From the bridge.",
                   "body_markdown": "# x\n\nBody.\n", "metrics": [],
                   "from_message_index": 1, "tier": "workspace"})
        };
        let transcript = write_transcript(
            ws.path(),
            &[
                msg(Role::System, "sys"),
                msg(Role::User, "list the files"),
                assistant_calling(vec![tool_call(
                    "toolu_1",
                    "list_directory",
                    json!({"path": "."}),
                )]),
                Message {
                    tool_call_id: Some("toolu_1".into()),
                    ..msg(Role::Tool, "a.txt")
                },
                msg(Role::User, "save this as a skill"),
            ],
        );
        let r = call_with(
            exec.as_ref(),
            json!(1),
            "skill_distill",
            args("from-bridge"),
            Some(&transcript),
        )
        .await;
        assert_eq!(r["isError"], false, "{r}");
        let out: Value = serde_json::from_str(text(&r)).unwrap();
        assert_eq!(out["fixtures_created"], 2, "{out}");
        let prompts = std::fs::read_to_string(
            ws.path()
                .join(".tengu/skills/from-bridge/evals/prompts.yaml"),
        )
        .unwrap();
        assert!(
            prompts.contains("list the files") && prompts.contains("list_directory"),
            "{prompts}"
        );

        let r = call(
            exec.as_ref(),
            json!(2),
            "skill_distill",
            args("no-transcript"),
        )
        .await;
        assert_eq!(r["isError"], true, "{r}");
        assert!(text(&r).contains("no conversation"), "{r}");
        assert!(!ws.path().join(".tengu/skills/no-transcript").exists());
    }

    /// `TENGU_BRIDGE_MCP_SERVERS` names are taken from the loaded config
    /// (`${VAR}` expanded by this process's `Config::load`), objects as
    /// given; an unknown name or garbage leaves those tools out.
    #[test]
    fn mcp_servers_are_named_and_taken_from_the_config() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(
            &file,
            "[agents.main]\ndefault = true\nengine = \"openrouter\"\nmodel = \"m\"\n\n\
             [[mcp_servers]]\nname = \"fake\"\ntransport = \"stdio\"\ncommand = [\"sh\", \"fake.sh\"]\n\
             env = { TOKEN = \"$FAKE_TOKEN\" }\n",
        )
        .unwrap();
        let config = Config::load(&file).unwrap();
        let servers = resolve_mcp_servers(r#"["fake", "ghost"]"#, Some(&config));
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].command, ["sh", "fake.sh"]);
        assert_eq!(servers[0].env["TOKEN"], "$FAKE_TOKEN");
        assert!(
            resolve_mcp_servers(r#"["fake"]"#, None).is_empty(),
            "no config"
        );
        let given = resolve_mcp_servers(
            r#"[{"name": "inline", "transport": "stdio", "command": ["true"]}]"#,
            None,
        );
        assert_eq!(given[0].name, "inline");
        assert!(resolve_mcp_servers("not json", Some(&config)).is_empty());
    }

    /// The sandbox file as `Config::load` reads it, and the bridge tool list
    /// a chat turn of `agent` advertises (`agent_base_tools`, memory on).
    fn sandbox_agent(name: &str, agent: &str) -> (Config, Vec<String>) {
        let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("sandboxes/{name}/config.toml"));
        let config = Config::load(&file).unwrap();
        let tools = crate::bootstrap::tools::agent_base_tools(&config.agents[agent], true, true)
            .into_iter()
            .map(|t| t.name)
            .collect();
        (config, tools)
    }

    /// W1-gate review (chat honours `tools`): the claude_code agents of aura
    /// and storage-test get, through the bridge, the tools their skills call
    /// — and their scopes no wider than before.
    ///
    /// | Sandbox | Skill needs | Scope |
    /// |---|---|---|
    /// | aura | `run_command` (aura-orchestrator / molecule-x402: x402 curl flow, Phase 4 node encrypt) | `shell_bins` = the skills' first command words, not the old `"*"` fallback |
    /// | storage-test | `http_request`, `write_file` (telegram-rag-ingest URL / pasted-text ingest) | http: any host, no `$VAR`, no upload; write: the workspace |
    #[tokio::test]
    async fn sandbox_skills_get_their_tools_through_the_bridge() {
        // aura: run_command joins the listed tools; signing stays off the list.
        let (aura, tools) = sandbox_agent("aura", "aura");
        let mut sorted = tools.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            [
                "http_request",
                "list_directory",
                "read_file",
                "run_command",
                "shared_cache"
            ]
        );
        let ws = TempDir::new().unwrap();
        let names: Vec<&str> = tools.iter().map(String::as_str).collect();
        let exec = build_bridge_executor(
            &setup(ws.path(), &names, Some(aura), Some("aura")),
            &no_secrets(),
        )
        .await
        .unwrap();
        let registered = exec.registry.tool_names();
        for tool in &tools {
            assert!(
                registered.contains(tool),
                "{tool} not served: {registered:?}"
            );
        }
        for absent in ["write_file", "sign_and_send_transaction", "memory_ingest"] {
            assert!(!registered.iter().any(|t| t == absent), "{absent}");
        }

        // Every command line the skills give run_command passes the scope;
        // python / pip / pdftotext / a shell do not.
        let shell = &exec.scopes["run_command"];
        let gate = |c: &str| shell.check_shell_bin(crate::domain::scope::shell_command_binary(c));
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut commands = vec!["date -u +%Y-%m-%dT%H:%M:%SZ".to_string()];
        for skill in ["aura-orchestrator", "molecule-x402"] {
            let body =
                std::fs::read_to_string(root.join(format!("skills/{skill}/SKILL.md"))).unwrap();
            commands.extend(
                body.lines()
                    .filter_map(|l| l.trim_start().strip_prefix("command: "))
                    .map(str::to_string),
            );
        }
        assert!(commands.len() >= 15, "{commands:?}");
        for command in &commands {
            assert!(gate(command).is_ok(), "{command}: {:?}", gate(command));
        }
        for command in [
            "python3 -c 'print(1)'",
            "pip install pdfplumber",
            "pdftotext paper.pdf -",
            "DEK=x bash -c 'node -e 1'",
        ] {
            assert!(gate(command).is_err(), "{command}");
        }
        let exec = sanitized(exec, &no_secrets());
        let r = call(
            exec.as_ref(),
            json!(1),
            "run_command",
            json!({"command": "MSG='bridged run' echo ok"}),
        )
        .await;
        assert!(r["isError"] == false && text(&r).contains("ok"), "{r}");
        let r = call(
            exec.as_ref(),
            json!(2),
            "run_command",
            json!({"command": "python3 -c 'print(1)'"}),
        )
        .await;
        assert!(
            r["isError"] == true && text(&r).contains("binary 'python3' not in allowed shell_bins"),
            "{r}"
        );

        // storage-test: the ingest path's http_request and write_file.
        let (storage, tools) = sandbox_agent("storage-test", "storage");
        let mut sorted = tools.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            [
                "http_request",
                "persistent_store",
                "read_file",
                "write_file"
            ]
        );
        // persistent_store left out here: serving it builds the memory store.
        let exec = build_bridge_executor(
            &setup(
                ws.path(),
                &["http_request", "write_file", "read_file"],
                Some(storage),
                Some("storage"),
            ),
            &no_secrets(),
        )
        .await
        .unwrap();
        let registered = exec.registry.tool_names();
        for tool in ["http_request", "write_file", "read_file"] {
            assert!(registered.iter().any(|t| t == tool), "{tool}");
        }
        let http = &exec.scopes["http_request"];
        assert!(http.check_net_host("example.org").is_ok());
        assert!(
            http.check_env_read("OPENROUTER_API_KEY").is_err(),
            "no $VAR"
        );
        assert!(http.check_fs_read(ws.path()).is_err(), "no upload");
        let home_ws =
            crate::config::paths::expand_tilde(std::path::Path::new("~/storage-test-workspace"));
        assert_eq!(exec.scopes["write_file"].fs_roots, [home_ws]);
        assert!(exec.scopes["write_file"].shell_bins.is_empty());
    }
}
