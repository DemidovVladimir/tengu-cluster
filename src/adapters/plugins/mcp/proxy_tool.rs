// src/adapters/plugins/mcp/proxy_tool.rs
//! `McpProxyTool` — forwards an `execute` call to a remote MCP server.
//!
//! One instance exists per advertised remote tool. The tool name is always
//! `{server_name}.{remote_tool_name}` so multiple MCP servers cannot collide.

#![allow(dead_code)]

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;

use super::client::McpCaller;
#[cfg(test)]
use crate::adapters::tool_plugin::ConversationView;
use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDef;

pub(crate) struct McpProxyTool {
    pub(crate) def: ToolDef,
    pub(crate) remote_name: String,
    pub(crate) client: Arc<dyn McpCaller>,
}

#[async_trait]
impl Tool for McpProxyTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // scope: pure-compute — the call itself is an opaque RPC to an
        // external MCP server. Tengu's scope layer cannot introspect what
        // the remote tool does with the arguments; the operator controls
        // risk by choosing which MCP servers to configure.
        let text = self.client.call_tool(&self.remote_name, args).await?;
        Ok(ToolOutput::from(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::plugins::mcp::protocol::McpRemoteTool;
    use crate::adapters::ports::{ShellExecutionPort, ToolActivityPort, ToolScope};
    use crate::adapters::secret_builder::SecretRegistry;
    use crate::adapters::shell_executor::LocalShellExecutor;
    use crate::adapters::types::ToolCall;
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;

    struct NoopActivity;
    impl ToolActivityPort for NoopActivity {
        fn publish_tool_activity(&self, _call: &ToolCall) {}
    }

    /// Stub MCP caller — records the most recent call and returns a canned
    /// string. Exists only to verify that the proxy tool forwards the right
    /// arguments and passes through the result.
    struct StubCaller {
        response: String,
        last_call: Arc<std::sync::Mutex<Option<(String, Value)>>>,
    }

    #[async_trait]
    impl McpCaller for StubCaller {
        async fn list_tools(&self) -> Result<Vec<McpRemoteTool>> {
            Ok(vec![])
        }
        async fn call_tool(&self, name: &str, args: &Value) -> Result<String> {
            *self.last_call.lock().unwrap() = Some((name.to_string(), args.clone()));
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn proxy_tool_calls_client() {
        let tmp = TempDir::new().unwrap();
        let workspace: PathBuf = tmp.path().to_path_buf();
        let last_call = Arc::new(std::sync::Mutex::new(None));
        let stub: Arc<dyn McpCaller> = Arc::new(StubCaller {
            response: "remote output".to_string(),
            last_call: Arc::clone(&last_call),
        });

        let tool = McpProxyTool {
            def: ToolDef::new("github.create_issue", "desc", json!({"type": "object"})),
            remote_name: "create_issue".to_string(),
            client: stub,
        };

        let shell: Arc<dyn ShellExecutionPort> = Arc::new(LocalShellExecutor::new());
        let http = reqwest::Client::new();
        let secrets = SecretRegistry::new();
        let activity: Arc<dyn ToolActivityPort> = Arc::new(NoopActivity);
        let scope = ToolScope::default();
        let ctx = ToolCtx {
            workspace: &workspace,
            scope: &scope,
            shell: shell.as_ref(),
            http: &http,
            memory: None,
            secret_registry: &secrets,
            activity: activity.as_ref(),
            subagents: None,
            conversation: ConversationView::empty(),
        };

        let args = json!({"title": "bug"});
        let out = tool.execute(&args, &ctx).await.unwrap();
        assert_eq!(out.text, "remote output");

        let recorded = last_call.lock().unwrap().clone().unwrap();
        assert_eq!(recorded.0, "create_issue");
        assert_eq!(recorded.1, json!({"title": "bug"}));
    }
}
