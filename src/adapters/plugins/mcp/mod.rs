// src/adapters/plugins/mcp/mod.rs
//! MCP plugin — inbound MCP client.
//!
//! Task A10 of Phase A. Complements `mcp_bridge.rs` (OUTBOUND server), which
//! exposes tengu's own tools to external Claude Code clients. This plugin is
//! the opposite direction: tengu CONNECTS to external MCP servers, calls
//! `tools/list`, and surfaces every remote tool as `{server_name}.{tool_name}`
//! so an LLM can call it like any other platform tool.
//!
//! Only wired in by `channel_runtime::build_tool_executor` when the root
//! `Config.mcp_servers` list is non-empty — zero cost for users who have not
//! configured any MCP integrations.
//!
//! Failure semantics: a bad MCP server config (unreachable, broken handshake,
//! non-JSON output) logs a warning and is skipped. We never let a misbehaving
//! external server prevent tengu from booting.

#![allow(dead_code)]

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use crate::adapters::config::McpServerConfig;
use crate::domain::message::ToolDef;
use crate::ports::tool::{PluginCtx, Tool, ToolPlugin};

pub(crate) mod client;
pub(crate) mod protocol;
pub(crate) mod proxy_tool;

use client::{McpCaller, McpClient};
use proxy_tool::McpProxyTool;

/// Plugin grouping the inbound-MCP proxy tools. One tool is registered per
/// remote tool discovered across all configured servers.
pub(crate) struct McpPlugin {
    servers: Vec<McpServerConfig>,
}

impl McpPlugin {
    pub(crate) fn new(servers: Vec<McpServerConfig>) -> Self {
        Self { servers }
    }
}

#[async_trait]
impl ToolPlugin for McpPlugin {
    fn name(&self) -> &'static str {
        "mcp"
    }

    async fn tools(&self, _ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        let mut out: Vec<Arc<dyn Tool>> = Vec::new();
        for cfg in &self.servers {
            let client = match McpClient::connect(cfg).await {
                Ok(c) => Arc::new(c) as Arc<dyn McpCaller>,
                Err(e) => {
                    tracing::warn!(
                        server = %cfg.name,
                        error = %e,
                        "mcp: server connection failed, skipping"
                    );
                    continue;
                }
            };

            let manifest = match client.list_tools().await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(
                        server = %cfg.name,
                        error = %e,
                        "mcp: server tools/list failed, skipping"
                    );
                    continue;
                }
            };

            for remote in manifest {
                let qualified = format!("{}.{}", cfg.name, remote.name);
                let def =
                    ToolDef::new(&qualified, &remote.description, remote.input_schema.clone());
                out.push(Arc::new(McpProxyTool {
                    def,
                    remote_name: remote.name,
                    client: Arc::clone(&client),
                }));
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::config::Config;
    use crate::adapters::secret_builder::SecretRegistry;
    use crate::adapters::shell_executor::LocalShellExecutor;
    use tempfile::TempDir;

    fn plugin_ctx<'a>(
        workspace: &'a std::path::Path,
        agent: &'a crate::adapters::config::AgentConfig,
    ) -> PluginCtx<'a> {
        PluginCtx {
            workspace,
            config: agent,
            http: reqwest::Client::new(),
            shell: Arc::new(LocalShellExecutor::new()),
            memory_manager: None,
            secret_registry: Arc::new(SecretRegistry::new()),
        }
    }

    #[test]
    fn mcp_config_deserializes() {
        let toml_str = r#"
            runtime_profile = "auto"

            [agents.main]
            default = true
            engine = "openrouter"
            model = "anthropic/claude-sonnet-4.6"

            [[mcp_servers]]
            name = "github"
            transport = "http"
            url = "https://example.com/mcp"

            [mcp_servers.auth]
            type = "bearer"
            token = "$GITHUB_TOKEN"

            [[mcp_servers]]
            name = "local"
            transport = "stdio"
            command = ["node", "mcp-server.js"]

            [mcp_servers.env]
            DEBUG = "1"
        "#;
        let config: Config = toml::from_str(toml_str).expect("should parse");
        assert_eq!(config.mcp_servers.len(), 2);

        let github = &config.mcp_servers[0];
        assert_eq!(github.name, "github");
        assert_eq!(github.transport, "http");
        assert_eq!(github.url.as_deref(), Some("https://example.com/mcp"));
        let auth = github.auth.as_ref().expect("auth present");
        assert_eq!(auth.auth_type, "bearer");
        assert_eq!(auth.token, "$GITHUB_TOKEN");

        let local = &config.mcp_servers[1];
        assert_eq!(local.name, "local");
        assert_eq!(local.transport, "stdio");
        assert_eq!(local.command, vec!["node", "mcp-server.js"]);
        assert_eq!(local.env.get("DEBUG").map(String::as_str), Some("1"));
    }

    #[tokio::test]
    async fn tool_defs_empty_when_no_mcp_servers() {
        let tmp = TempDir::new().unwrap();
        let config = Config::default();
        let agent = config.agents.get("main").unwrap().clone();
        let ctx = plugin_ctx(tmp.path(), &agent);

        let plugin = McpPlugin::new(Vec::new());
        let tools = plugin.tools(&ctx).await.unwrap();
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn mcp_plugin_skips_failing_servers() {
        let tmp = TempDir::new().unwrap();
        let config = Config::default();
        let agent = config.agents.get("main").unwrap().clone();
        let ctx = plugin_ctx(tmp.path(), &agent);

        // A stdio server pointing at a nonexistent binary. `McpClient::connect`
        // will fail at spawn; the plugin must warn and return an empty vec.
        let bad_server = McpServerConfig {
            name: "broken".to_string(),
            transport: "stdio".to_string(),
            command: vec!["/nonexistent/binary/that/cannot/possibly/exist/abc123xyz".to_string()],
            url: None,
            env: Default::default(),
            auth: None,
        };
        let plugin = McpPlugin::new(vec![bad_server]);
        let tools = plugin.tools(&ctx).await.unwrap();
        assert!(tools.is_empty(), "bad server must not contribute any tools");
    }
}
