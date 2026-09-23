//! Engine port — the AI backend powering an agent (OpenRouter, Claude Code,
//! noop) and the `ToolExecutor` the engine's tool loop calls back into.

use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

use crate::domain::message::{Message, ModelInfo, StreamEvent, ToolCall, ToolDef};

/// Runtime-discoverable engine capability snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineCapabilities {
    pub context_window: usize,
    pub max_output_tokens_per_turn: u32,
    pub supports_tool_use: bool,
    pub supports_streaming: bool,
    pub manages_own_workspace: bool,
}

/// Runtime diagnostics metadata surfaced by engines for status/doctor output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineDiagnostics {
    pub engine_id: String,
    pub configured_model: Option<String>,
    pub endpoint: Option<String>,
    pub transport: Option<String>,
    pub capabilities: EngineCapabilities,
}

// Fields populated for engine builders that route through the Claude Code
// MCP bridge. The OpenRouter / Sonnet path used by the v2 RagPlanner +
// SubprocessRunner ignores everything except `system_prompt`. Kept for the
// in-flight bridge engine refactor; no removal until that lands.
#[allow(dead_code)]
pub struct EngineContext {
    pub workspace: Option<std::path::PathBuf>,
    pub system_prompt: Option<String>,
    /// Tools to expose via MCP bridge (used by Claude Code engine).
    pub bridge_tools: Option<Vec<ToolDef>>,
    /// Maximum tool call rounds before killing the session.
    /// Enforced inside the Claude Code NDJSON reader (the outer
    /// `collect_engine_response` loop already caps OpenRouter rounds).
    pub max_tool_rounds: Option<u32>,
    /// Maximum chars per MCP bridge tool result. Passed to the bridge
    /// subprocess via `TENGU_BRIDGE_MAX_RESULT_CHARS`.
    pub max_mcp_result_chars: Option<u32>,
    /// `[[mcp_servers]]` the Claude Code engine hands to its tengu bridge so
    /// `{server}__{tool}` entries in `bridge_tools` can execute there. Empty
    /// everywhere except `run-agent` today.
    pub mcp_servers: Vec<crate::config::McpServerConfig>,
}

#[async_trait]
pub trait Engine: Send + Sync {
    fn id(&self) -> &str;
    fn context_window(&self) -> usize;
    fn max_output_tokens_per_turn(&self) -> u32 {
        ((self.context_window() / 8).clamp(256, 16_384)) as u32
    }
    fn supports_tool_use(&self) -> bool;
    fn manages_own_workspace(&self) -> bool;
    fn supports_streaming(&self) -> bool {
        false
    }
    fn capabilities(&self) -> EngineCapabilities {
        EngineCapabilities {
            context_window: self.context_window(),
            max_output_tokens_per_turn: self.max_output_tokens_per_turn(),
            supports_tool_use: self.supports_tool_use(),
            supports_streaming: self.supports_streaming(),
            manages_own_workspace: self.manages_own_workspace(),
        }
    }
    fn diagnostics(&self) -> EngineDiagnostics {
        EngineDiagnostics {
            engine_id: self.id().to_string(),
            configured_model: self.available_models().first().map(|m| m.id.clone()),
            endpoint: None,
            transport: None,
            capabilities: self.capabilities(),
        }
    }
    fn available_models(&self) -> Vec<ModelInfo>;

    async fn run(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        context: &EngineContext,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>>;
}

/// Trait for executing tool calls.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, call: &ToolCall, messages: &[Message]) -> anyhow::Result<String>;
}
