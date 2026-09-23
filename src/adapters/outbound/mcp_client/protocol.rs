// src/adapters/outbound/mcp_client/protocol.rs
//! JSON-RPC 2.0 wire types plus the MCP-specific `tools/list` response shapes.
//!
//! These are the bare minimum needed to drive an outbound MCP client. They are
//! intentionally kept local to the `mcp` plugin — `mcp_bridge.rs` (the inbound
//! server) has its own, slightly different types (uses `id: Option<Value>`
//! because servers must echo any id the client sends; clients always allocate
//! a `u64`).

#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Debug)]
pub(crate) struct JsonRpcRequest<'a> {
    pub jsonrpc: &'static str,
    pub id: u64,
    pub method: &'a str,
    pub params: Value,
}

#[derive(Deserialize, Debug)]
pub(crate) struct JsonRpcResponse {
    #[allow(dead_code)]
    pub jsonrpc: String,
    #[allow(dead_code)]
    pub id: u64,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<JsonRpcError>,
}

#[derive(Deserialize, Debug)]
pub(crate) struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub data: Option<Value>,
}

// ---------------------------------------------------------------------------
// MCP-specific: tools/list response
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug, Clone)]
pub(crate) struct McpRemoteTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

#[derive(Deserialize, Debug)]
pub(crate) struct McpListToolsResult {
    pub tools: Vec<McpRemoteTool>,
}
