// src/adapters/outbound/mcp_client/mod.rs
//! MCP plugin — inbound MCP client.
//!
//! Task A10 of Phase A. Complements `mcp_bridge.rs` (OUTBOUND server), which
//! exposes tengu's own tools to external Claude Code clients. This plugin is
//! the opposite direction: tengu CONNECTS to external MCP servers, calls
//! `tools/list`, and surfaces every remote tool as `{server_name}__{tool_name}`
//! so an LLM can call it like any other platform tool.
//!
//! Only wired in by `crate::bootstrap::tools::build_tool_executor` when the root
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

use crate::config::McpServerConfig;
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
                let qualified = qualified_tool_name(&cfg.name, &remote.name);
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

/// Enumerate tools from every configured external MCP server for the planner registry.
///
/// For each `McpServerConfig`: dial the server (stdio or HTTP), call
/// `tools/list`, and flatten each remote tool into a `ToolDef` named
/// `{server}__{tool}` — the same qualifier the runtime `McpProxyTool` uses,
/// so the registry name and the tool-call name stay in lockstep.
///
/// Requires a live connection (`McpServerConfig` carries no static tool
/// list). Fail-soft per server: an unreachable server logs a warning and is
/// skipped. Returns an empty `Vec` when `servers` is empty.
pub(crate) async fn enumerate_tools(servers: &[McpServerConfig]) -> Vec<ToolDef> {
    use client::McpCaller;

    let mut out: Vec<ToolDef> = Vec::new();
    for cfg in servers {
        let client = match McpClient::connect(cfg).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    server = %cfg.name,
                    error = %e,
                    "planner registry: MCP server connect failed; skipping (other servers continue)"
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
                    "planner registry: MCP tools/list failed; skipping (other servers continue)"
                );
                continue;
            }
        };
        for remote in manifest {
            out.push(ToolDef {
                name: qualified_tool_name(&cfg.name, &remote.name),
                description: remote.description,
                parameters: remote.input_schema,
            });
        }
    }
    out
}

/// Separator between server and tool name. Provider function names allow only
/// `[a-zA-Z0-9_-]`, so a `.` would be rejected by the model API.
pub(crate) const MCP_NAME_SEPARATOR: &str = "__";

/// Name an external MCP tool is exposed under: `{server}__{tool}`.
pub(crate) fn qualified_tool_name(server: &str, tool: &str) -> String {
    format!("{server}{MCP_NAME_SEPARATOR}{tool}")
}

/// Environment variables a server config reads through `$VAR` values
/// (`env` entries and the http bearer token). A child process that
/// reconnects to the server (the MCP bridge) needs them forwarded.
pub(crate) fn referenced_env_vars(cfg: &McpServerConfig) -> Vec<String> {
    cfg.env
        .values()
        .chain(cfg.auth.as_ref().map(|a| &a.token))
        .filter_map(|v| v.trim().strip_prefix('$'))
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether `tool_name` belongs to the `[[mcp_servers]]` entry named `server`.
pub(crate) fn is_server_tool(server: &str, tool_name: &str) -> bool {
    tool_name
        .strip_prefix(server)
        .is_some_and(|rest| rest.starts_with(MCP_NAME_SEPARATOR))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::adapters::outbound::shell::LocalShellExecutor;
    use crate::config::Config;
    use crate::domain::secrets::SecretRegistry;
    use tempfile::TempDir;

    fn plugin_ctx<'a>(
        workspace: &'a std::path::Path,
        agent: &'a crate::config::AgentConfig,
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

    /// `[[mcp_servers]]` entry for `tests/fixtures/fake_mcp_server.sh`
    /// (one tool, `echo`, answering "pong").
    pub(crate) fn fake_server(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: "stdio".to_string(),
            command: vec![
                "sh".to_string(),
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/fake_mcp_server.sh"
                )
                .to_string(),
            ],
            url: None,
            env: Default::default(),
            auth: None,
        }
    }

    #[tokio::test]
    async fn stdio_server_tools_are_qualified_and_callable() {
        let tmp = TempDir::new().unwrap();
        let config = Config::default();
        let agent = config.agents.get("main").unwrap().clone();
        let ctx = plugin_ctx(tmp.path(), &agent);

        let tools = McpPlugin::new(vec![fake_server("fake")])
            .tools(&ctx)
            .await
            .unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.definition().name.as_str()).collect();
        assert_eq!(names, ["fake__echo"]);
        assert!(is_server_tool("fake", "fake__echo"));
        assert!(!is_server_tool("fake", "fakeecho"));
        assert!(!is_server_tool("fak", "fake__echo"));

        let scope = crate::domain::scope::ToolScope::default();
        let shell = LocalShellExecutor::new();
        let http = reqwest::Client::new();
        let secrets = SecretRegistry::new();
        struct NoActivity;
        impl crate::ports::tool_activity::ToolActivityPort for NoActivity {
            fn publish_tool_activity(&self, _call: &crate::domain::message::ToolCall) {}
        }
        let tool_ctx = crate::ports::tool::ToolCtx {
            workspace: tmp.path(),
            scope: &scope,
            shell: &shell,
            http: &http,
            memory_manager: None,
            secret_registry: &secrets,
            activity: &NoActivity,
            conversation: crate::ports::tool::ConversationView::empty(),
            agent_config: None,
        };
        let out = tools[0]
            .execute(&serde_json::json!({}), &tool_ctx)
            .await
            .unwrap();
        assert_eq!(out.text, "pong");
    }
}
