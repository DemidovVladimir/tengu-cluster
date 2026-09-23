//! Tool port — the per-tool trait, plugin grouping, and the borrowed contexts
//! passed to them. Implementations live in `adapters/outbound/tools/`
//! (currently `outbound/tools/`); dispatch lives in
//! `application/tools/registry.rs`.

#![allow(dead_code)]

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;

use crate::domain::message::ToolDef;
use crate::domain::scope::ToolScope;
use crate::domain::secrets::SecretRegistry;
use crate::ports::memory::MemoryService;
use crate::ports::shell::ShellExecutionPort;
use crate::ports::tool_activity::ToolActivityPort;

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

/// Read-only view of the current conversation messages, passed to tools
/// that need to inspect history (e.g. skill_lifecycle::distill).
pub(crate) struct ConversationView<'a> {
    messages: &'a [crate::domain::message::Message],
}

impl<'a> ConversationView<'a> {
    pub(crate) fn new(messages: &'a [crate::domain::message::Message]) -> Self {
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
    ) -> anyhow::Result<&'a [crate::domain::message::Message]> {
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
    pub memory_manager: Option<&'a dyn MemoryService>,
    pub secret_registry: &'a SecretRegistry,
    pub activity: &'a dyn ToolActivityPort,
    pub conversation: ConversationView<'a>,
    /// Calling agent's resolved config. `Some(_)` for tool calls dispatched
    /// from inside an agent loop (populated by `PluginToolExecutor` from the
    /// `AgentConfig` it was built with); `None` for harness-level invocations
    /// (e.g. `tengu eval` runner construction, MetricRunCtx-degraded paths,
    /// most unit tests).
    pub agent_config: Option<&'a crate::config::AgentConfig>,
}

/// Construction-time context passed to `ToolPlugin::tools()`.
pub(crate) struct PluginCtx<'a> {
    pub workspace: &'a Path,
    pub config: &'a crate::config::AgentConfig,
    pub http: reqwest::Client,
    pub shell: Arc<dyn ShellExecutionPort>,
    pub memory_manager: Option<Arc<dyn MemoryService>>,
    pub secret_registry: Arc<SecretRegistry>,
}

/// Every tool definition the planner's registry lists: the built-in catalog
/// plus the tools of each `[[mcp_servers]]` entry. Impl:
/// `outbound::tools::CatalogDirectory`.
#[async_trait]
pub(crate) trait ToolDirectory: Send + Sync {
    async fn all_tool_defs(&self) -> Vec<ToolDef>;
}
