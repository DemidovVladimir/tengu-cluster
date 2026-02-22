//! Hugging Face Inference Providers backend implementation.
//!
//! Potential use case:
//! Route runtime turns to Hugging Face hosted providers through the
//! OpenAI-compatible Chat Completions API.

use async_trait::async_trait;
use futures::stream;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::pin::Pin;
use tracing::{debug, error};

use tengu_core::types::message::Role;
use tengu_core::types::{Message, ModelInfo, StreamEvent, ToolDef};
use tengu_core::{Engine, EngineContext, EngineDiagnostics};

/// Engine implementation backed by Hugging Face Inference Providers.
pub struct HuggingFaceEngine {
    base_url: String,
    model: String,
    api_token: String,
    context_window_tokens: usize,
    max_output_tokens: u32,
    client: reqwest::Client,
}

#[derive(Debug, Serialize)]
struct HuggingFaceChatRequest {
    /// HF model identifier (for example: `THUDM/GLM-4.7:fastest`).
    model: String,
    /// Typed conversation history.
    messages: Vec<HuggingFaceInputMessage>,
    /// Disable streaming for deterministic terminal event handling.
    stream: bool,
    /// Max generated tokens for this turn.
    max_tokens: u32,
}

#[derive(Debug, Serialize)]
struct HuggingFaceInputMessage {
    /// Message role (`system`/`user`/`assistant`).
    role: String,
    /// Message text payload.
    content: String,
}

#[derive(Debug, Deserialize)]
struct HuggingFaceChatResponse {
    /// Generated response candidates.
    #[serde(default)]
    choices: Vec<HuggingFaceChoice>,
    /// Usage counters for this completion.
    #[serde(default)]
    usage: Option<HuggingFaceUsage>,
}

#[derive(Debug, Deserialize)]
struct HuggingFaceChoice {
    /// Choice message payload.
    message: HuggingFaceOutputMessage,
}

#[derive(Debug, Deserialize)]
struct HuggingFaceOutputMessage {
    /// Optional response text or structured content payload.
    #[serde(default)]
    content: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct HuggingFaceUsage {
    /// Prompt token count.
    #[serde(default)]
    prompt_tokens: u32,
    /// Completion token count.
    #[serde(default)]
    completion_tokens: u32,
}

impl HuggingFaceEngine {
    /// Create a new Hugging Face engine using API endpoint, model, and API token.
    ///
    /// `context_window_override` and `max_output_tokens_override` are optional
    /// per-agent hard overrides. When absent, model-aware defaults are used.
    pub fn new(
        base_url: &str,
        model: &str,
        api_token: &str,
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
            api_token: api_token.to_string(),
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

    /// Default context-window mapping for known Hugging Face model families.
    ///
    /// This is a conservative fallback used until model discovery metadata is wired.
    fn default_context_window_tokens(model: &str) -> usize {
        let model = model.to_ascii_lowercase();
        if model.contains("glm-4.7") || model.contains("llama-3.3") {
            128_000
        } else {
            128_000
        }
    }

    /// Default per-turn output cap derived from context size.
    fn default_max_output_tokens(context_window_tokens: usize) -> u32 {
        ((context_window_tokens / 8).clamp(512, 8_192)) as u32
    }

    /// Convert core message roles into OpenAI-compatible request shape.
    ///
    /// If provided, `system_prompt` is prepended as a `system` message.
    fn convert_messages(
        messages: &[Message],
        system_prompt: Option<&str>,
    ) -> Vec<HuggingFaceInputMessage> {
        let system = system_prompt
            .filter(|value| !value.trim().is_empty())
            .map(|value| HuggingFaceInputMessage {
                role: "system".to_string(),
                content: value.to_string(),
            });

        system
            .into_iter()
            .chain(messages.iter().map(|message| {
                HuggingFaceInputMessage {
                    role: match message.role {
                        Role::System => "system",
                        Role::Assistant => "assistant",
                        Role::User | Role::Tool => "user",
                    }
                    .to_string(),
                    content: message.content.clone(),
                }
            }))
            .collect()
    }

    /// Convert OpenAI-compatible message content payload into plain text.
    fn extract_content_text(content: &Value) -> Option<String> {
        if let Some(text) = content.as_str() {
            return Some(text.to_string());
        }

        content.as_array().map(|parts| {
            parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(Value::as_str)
                        .or_else(|| part.get("content").and_then(Value::as_str))
                })
                .collect::<Vec<_>>()
                .join("")
        })
    }

    /// Extract first choice text from HF response.
    fn extract_text(response: &HuggingFaceChatResponse) -> String {
        response
            .choices
            .iter()
            .filter_map(|choice| choice.message.content.as_ref())
            .filter_map(Self::extract_content_text)
            .find(|text| !text.trim().is_empty())
            .unwrap_or_default()
    }

    /// Build terminal success events in deterministic order.
    fn success_events(response: &HuggingFaceChatResponse) -> Vec<StreamEvent> {
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
impl Engine for HuggingFaceEngine {
    fn id(&self) -> &str {
        "huggingface"
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
            provider: "huggingface".to_string(),
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
        let request = HuggingFaceChatRequest {
            model: self.model.clone(),
            messages: Self::convert_messages(messages, context.system_prompt.as_deref()),
            stream: false,
            max_tokens: self.max_output_tokens,
        };

        if request.messages.is_empty() {
            return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                message: "Hugging Face request has no messages".to_string(),
            }])));
        }

        debug!(model = %self.model, "Sending request to Hugging Face Inference Providers");

        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_token))
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!(%status, %body, "Hugging Face request failed");
            return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                message: format!("Hugging Face error {}: {}", status, body),
            }])));
        }

        let parsed: HuggingFaceChatResponse = response.json().await?;
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
        let converted = HuggingFaceEngine::convert_messages(
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
    fn extract_content_text_handles_string_or_structured_content() {
        let string_payload = Value::String("hello".to_string());
        assert_eq!(
            HuggingFaceEngine::extract_content_text(&string_payload).as_deref(),
            Some("hello")
        );

        let structured = serde_json::json!([
            {"type": "text", "text": "part-one"},
            {"type": "text", "text": "part-two"}
        ]);
        assert_eq!(
            HuggingFaceEngine::extract_content_text(&structured).as_deref(),
            Some("part-onepart-two")
        );
    }
}
