// src/adapters/tool_plugin.rs
//! Tool plugin architecture — per-tool trait, plugin grouping, and registry.

#![allow(dead_code)]

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::adapters::memory_builder::MemoryServiceHandle;
use crate::adapters::plugins::subagents::SubagentRegistry;
use crate::adapters::ports::{ShellExecutionPort, ToolActivityPort, ToolScope};
use crate::adapters::secret_builder::SecretRegistry;
use crate::adapters::types::{ToolCall, ToolDef};

// ---------------------------------------------------------------------------
// Core trait
// ---------------------------------------------------------------------------

/// A single callable tool exposed to the LLM.
#[async_trait]
pub(crate) trait Tool: Send + Sync {
    fn definition(&self) -> &ToolDef;
    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput>;
}

/// Output of a tool call.
#[derive(Debug, Clone)]
pub(crate) struct ToolOutput {
    pub text: String,
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        Self { text }
    }
}

// ---------------------------------------------------------------------------
// Plugin trait
// ---------------------------------------------------------------------------

/// A group of related tools instantiated together.
#[async_trait]
pub(crate) trait ToolPlugin: Send + Sync {
    fn name(&self) -> &'static str;
    async fn tools(&self, ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>>;
}

// ---------------------------------------------------------------------------
// Contexts
// ---------------------------------------------------------------------------

/// Read-only view of the calling agent's message history. Passed by the engine
/// to each `Tool::execute` so tools can inspect conversation context without
/// being able to mutate it.
#[derive(Clone, Copy)]
pub(crate) struct ConversationView<'a> {
    messages: &'a [crate::adapters::types::Message],
}

impl<'a> ConversationView<'a> {
    pub(crate) fn new(messages: &'a [crate::adapters::types::Message]) -> Self {
        Self { messages }
    }
    pub(crate) fn empty() -> Self {
        Self { messages: &[] }
    }
    pub(crate) fn len(&self) -> usize {
        self.messages.len()
    }
    pub(crate) fn slice(
        &self,
        from: usize,
        to: usize,
    ) -> anyhow::Result<&'a [crate::adapters::types::Message]> {
        if from > to || to > self.messages.len() {
            anyhow::bail!(
                "conversation slice out of range: {from}..{to} len={}",
                self.messages.len()
            );
        }
        Ok(&self.messages[from..to])
    }
}

/// Per-call context passed to every `Tool::execute`. Borrowed, never stored.
pub(crate) struct ToolCtx<'a> {
    pub workspace: &'a Path,
    pub scope: &'a ToolScope,
    pub shell: &'a dyn ShellExecutionPort,
    pub http: &'a reqwest::Client,
    pub memory: Option<&'a MemoryServiceHandle>,
    pub secret_registry: &'a SecretRegistry,
    pub activity: &'a dyn ToolActivityPort,
    /// Subagent registry handle — present only when the orchestrator is
    /// enabled (gated by `OrchestratorConfig.enabled`). The subagents plugin
    /// uses this to spawn / kill / steer LLM-driven subagents; all other
    /// plugins ignore it.
    pub subagents: Option<&'a SubagentRegistry>,
    /// Read-only snapshot of the calling agent's message history up to the
    /// current turn. Tools that need to inspect the conversation (e.g.
    /// `skill_distill`) use this; all other tools ignore it.
    pub conversation: ConversationView<'a>,
}

/// Construction-time context passed to `ToolPlugin::tools()`.
pub(crate) struct PluginCtx<'a> {
    pub workspace: &'a Path,
    pub config: &'a crate::adapters::config::AgentConfig,
    pub http: reqwest::Client,
    pub shell: Arc<dyn ShellExecutionPort>,
    pub memory: Option<Arc<MemoryServiceHandle>>,
    pub secret_registry: Arc<SecretRegistry>,
    /// Subagent registry — only set when the orchestrator is enabled.
    pub subagents: Option<Arc<SubagentRegistry>>,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub(crate) struct ToolRegistry {
    by_name: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub(crate) fn new() -> Self {
        Self {
            by_name: HashMap::new(),
        }
    }

    /// Register tools from a plugin, filtered by the allow list.
    pub(crate) async fn register_plugin(
        &mut self,
        plugin: &dyn ToolPlugin,
        ctx: &PluginCtx<'_>,
        allowed: &[String],
    ) -> Result<()> {
        for tool in plugin.tools(ctx).await? {
            let name = tool.definition().name.clone();
            if !allowed.is_empty() && !allowed.contains(&name) {
                continue;
            }
            if self.by_name.contains_key(&name) {
                anyhow::bail!("duplicate tool '{}' from plugin '{}'", name, plugin.name());
            }
            self.by_name.insert(name, tool);
        }
        Ok(())
    }

    /// Register a single tool directly.
    pub(crate) fn register_tool(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.definition().name.clone();
        self.by_name.insert(name, tool);
    }

    pub(crate) fn definitions(&self) -> Vec<ToolDef> {
        self.by_name.values().map(|t| t.definition().clone()).collect()
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.by_name.get(name)
    }

    pub(crate) fn tool_names(&self) -> Vec<String> {
        self.by_name.keys().cloned().collect()
    }

    pub(crate) async fn invoke(
        &self,
        name: &str,
        args: &Value,
        ctx: &ToolCtx<'_>,
    ) -> Result<ToolOutput> {
        let tool = self
            .by_name
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool '{}'", name))?;
        tool.execute(args, ctx).await
    }
}

// ---------------------------------------------------------------------------
// PluginToolExecutor — bridges ToolRegistry into the engine's ToolExecutor
// ---------------------------------------------------------------------------

use crate::adapters::engine_builder::ToolExecutor;

/// Wraps a ToolRegistry + context handles to implement the engine's ToolExecutor.
pub(crate) struct PluginToolExecutor {
    pub registry: ToolRegistry,
    pub workspace: std::path::PathBuf,
    pub shell: Arc<dyn ShellExecutionPort>,
    pub http: reqwest::Client,
    pub memory: Option<Arc<MemoryServiceHandle>>,
    pub secret_registry: Arc<SecretRegistry>,
    pub activity: Arc<dyn ToolActivityPort>,
    pub scopes: HashMap<String, ToolScope>,
    /// Subagent registry — only populated when the orchestrator is enabled.
    pub subagents: Option<Arc<SubagentRegistry>>,
    /// Snapshot of the calling agent's message history at the start of the
    /// current turn. Updated by callers before invoking `collect_engine_response`.
    /// Empty when not set (non-distill tools ignore this field entirely).
    pub conversation: Vec<crate::adapters::types::Message>,
}

impl PluginToolExecutor {
    /// Return tool definitions registered in the executor but NOT in `already_advertised`.
    ///
    /// Used to surface dynamically-discovered plugin tools (currently: MCP proxy tools
    /// with `{server}.{tool}` names) to the LLM. The static plugins (workspace, http,
    /// crypto, cache, memory, skill, subagents) contribute tool defs via their own
    /// `tool_defs()` helpers which the caller already includes; this method returns
    /// only the extras.
    pub(crate) fn additional_tool_defs(&self, already_advertised: &[ToolDef]) -> Vec<ToolDef> {
        let known: std::collections::HashSet<&str> = already_advertised
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        self.registry
            .definitions()
            .into_iter()
            .filter(|def| !known.contains(def.name.as_str()))
            .collect()
    }
}

#[async_trait]
impl ToolExecutor for PluginToolExecutor {
    async fn execute(&self, call: &ToolCall) -> Result<String> {
        self.activity.publish_tool_activity(call);

        if self.registry.get(&call.name).is_none() {
            anyhow::bail!("Tool '{}' is not available to this agent.", call.name);
        }

        let scope = self.scopes.get(&call.name).cloned().unwrap_or_default();
        let view = ConversationView::new(&self.conversation);
        let ctx = ToolCtx {
            workspace: &self.workspace,
            scope: &scope,
            shell: self.shell.as_ref(),
            http: &self.http,
            memory: self.memory.as_ref().map(|m| m.as_ref()),
            secret_registry: &self.secret_registry,
            activity: self.activity.as_ref(),
            subagents: self.subagents.as_ref().map(|r| r.as_ref()),
            conversation: view,
        };

        let output = self.registry.invoke(&call.name, &call.arguments, &ctx).await?;
        Ok(output.text)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::secret_builder::SecretRegistry;
    use crate::adapters::shell_executor::LocalShellExecutor;
    use serde_json::Value;

    struct StubTool {
        def: ToolDef,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn definition(&self) -> &ToolDef {
            &self.def
        }
        async fn execute(&self, _args: &Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
            Ok(ToolOutput::from(String::new()))
        }
    }

    struct StubActivity;
    impl ToolActivityPort for StubActivity {
        fn publish_tool_activity(&self, _call: &ToolCall) {}
    }

    fn make_executor(defs: Vec<ToolDef>) -> PluginToolExecutor {
        let mut registry = ToolRegistry::new();
        for def in defs {
            registry.register_tool(Arc::new(StubTool { def }));
        }
        PluginToolExecutor {
            registry,
            workspace: std::path::PathBuf::from("."),
            shell: Arc::new(LocalShellExecutor::new()),
            http: reqwest::Client::new(),
            memory: None,
            secret_registry: Arc::new(SecretRegistry::new()),
            activity: Arc::new(StubActivity),
            scopes: HashMap::new(),
            subagents: None,
            conversation: Vec::new(),
        }
    }

    #[test]
    fn additional_tool_defs_returns_only_unadvertised_tools() {
        let static_def = ToolDef::new("workspace.read", "desc", serde_json::json!({}));
        let dynamic_def = ToolDef::new("github.create_issue", "desc", serde_json::json!({}));
        let exec = make_executor(vec![static_def.clone(), dynamic_def.clone()]);

        // Caller already advertises only the static tool.
        let already = vec![static_def.clone()];
        let extras = exec.additional_tool_defs(&already);

        assert_eq!(extras.len(), 1, "expected exactly one extra tool");
        assert_eq!(extras[0].name, "github.create_issue");
    }

    #[test]
    fn additional_tool_defs_empty_when_all_known() {
        let a = ToolDef::new("a", "desc", serde_json::json!({}));
        let b = ToolDef::new("b", "desc", serde_json::json!({}));
        let exec = make_executor(vec![a.clone(), b.clone()]);

        let already = vec![a, b];
        assert!(exec.additional_tool_defs(&already).is_empty());
    }
}
