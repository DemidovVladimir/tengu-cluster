//! OpenAI Chat Completions backend implementation.
//!
//! Potential use case:
//! Route runtime turns to hosted OpenAI chat models through a typed REST contract.

use async_trait::async_trait;
use futures::stream;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tracing::{debug, error};

use tengu_core::types::message::Role;
use tengu_core::types::{Message, ModelInfo, StreamEvent, ToolDef};
use tengu_core::{Engine, EngineContext, EngineDiagnostics};

/// Engine implementation backed by OpenAI Chat Completions API.
pub struct OpenAIEngine {
    base_url: String,
    model: String,
    api_key: String,
    context_window_tokens: usize,
    max_output_tokens: u32,
    client: reqwest::Client,
}

#[derive(Debug, Serialize)]
struct OpenAIChatRequest {
    /// OpenAI model identifier (for example: `gpt-4o-mini`).
    model: String,
    /// Typed conversation history.
    messages: Vec<OpenAIInputMessage>,
    /// Disable streaming for deterministic terminal event handling.
    stream: bool,
    /// Max generated tokens for this turn.
    max_tokens: u32,
}

#[derive(Debug, Serialize)]
struct OpenAIInputMessage {
    /// OpenAI message role (`system`/`user`/`assistant`).
    role: String,
    /// Message text payload.
    content: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIChatResponse {
    /// Generated response candidates.
    #[serde(default)]
    choices: Vec<OpenAIChoice>,
    /// Usage counters for this completion.
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoice {
    /// Choice message payload.
    message: OpenAIOutputMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAIOutputMessage {
    /// Optional response text.
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAIUsage {
    /// Prompt token count.
    #[serde(default)]
    prompt_tokens: u32,
    /// Completion token count.
    #[serde(default)]
    completion_tokens: u32,
}

impl OpenAIEngine {
    /// Create a new OpenAI engine using API endpoint, model, and API key.
    ///
    /// `context_window_override` and `max_output_tokens_override` are optional
    /// per-agent hard overrides. When absent, model-aware defaults are used.
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: &str,
        context_window_override: Option<usize>,
        max_output_tokens_override: Option<u32>,
    ) -> Self {
        let context_window_tokens =
            Self::resolve_context_window_tokens(model, context_window_override);
        let max_output_tokens =
            Self::resolve_max_output_tokens(context_window_tokens, max_output_tokens_override);
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key: api_key.to_string(),
            context_window_tokens,
            max_output_tokens,
            client: reqwest::Client::new(),
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

    /// Default context-window mapping for known OpenAI model families.
    fn default_context_window_tokens(model: &str) -> usize {
        let model = model.to_ascii_lowercase();
        if model.starts_with("gpt-4.1") {
            1_047_576
        } else if model.starts_with("gpt-4o") || model.starts_with("gpt-4-turbo") {
            128_000
        } else if model.starts_with("gpt-3.5") {
            16_000
        } else {
            128_000
        }
    }

    /// Default per-turn output cap derived from context size.
    ///
    /// This avoids tiny static caps while remaining conservative by default.
    fn default_max_output_tokens(context_window_tokens: usize) -> u32 {
        ((context_window_tokens / 8).clamp(512, 8_192)) as u32
    }

    /// Convert core message roles into OpenAI chat request shape.
    ///
    /// If provided, `system_prompt` is prepended as a `system` message.
    fn convert_messages(
        messages: &[Message],
        system_prompt: Option<&str>,
    ) -> Vec<OpenAIInputMessage> {
        let system = system_prompt
            .filter(|value| !value.trim().is_empty())
            .map(|value| OpenAIInputMessage {
                role: "system".to_string(),
                content: value.to_string(),
            });

        system
            .into_iter()
            .chain(messages.iter().map(|m| {
                OpenAIInputMessage {
                    role: match m.role {
                        Role::System => "system",
                        Role::Assistant => "assistant",
                        Role::User | Role::Tool => "user",
                    }
                    .to_string(),
                    content: m.content.clone(),
                }
            }))
            .collect()
    }

    /// Extract first choice text from OpenAI response.
    fn extract_text(response: &OpenAIChatResponse) -> String {
        response
            .choices
            .iter()
            .filter_map(|choice| choice.message.content.as_deref())
            .next()
            .unwrap_or_default()
            .to_string()
    }

    /// Build terminal success events in deterministic order.
    fn success_events(response: &OpenAIChatResponse) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let text = Self::extract_text(response);
        if !text.is_empty() {
            events.push(StreamEvent::TextDelta { text });
        }
        if let Some(usage) = &response.usage {
            events.push(StreamEvent::Usage {
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
            });
        }
        events.push(StreamEvent::Done);
        events
    }
}

#[async_trait]
impl Engine for OpenAIEngine {
    fn id(&self) -> &str {
        "openai"
    }

    fn context_window(&self) -> usize {
        self.context_window_tokens
    }

    fn max_output_tokens_per_turn(&self) -> u32 {
        self.max_output_tokens
    }

    fn supports_tool_use(&self) -> bool {
        false
    }

    fn manages_own_workspace(&self) -> bool {
        false
    }

    fn supports_streaming(&self) -> bool {
        false
    }

    fn diagnostics(&self) -> EngineDiagnostics {
        EngineDiagnostics {
            engine_id: self.id().to_string(),
            configured_model: Some(self.model.clone()),
            endpoint: Some(self.base_url.clone()),
            transport: Some("http-json".to_string()),
            capabilities: self.capabilities(),
        }
    }

    fn available_models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.clone(),
            provider: "openai".to_string(),
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
        let request = OpenAIChatRequest {
            model: self.model.clone(),
            messages: Self::convert_messages(messages, context.system_prompt.as_deref()),
            stream: false,
            max_tokens: self.max_output_tokens,
        };

        if request.messages.is_empty() {
            return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                message: "OpenAI request has no messages".to_string(),
            }])));
        }

        debug!(model = %self.model, "Sending request to OpenAI");

        let response = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!(%status, %body, "OpenAI request failed");
            return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                message: format!("OpenAI error {}: {}", status, body),
            }])));
        }

        let parsed: OpenAIChatResponse = response.json().await?;
        Ok(Box::pin(stream::iter(Self::success_events(&parsed))))
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
    fn convert_messages_maps_roles() {
        let converted = OpenAIEngine::convert_messages(
            &[
                msg(Role::System, "sys"),
                msg(Role::User, "u"),
                msg(Role::Assistant, "a"),
                msg(Role::Tool, "t"),
            ],
            None,
        );

        assert_eq!(converted.len(), 4);
        assert_eq!(converted[0].role, "system");
        assert_eq!(converted[1].role, "user");
        assert_eq!(converted[2].role, "assistant");
        assert_eq!(converted[3].role, "user");
    }

    #[test]
    fn convert_messages_prepends_context_system_prompt() {
        let converted = OpenAIEngine::convert_messages(
            &[msg(Role::User, "u"), msg(Role::Assistant, "a")],
            Some("runtime system"),
        );

        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].role, "system");
        assert_eq!(converted[0].content, "runtime system");
        assert_eq!(converted[1].role, "user");
        assert_eq!(converted[2].role, "assistant");
    }

    #[test]
    fn success_events_order_text_usage_done() {
        let response = OpenAIChatResponse {
            choices: vec![OpenAIChoice {
                message: OpenAIOutputMessage {
                    content: Some("hello".to_string()),
                },
            }],
            usage: Some(OpenAIUsage {
                prompt_tokens: 11,
                completion_tokens: 6,
            }),
        };

        let events = OpenAIEngine::success_events(&response);
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], StreamEvent::TextDelta { ref text } if text == "hello"));
        assert!(matches!(
            events[1],
            StreamEvent::Usage {
                input_tokens: 11,
                output_tokens: 6
            }
        ));
        assert!(matches!(events[2], StreamEvent::Done));
    }

    #[test]
    fn diagnostics_report_endpoint_model_and_transport() {
        let engine = OpenAIEngine::new(
            "https://api.openai.com/",
            "gpt-4o-mini",
            "test-key",
            None,
            None,
        );
        let diagnostics = engine.diagnostics();

        assert_eq!(diagnostics.engine_id, "openai");
        assert_eq!(diagnostics.configured_model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(
            diagnostics.endpoint.as_deref(),
            Some("https://api.openai.com")
        );
        assert_eq!(diagnostics.transport.as_deref(), Some("http-json"));
    }

    #[test]
    fn context_window_uses_model_defaults_and_override() {
        assert_eq!(
            OpenAIEngine::resolve_context_window_tokens("gpt-4.1", None),
            1_047_576
        );
        assert_eq!(
            OpenAIEngine::resolve_context_window_tokens("gpt-4o-mini", None),
            128_000
        );
        assert_eq!(
            OpenAIEngine::resolve_context_window_tokens("custom-model", Some(65_536)),
            65_536
        );
    }

    #[test]
    fn max_output_tokens_uses_fallback_and_override() {
        assert_eq!(
            OpenAIEngine::resolve_max_output_tokens(128_000, None),
            8_192
        );
        assert_eq!(OpenAIEngine::resolve_max_output_tokens(8_192, None), 1_024);
        assert_eq!(
            OpenAIEngine::resolve_max_output_tokens(128_000, Some(12_000)),
            12_000
        );
    }
}
