//! Claude Code subprocess backend implementation.
//!
//! Potential use case:
//! Use local `claude` CLI authentication (Pro/MAX/API-key setup) from Tengu
//! without issuing direct Anthropic REST calls in this backend path.

use async_trait::async_trait;
use futures::stream;
use futures::Stream;
use serde_json::Value;
use std::pin::Pin;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, error};

use tengu_core::types::message::Role;
use tengu_core::types::{Message, ModelInfo, StreamEvent, ToolDef};
use tengu_core::{Engine, EngineContext, EngineDiagnostics};

/// Engine implementation backed by Claude Code CLI subprocess execution.
pub struct ClaudeCodeEngine {
    binary: String,
    model: String,
    context_window_tokens: usize,
    max_output_tokens: u32,
}

impl ClaudeCodeEngine {
    /// Create a new Claude Code engine.
    ///
    /// Runtime env knobs:
    /// - `CLAUDE_CODE_BIN`: optional CLI binary path (default: `claude`)
    pub fn new(
        model: &str,
        context_window_override: Option<usize>,
        max_output_tokens_override: Option<u32>,
    ) -> Self {
        let context_window_tokens =
            Self::resolve_context_window_tokens(model, context_window_override);
        let max_output_tokens =
            Self::resolve_max_output_tokens(context_window_tokens, max_output_tokens_override);
        let binary = std::env::var("CLAUDE_CODE_BIN").unwrap_or_else(|_| "claude".to_string());
        Self {
            binary,
            model: model.to_string(),
            context_window_tokens,
            max_output_tokens,
        }
    }

    /// Resolve context window using optional override then model-aware defaults.
    fn resolve_context_window_tokens(model: &str, override_value: Option<usize>) -> usize {
        override_value
            .filter(|value| *value > 0)
            .unwrap_or_else(|| Self::default_context_window_tokens(model))
    }

    /// Resolve per-turn output cap using optional override then context-derived fallback.
    fn resolve_max_output_tokens(context_window_tokens: usize, override_value: Option<u32>) -> u32 {
        override_value
            .filter(|value| *value > 0)
            .unwrap_or_else(|| Self::default_max_output_tokens(context_window_tokens))
    }

    /// Default context-window mapping for known Claude model families.
    fn default_context_window_tokens(model: &str) -> usize {
        let model = model.to_ascii_lowercase();
        if model.starts_with("claude-3") || model.starts_with("claude-4") {
            200_000
        } else {
            200_000
        }
    }

    /// Default per-turn output cap derived from context size.
    fn default_max_output_tokens(context_window_tokens: usize) -> u32 {
        ((context_window_tokens / 8).clamp(512, 8_192)) as u32
    }

    /// Render runtime messages into one Claude CLI prompt payload.
    fn assemble_prompt(messages: &[Message]) -> String {
        messages
            .iter()
            .map(|message| {
                let role = match message.role {
                    Role::System => "System",
                    Role::User => "User",
                    Role::Assistant => "Assistant",
                    Role::Tool => "Tool",
                };
                format!("[{}]\n{}", role, message.content)
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Extract best-effort text field from Claude JSON output.
    fn extract_text(value: &Value) -> Option<String> {
        [
            value.get("result").and_then(Value::as_str),
            value.get("content").and_then(Value::as_str),
            value.get("output").and_then(Value::as_str),
            value.get("text").and_then(Value::as_str),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|text| !text.is_empty())
        .map(ToString::to_string)
    }

    /// Extract best-effort usage counters from Claude JSON output.
    fn extract_usage(value: &Value) -> Option<(u32, u32)> {
        let usage = value.get("usage")?;
        let input = usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        let output = usage
            .get("output_tokens")
            .or_else(|| usage.get("completion_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32;
        Some((input, output))
    }

    /// Parse subprocess stdout as JSON first, then fall back to raw text.
    fn parse_stdout(stdout: &str) -> (String, Option<(u32, u32)>) {
        let trimmed = stdout.trim();
        if trimmed.is_empty() {
            return (String::new(), None);
        }

        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            let text = Self::extract_text(&value).unwrap_or_default();
            let usage = Self::extract_usage(&value);
            return (text, usage);
        }

        (trimmed.to_string(), None)
    }
}

#[async_trait]
impl Engine for ClaudeCodeEngine {
    fn id(&self) -> &str {
        "claude-code"
    }

    fn context_window(&self) -> usize {
        self.context_window_tokens
    }

    fn max_output_tokens_per_turn(&self) -> u32 {
        self.max_output_tokens
    }

    fn supports_tool_use(&self) -> bool {
        true
    }

    fn manages_own_workspace(&self) -> bool {
        true
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn diagnostics(&self) -> EngineDiagnostics {
        EngineDiagnostics {
            engine_id: self.id().to_string(),
            configured_model: Some(self.model.clone()),
            endpoint: Some(self.binary.clone()),
            transport: Some("subprocess-json".to_string()),
            capabilities: self.capabilities(),
        }
    }

    fn available_models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.clone(),
            provider: "claude-code".to_string(),
            display_name: self.model.clone(),
            context_window: self.context_window(),
            supports_tools: self.supports_tool_use(),
            supports_streaming: self.supports_streaming(),
        }]
    }

    async fn run(
        &self,
        messages: &[Message],
        _tools: &[ToolDef],
        context: &EngineContext,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>> {
        if messages.is_empty() {
            return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                message: "Claude Code request has no messages".to_string(),
            }])));
        }

        let mut command = Command::new(&self.binary);
        command
            .arg("--print")
            .arg("--output-format")
            .arg("json")
            .arg("--model")
            .arg(&self.model)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        if let Some(workspace) = context.workspace.as_ref() {
            command.arg("--add-dir").arg(workspace);
        }
        if let Some(system_prompt) = context
            .system_prompt
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            command.arg("--system-prompt").arg(system_prompt);
        }

        debug!(model = %self.model, binary = %self.binary, "Spawning Claude Code subprocess");
        let mut child = command.spawn()?;

        if let Some(mut stdin) = child.stdin.take() {
            let prompt = Self::assemble_prompt(messages);
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.shutdown().await?;
        }

        let output = child.wait_with_output().await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if !output.status.success() {
            error!(
                status = ?output.status.code(),
                stderr = %stderr.trim(),
                "Claude Code subprocess failed"
            );
            return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                message: format!(
                    "Claude Code subprocess failed (status={:?}): {}",
                    output.status.code(),
                    stderr.trim()
                ),
            }])));
        }

        let (text, usage) = Self::parse_stdout(&stdout);
        if text.is_empty() {
            return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                message: "Claude Code returned empty output".to_string(),
            }])));
        }

        let mut events = vec![StreamEvent::TextDelta { text }];
        if let Some((input_tokens, output_tokens)) = usage {
            events.push(StreamEvent::Usage {
                input_tokens,
                output_tokens,
            });
        }
        events.push(StreamEvent::Done);
        Ok(Box::pin(stream::iter(events)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    #[test]
    fn assemble_prompt_includes_roles() {
        let prompt = ClaudeCodeEngine::assemble_prompt(&[
            msg(Role::User, "hello"),
            msg(Role::Assistant, "world"),
        ]);
        assert!(prompt.contains("[User]"));
        assert!(prompt.contains("[Assistant]"));
    }

    #[test]
    fn parse_stdout_handles_json_text_and_usage() {
        let stdout = r#"{"result":"hello","usage":{"input_tokens":12,"output_tokens":7}}"#;
        let (text, usage) = ClaudeCodeEngine::parse_stdout(stdout);
        assert_eq!(text, "hello");
        assert_eq!(usage, Some((12, 7)));
    }

    #[test]
    fn parse_stdout_falls_back_to_plain_text() {
        let (text, usage) = ClaudeCodeEngine::parse_stdout("plain output");
        assert_eq!(text, "plain output");
        assert_eq!(usage, None);
    }

    #[test]
    fn diagnostics_report_binary_model_and_transport() {
        let engine = ClaudeCodeEngine::new("claude-sonnet-4-5-20250929", None, None);
        let diagnostics = engine.diagnostics();

        assert_eq!(diagnostics.engine_id, "claude-code");
        assert_eq!(
            diagnostics.configured_model.as_deref(),
            Some("claude-sonnet-4-5-20250929")
        );
        assert_eq!(diagnostics.transport.as_deref(), Some("subprocess-json"));
    }
}
