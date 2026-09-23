// src/adapters/plugins/mcp/client.rs
//! Outbound MCP client — stdio and http transports over JSON-RPC 2.0.
//!
//! The `McpCaller` trait is the narrow interface the proxy tool depends on:
//! just "call a named remote tool with args, get a string back". Production
//! code uses `McpClient`, an enum over the two transports; tests can plug in
//! a stub that never touches the network.
//!
//! Stdio transport: line-delimited JSON over the child's stdin/stdout.
//! HTTP transport: POST JSON-RPC to `config.url`, optional bearer auth.
//!
//! The `initialize` handshake is sent minimally (protocolVersion + clientInfo)
//! to maximise server compatibility — many reference MCP servers reject
//! `tools/list` before `initialize`.

#![allow(dead_code)]

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use super::protocol::{JsonRpcRequest, JsonRpcResponse, McpListToolsResult, McpRemoteTool};
use crate::config::{McpAuthConfig, McpServerConfig};

/// Narrow interface: list tools and call them. Kept as a trait so tests can
/// stub network I/O.
#[async_trait]
pub(crate) trait McpCaller: Send + Sync {
    async fn list_tools(&self) -> Result<Vec<McpRemoteTool>>;
    async fn call_tool(&self, name: &str, args: &Value) -> Result<String>;
}

/// The concrete MCP client — one of two transport flavours.
pub(crate) enum McpClient {
    Stdio(StdioClient),
    Http(HttpClient),
}

impl McpClient {
    pub(crate) async fn connect(config: &McpServerConfig) -> Result<Self> {
        match config.transport.as_str() {
            "stdio" => {
                let client = StdioClient::connect(config).await?;
                Ok(McpClient::Stdio(client))
            }
            "http" => {
                let client = HttpClient::new(config)?;
                client.initialize().await?;
                Ok(McpClient::Http(client))
            }
            other => bail!(
                "mcp: unsupported transport '{}' (expected 'stdio' or 'http')",
                other
            ),
        }
    }
}

#[async_trait]
impl McpCaller for McpClient {
    async fn list_tools(&self) -> Result<Vec<McpRemoteTool>> {
        match self {
            McpClient::Stdio(c) => c.list_tools().await,
            McpClient::Http(c) => c.list_tools().await,
        }
    }

    async fn call_tool(&self, name: &str, args: &Value) -> Result<String> {
        match self {
            McpClient::Stdio(c) => c.call_tool(name, args).await,
            McpClient::Http(c) => c.call_tool(name, args).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Stdio transport
// ---------------------------------------------------------------------------

/// Stdio MCP client — line-delimited JSON-RPC over a subprocess's stdio.
pub(crate) struct StdioClient {
    inner: Arc<Mutex<StdioInner>>,
    next_id: AtomicU64,
}

struct StdioInner {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Hold the child so it doesn't get reaped while the client is alive.
    _child: tokio::process::Child,
}

impl StdioClient {
    async fn connect(config: &McpServerConfig) -> Result<Self> {
        if config.command.is_empty() {
            bail!(
                "mcp: stdio server '{}' requires a non-empty 'command'",
                config.name
            );
        }
        let mut cmd = Command::new(&config.command[0]);
        if config.command.len() > 1 {
            cmd.args(&config.command[1..]);
        }
        // Inherit parent env, add `[egress]` proxy vars (advisory — the
        // server is a separate program), then overlay + expand $VAR refs.
        for (k, v) in crate::adapters::outbound::egress::policy().proxy_env() {
            cmd.env(k, v);
        }
        for (k, v) in &config.env {
            let resolved = expand_dollar_var(v)?;
            cmd.env(k, resolved);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "mcp: failed to spawn stdio server '{}' ({:?})",
                config.name, config.command
            )
        })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("mcp: child stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("mcp: child stdout unavailable"))?;

        let client = Self {
            inner: Arc::new(Mutex::new(StdioInner {
                stdin,
                stdout: BufReader::new(stdout),
                _child: child,
            })),
            next_id: AtomicU64::new(1),
        };
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&self) -> Result<()> {
        // Minimal MCP initialize handshake. Many reference servers reject
        // subsequent requests (including tools/list) until this succeeds.
        let params = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "tengu",
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        let _ = self.rpc_call("initialize", params).await?;
        // Notification — no id, no response expected.
        let _ = self
            .rpc_notify("notifications/initialized", json!({}))
            .await;
        Ok(())
    }

    async fn rpc_call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');

        let mut inner = self.inner.lock().await;
        inner
            .stdin
            .write_all(line.as_bytes())
            .await
            .with_context(|| format!("mcp: failed to send '{}' request", method))?;
        inner.stdin.flush().await.ok();

        // Read one line; drop anything that's not a response to our id (MCP
        // servers may interleave notifications like `tools/list_changed`).
        loop {
            let mut buf = String::new();
            let n = inner
                .stdout
                .read_line(&mut buf)
                .await
                .with_context(|| format!("mcp: failed to read response for '{}'", method))?;
            if n == 0 {
                bail!("mcp: stdio server closed before responding to '{}'", method);
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Try to parse as a response. If it has no id, treat as a
            // notification and keep reading.
            let parsed: serde_json::Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(line = %trimmed, error = %e, "mcp: unparseable stdio line");
                    continue;
                }
            };
            if parsed.get("id").is_none() {
                continue; // notification
            }
            let resp: JsonRpcResponse = serde_json::from_value(parsed)?;
            if resp.id != id {
                continue;
            }
            if let Some(err) = resp.error {
                bail!("mcp: server error {}: {}", err.code, err.message);
            }
            return Ok(resp.result.unwrap_or(Value::Null));
        }
    }

    async fn rpc_notify(&self, method: &str, params: Value) -> Result<()> {
        // Notifications have no id. Some servers are strict about this.
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&body)?;
        line.push('\n');

        let mut inner = self.inner.lock().await;
        inner.stdin.write_all(line.as_bytes()).await?;
        inner.stdin.flush().await.ok();
        Ok(())
    }

    async fn list_tools(&self) -> Result<Vec<McpRemoteTool>> {
        let raw = self.rpc_call("tools/list", json!({})).await?;
        let parsed: McpListToolsResult = serde_json::from_value(raw)
            .context("mcp: tools/list response did not match expected shape")?;
        Ok(parsed.tools)
    }

    async fn call_tool(&self, name: &str, args: &Value) -> Result<String> {
        let params = json!({ "name": name, "arguments": args });
        let raw = self.rpc_call("tools/call", params).await?;
        Ok(stringify_call_result(raw))
    }
}

// ---------------------------------------------------------------------------
// HTTP transport
// ---------------------------------------------------------------------------

/// HTTP MCP client — JSON-RPC 2.0 over POST.
pub(crate) struct HttpClient {
    http: reqwest::Client,
    url: String,
    auth_header: Option<(String, String)>, // (name, value)
    next_id: AtomicU64,
}

impl HttpClient {
    fn new(config: &McpServerConfig) -> Result<Self> {
        let url = config
            .url
            .clone()
            .ok_or_else(|| anyhow!("mcp: http server '{}' requires 'url'", config.name))?;

        let auth_header = match &config.auth {
            None => None,
            Some(McpAuthConfig { auth_type, token }) => {
                if auth_type != "bearer" {
                    bail!(
                        "mcp: unsupported auth type '{}' (only 'bearer' is supported)",
                        auth_type
                    );
                }
                let resolved = expand_dollar_var(token)?;
                Some(("Authorization".to_string(), format!("Bearer {}", resolved)))
            }
        };

        Ok(Self {
            // Proxied per `[egress]`; loopback servers stay direct.
            http: crate::adapters::outbound::egress::policy().mcp_client(
                reqwest::Client::builder().timeout(std::time::Duration::from_secs(60)),
            )?,
            url,
            auth_header,
            next_id: AtomicU64::new(1),
        })
    }

    async fn initialize(&self) -> Result<()> {
        let params = json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "tengu",
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        let _ = self.rpc_call("initialize", params).await?;
        Ok(())
    }

    async fn rpc_call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method,
            params,
        };

        let mut req = self.http.post(&self.url).json(&body);
        if let Some((k, v)) = &self.auth_header {
            req = req.header(k, v);
        }
        let response = req
            .send()
            .await
            .with_context(|| format!("mcp: http '{}' request failed", method))?;
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            bail!(
                "mcp: http '{}' returned HTTP {}: {}",
                method,
                status.as_u16(),
                text
            );
        }
        let resp: JsonRpcResponse = response
            .json()
            .await
            .with_context(|| format!("mcp: failed to parse '{}' response", method))?;
        if let Some(err) = resp.error {
            bail!("mcp: server error {}: {}", err.code, err.message);
        }
        Ok(resp.result.unwrap_or(Value::Null))
    }

    async fn list_tools(&self) -> Result<Vec<McpRemoteTool>> {
        let raw = self.rpc_call("tools/list", json!({})).await?;
        let parsed: McpListToolsResult = serde_json::from_value(raw)
            .context("mcp: tools/list response did not match expected shape")?;
        Ok(parsed.tools)
    }

    async fn call_tool(&self, name: &str, args: &Value) -> Result<String> {
        let params = json!({ "name": name, "arguments": args });
        let raw = self.rpc_call("tools/call", params).await?;
        Ok(stringify_call_result(raw))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a `tools/call` result value into a plain string.
///
/// MCP servers return `{ "content": [{"type": "text", "text": "..."}, ...] }`.
/// We concatenate every text part; for non-text content we serialize the raw
/// JSON so the LLM at least sees the structure.
fn stringify_call_result(raw: Value) -> String {
    if let Some(content) = raw.get("content").and_then(|v| v.as_array()) {
        let mut out = String::new();
        for part in content {
            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            } else {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&part.to_string());
            }
        }
        if raw.get("isError").and_then(|v| v.as_bool()) == Some(true) {
            return format!("error: {}", out);
        }
        return out;
    }
    raw.to_string()
}

/// Resolve a `$VAR` reference (or pass through a literal). Matches the pattern
/// used by the http plugin's auth fields, but without scope gating — MCP
/// server credentials come from the config file, not from per-call tool args.
fn expand_dollar_var(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if let Some(name) = trimmed.strip_prefix('$') {
        if !name.is_empty() {
            return std::env::var(name).map_err(|_| {
                anyhow!(
                    "mcp: environment variable '{}' referenced but not set",
                    name
                )
            });
        }
    }
    Ok(input.to_string())
}
