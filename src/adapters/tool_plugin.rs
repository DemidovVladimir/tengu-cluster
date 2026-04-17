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
use crate::adapters::ports::{ShellExecutionPort, ToolActivityPort, ToolExecutionPort, ToolScope};
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
// Legacy bridge — wraps sync ToolExecutionPort as async Tool
// ---------------------------------------------------------------------------

/// Adapts an old sync executor + tool definition into the new async Tool trait.
/// Used during incremental migration (A1–A8). Deleted in A9.
pub(crate) struct LegacyToolBridge {
    def: ToolDef,
    executor: Arc<dyn ToolExecutionPort>,
}

impl LegacyToolBridge {
    pub(crate) fn new(def: ToolDef, executor: Arc<dyn ToolExecutionPort>) -> Self {
        Self { def, executor }
    }
}

#[async_trait]
impl Tool for LegacyToolBridge {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        let call = ToolCall {
            id: String::new(),
            name: self.def.name.clone(),
            arguments: args.clone(),
        };
        let result = self.executor.execute_tool(&call)?;
        Ok(ToolOutput::from(result))
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
}

#[async_trait]
impl ToolExecutor for PluginToolExecutor {
    async fn execute(&self, call: &ToolCall) -> Result<String> {
        self.activity.publish_tool_activity(call);

        if self.registry.get(&call.name).is_none() {
            anyhow::bail!("Tool '{}' is not available to this agent.", call.name);
        }

        let scope = self.scopes.get(&call.name).cloned().unwrap_or_default();
        let ctx = ToolCtx {
            workspace: &self.workspace,
            scope: &scope,
            shell: self.shell.as_ref(),
            http: &self.http,
            memory: self.memory.as_ref().map(|m| m.as_ref()),
            secret_registry: &self.secret_registry,
            activity: self.activity.as_ref(),
            subagents: self.subagents.as_ref().map(|r| r.as_ref()),
        };

        let output = self.registry.invoke(&call.name, &call.arguments, &ctx).await?;
        Ok(output.text)
    }
}
