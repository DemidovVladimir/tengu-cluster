//! Engine port — the AI backend powering an agent (OpenRouter, Claude Code,
//! noop) and the `ToolExecutor` the engine's tool loop calls back into.

use async_trait::async_trait;
use futures::Stream;
use std::pin::Pin;

use crate::domain::message::{Message, ModelInfo, StreamEvent, ToolCall, ToolDef};
use crate::ports::tool::ToolOutput;

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
    /// `limits.max_tool_rounds` as the Claude Code engine enforces it: the
    /// tool calls (`tool_use` blocks) of one CLI run — not rounds — past
    /// which the CLI is killed with an error. In-process engines are capped
    /// by their loop instead, in rounds (`collect_engine_response`, a
    /// `run-agent` step's turns).
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
    /// Max chars of one tool result fed back to this engine. `Some` also
    /// swaps a typed row's `data` for its store key
    /// (`Observation::compact_text`) when the row exceeds the cap — a row
    /// that fits arrives whole — in both tool loops (`chat/tool_loop.rs`,
    /// `run-agent`). `None` = the agent's `limits.max_tool_result_chars`
    /// only. `LocalEngine`: `domain::token::tool_result_char_budget`.
    fn tool_result_char_cap(&self) -> Option<usize> {
        None
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

    /// Typed path: text + the tool's `Observation` when it has one. The
    /// default wraps `execute` (no observation), so text-only executors
    /// need no change; `PluginToolExecutor` and `SanitizedToolExecutor`
    /// override it to carry the observation through.
    async fn execute_typed(
        &self,
        call: &ToolCall,
        messages: &[Message],
    ) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::from(self.execute(call, messages).await?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct TextOnly;

    #[async_trait]
    impl ToolExecutor for TextOnly {
        async fn execute(&self, call: &ToolCall, _m: &[Message]) -> anyhow::Result<String> {
            Ok(format!("ran {}", call.name))
        }
    }

    #[tokio::test]
    async fn default_execute_typed_wraps_text() {
        let call = ToolCall {
            id: "1".into(),
            name: "x".into(),
            arguments: json!({}),
        };
        let out = TextOnly.execute_typed(&call, &[]).await.unwrap();
        assert_eq!(out.text, "ran x");
        assert!(out.observation.is_none());
    }
}
