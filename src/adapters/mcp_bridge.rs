//! Stdio MCP bridge — exposes Tengu tools to Claude Code via the MCP protocol.
//!
//! Runs as `tengu mcp-bridge` subprocess. Claude CLI spawns this as an MCP stdio
//! server and routes tool calls through it. The bridge creates its own tool
//! executors (same env vars / filesystem as the parent Tengu process).
//!
//! Protocol: JSON-RPC 2.0 over stdin/stdout (newline-delimited).

use std::collections::HashSet;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::adapters::cache_tool_executor::{CacheToolExecutionAdapter, SHARED_CACHE_TOOL_NAME};
use crate::adapters::composite_tool_executor::CompositeToolExecutionAdapter;
use crate::adapters::crypto_tool_executor::CryptoToolExecutionAdapter;
use crate::adapters::http_tool_executor::HttpToolExecutionAdapter;
use crate::adapters::ports::ToolExecutionPort;
use crate::adapters::shell_executor::LocalShellExecutor;
use crate::adapters::tool_builder::{build_tool_activity_text, WorkspaceToolExecutionAdapter};
use crate::adapters::types::{ToolCall, ToolDef};

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
// Bridge entry point
// ---------------------------------------------------------------------------

/// Run the MCP bridge stdio server. Blocks until stdin closes.
pub fn run_mcp_bridge() -> Result<()> {
    let workspace = std::env::var("TENGU_BRIDGE_WORKSPACE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());

    let tools: Vec<ToolDef> = match std::env::var("TENGU_BRIDGE_TOOLS") {
        Ok(json) => serde_json::from_str(&json)
            .map_err(|e| anyhow::anyhow!("Failed to parse TENGU_BRIDGE_TOOLS: {}", e))?,
        Err(_) => vec![],
    };

    let executor = build_bridge_executor(&workspace, &tools);
    let mcp_tools: Vec<McpToolDef> = tools.iter().map(McpToolDef::from).collect();

    info!(
        workspace = %workspace.display(),
        tool_count = tools.len(),
        "MCP bridge started"
    );

    let stdin = io::stdin();
    let stdout = io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) if l.trim().is_empty() => continue,
            Ok(l) => l,
            Err(_) => break,
        };

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "MCP bridge received invalid JSON");
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
            "initialize" => handle_initialize(id),
            "notifications/initialized" => continue, // notification, no response
            "tools/list" => handle_tools_list(id, &mcp_tools),
            "tools/call" => handle_tools_call(id, &request.params, executor.as_ref()),
            "ping" => JsonRpcResponse::success(id, serde_json::json!({})),
            _ => JsonRpcResponse::error(id, -32601, format!("Method not found: {}", request.method)),
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

fn handle_initialize(id: serde_json::Value) -> JsonRpcResponse {
    JsonRpcResponse::success(
        id,
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "tengu-tools",
                "version": env!("CARGO_PKG_VERSION")
            }
        }),
    )
}

fn handle_tools_list(id: serde_json::Value, tools: &[McpToolDef]) -> JsonRpcResponse {
    JsonRpcResponse::success(id, serde_json::json!({ "tools": tools }))
}

fn handle_tools_call(
    id: serde_json::Value,
    params: &serde_json::Value,
    executor: &dyn ToolExecutionPort,
) -> JsonRpcResponse {
    let tool_name = params
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
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

    match executor.execute_tool(&call) {
        Ok(result) => {
            debug!(
                tool = %call.name,
                elapsed_ms = started_at.elapsed().as_millis(),
                result_len = result.len(),
                "MCP tool call completed"
            );
            JsonRpcResponse::success(
                id,
                serde_json::json!({
                    "content": [{ "type": "text", "text": result }],
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
// Tool executor construction (mirrors channel_runtime::build_tool_executor)
// ---------------------------------------------------------------------------

fn build_bridge_executor(workspace: &std::path::Path, tools: &[ToolDef]) -> Arc<dyn ToolExecutionPort> {
    let allowed_names: HashSet<&str> = tools.iter().map(|t| t.name.as_str()).collect();

    let shell: Arc<dyn crate::adapters::ports::ShellExecutionPort> =
        Arc::new(LocalShellExecutor::new());

    let workspace_exec = Arc::new(
        WorkspaceToolExecutionAdapter::new(workspace.to_path_buf()).with_shell(Arc::clone(&shell)),
    );

    let mut composite = CompositeToolExecutionAdapter::new(workspace_exec);

    // HTTP
    if allowed_names.contains("http_request") {
        if let Ok(http_exec) = HttpToolExecutionAdapter::with_client(None, workspace.to_path_buf()) {
            composite = composite.with_executor(
                Arc::new(http_exec),
                HashSet::from(["http_request".to_string()]),
            );
        }
    }

    // Crypto
    let crypto_tool_names: HashSet<String> = [
        "sign_and_send_transaction",
        "sign_message",
        "get_wallet_address",
        "abi_encode",
        "hex_to_uint256",
    ]
    .iter()
    .filter(|n| allowed_names.contains(**n))
    .map(|n| n.to_string())
    .collect();

    if !crypto_tool_names.is_empty() {
        if let Ok(crypto_exec) = CryptoToolExecutionAdapter::with_client(None) {
            composite = composite.with_executor(Arc::new(crypto_exec), crypto_tool_names);
        }
    }

    // Shared cache
    if allowed_names.contains(SHARED_CACHE_TOOL_NAME) {
        if let Ok(cache_exec) = CacheToolExecutionAdapter::open(workspace) {
            composite = composite.with_executor(
                Arc::new(cache_exec),
                HashSet::from([SHARED_CACHE_TOOL_NAME.to_string()]),
            );
        }
    }

    Arc::new(composite)
}
