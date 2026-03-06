//! OpenRouter unified API backend implementation.
//!
//! Routes runtime turns through the OpenRouter aggregation layer, giving access
//! to hundreds of models (Anthropic, OpenAI, Google, Meta, Mistral, etc.) behind
//! a single API key and OpenAI-compatible REST contract.
//!
//! API reference: <https://openrouter.ai/docs/quickstart>

use async_trait::async_trait;
use futures::stream;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tracing::{debug, error};

use crate::tooling::{
    append_tool_call_events, convert_messages_openai_compatible, map_function_tools,
};
use tengu_core::types::{Message, ModelInfo, StreamEvent, ToolDef};
use tengu_core::{Engine, EngineContext, EngineDiagnostics};

/// Engine implementation backed by the OpenRouter unified API.
///
/// OpenRouter is an aggregation layer that exposes models from multiple
/// providers (Anthropic, OpenAI, Google, Meta, Mistral, etc.) through a
/// single OpenAI-compatible endpoint. This means one API key, one bill,
/// and automatic fallback handling.
///
/// Model IDs use the `provider/model` format, e.g.:
/// - `anthropic/claude-sonnet-4`
/// - `openai/gpt-4o`
/// - `google/gemini-2.5-pro`
/// - `meta-llama/llama-4-maverick`
pub struct OpenRouterEngine {
    base_url: String,
    model: String,
    api_key: String,
    context_window_tokens: usize,
    max_output_tokens: u32,
    /// Optional site URL for OpenRouter leaderboard attribution.
    referer: Option<String>,
    /// Optional app title for OpenRouter leaderboard attribution.
    title: Option<String>,
    client: reqwest::Client,
}

// ── Request types ──────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct OpenRouterChatRequest {
    model: String,
    messages: Vec<serde_json::Value>,
    stream: bool,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenRouterToolDef>,
}

#[derive(Debug, Serialize)]
struct OpenRouterToolDef {
    #[serde(rename = "type")]
    kind: String,
    function: OpenRouterFunction,
}

#[derive(Debug, Serialize)]
struct OpenRouterFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

// ── Response types ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct OpenRouterChatResponse {
    #[serde(default)]
    choices: Vec<OpenRouterChoice>,
    #[serde(default)]
    usage: Option<OpenRouterUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterChoice {
    message: OpenRouterOutputMessage,
}

#[derive(Debug, Deserialize)]
struct OpenRouterOutputMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenRouterResponseToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenRouterResponseToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: OpenRouterResponseFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenRouterResponseFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenRouterUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

impl OpenRouterEngine {
    /// Create a new OpenRouter engine.
    ///
    /// # Arguments
    /// - `base_url` — API base (defaults to `https://openrouter.ai/api`).
    /// - `model` — Model ID in `provider/model` format (e.g. `anthropic/claude-sonnet-4`).
    /// - `api_key` — OpenRouter API key.
    /// - `context_window_override` — Override the default context window.
    /// - `max_output_tokens_override` — Override the default max output tokens.
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
            referer: std::env::var("OPENROUTER_REFERER").ok(),
            title: std::env::var("OPENROUTER_TITLE").ok(),
            client: reqwest::Client::new(),
        }
    }

    fn resolve_context_window_tokens(model: &str, override_value: Option<usize>) -> usize {
        override_value
            .filter(|value| *value > 0)
            .unwrap_or_else(|| Self::default_context_window_tokens(model))
    }

    fn resolve_max_output_tokens(context_window_tokens: usize, override_value: Option<u32>) -> u32 {
        override_value
            .filter(|value| *value > 0)
            .unwrap_or_else(|| Self::default_max_output_tokens(context_window_tokens))
    }

    /// Infer context window from model ID. OpenRouter model IDs use
    /// `provider/model` format, so we strip the provider prefix before matching.
    fn default_context_window_tokens(model: &str) -> usize {
        let model_lower = model.to_ascii_lowercase();
        // Strip provider prefix for matching (e.g. "anthropic/claude-sonnet-4" -> "claude-sonnet-4")
        let model_name = model_lower
            .split('/')
            .last()
            .unwrap_or(&model_lower);

        if model_name.contains("claude") {
            200_000
        } else if model_name.starts_with("gpt-4.1") {
            1_047_576
        } else if model_name.starts_with("gpt-4o") || model_name.starts_with("gpt-4-turbo") {
            128_000
        } else if model_name.contains("gemini") {
            1_000_000
        } else if model_name.contains("llama") {
            128_000
        } else if model_name.contains("mistral") || model_name.contains("mixtral") {
            128_000
        } else if model_name.contains("deepseek") {
            128_000
        } else {
            128_000
        }
    }

    fn default_max_output_tokens(context_window_tokens: usize) -> u32 {
        ((context_window_tokens / 8).clamp(512, 8_192)) as u32
    }

    /// Convert tengu ToolDef array to OpenAI-compatible function-calling format.
    fn convert_tools(tools: &[ToolDef]) -> Vec<OpenRouterToolDef> {
        map_function_tools(tools, |t| OpenRouterToolDef {
            kind: "function".to_string(),
            function: OpenRouterFunction {
                name: t.name,
                description: t.description,
                parameters: t.parameters,
            },
        })
    }

    /// Convert core messages to OpenAI-compatible JSON format.
    fn convert_messages(
        messages: &[Message],
        system_prompt: Option<&str>,
    ) -> Vec<serde_json::Value> {
        convert_messages_openai_compatible(messages, system_prompt)
    }

    fn extract_text(response: &OpenRouterChatResponse) -> String {
        response
            .choices
            .iter()
            .filter_map(|choice| choice.message.content.as_deref())
            .next()
            .unwrap_or_default()
            .to_string()
    }

    /// Build stream events from the response, including tool calls if present.
    fn success_events(response: &OpenRouterChatResponse) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let text = Self::extract_text(response);
        if !text.is_empty() {
            events.push(StreamEvent::TextDelta { text });
        }

        if let Some(tool_calls) = response
            .choices
            .first()
            .and_then(|c| c.message.tool_calls.as_ref())
        {
            append_tool_call_events(
                &mut events,
                tool_calls.iter().map(|tc| {
                    (
                        tc.id.clone(),
                        tc.function.name.clone(),
                        tc.function.arguments.clone(),
                    )
                }),
            );
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
impl Engine for OpenRouterEngine {
    fn id(&self) -> &str {
        "openrouter"
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
            provider: "openrouter".to_string(),
            display_name: self.model.clone(),
            context_window: self.context_window(),
            supports_tools: self.supports_tool_use(),
            supports_streaming: self.supports_streaming(),
        }]
    }

    async fn run(
        &self,
        messages: &[Message],
        tools: &[ToolDef],
        context: &EngineContext,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>> {
        let request = OpenRouterChatRequest {
            model: self.model.clone(),
            messages: Self::convert_messages(messages, context.system_prompt.as_deref()),
            stream: false,
            max_tokens: self.max_output_tokens,
            tools: Self::convert_tools(tools),
        };

        if request.messages.is_empty() {
            return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                message: "OpenRouter request has no messages".to_string(),
            }])));
        }

        debug!(
            model = %self.model,
            tools = tools.len(),
            "Sending request to OpenRouter"
        );

        let mut req = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url))
            .header("Authorization", format!("Bearer {}", self.api_key));

        // Optional attribution headers for OpenRouter leaderboard.
        if let Some(ref referer) = self.referer {
            req = req.header("HTTP-Referer", referer);
        }
        if let Some(ref title) = self.title {
            req = req.header("X-OpenRouter-Title", title);
        }

        let response = req.json(&request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!(%status, %body, "OpenRouter request failed");
            return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                message: format!("OpenRouter error {}: {}", status, body),
            }])));
        }

        let parsed: OpenRouterChatResponse = response.json().await?;
        Ok(Box::pin(stream::iter(Self::success_events(&parsed))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tengu_core::types::message::Role;
    use tengu_core::types::ToolCall;

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
        let converted = OpenRouterEngine::convert_messages(
            &[
                msg(Role::System, "sys"),
                msg(Role::User, "u"),
                msg(Role::Assistant, "a"),
            ],
            None,
        );

        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0]["role"], "system");
        assert_eq!(converted[1]["role"], "user");
        assert_eq!(converted[2]["role"], "assistant");
    }

    #[test]
    fn convert_messages_prepends_context_system_prompt() {
        let converted = OpenRouterEngine::convert_messages(
            &[msg(Role::User, "u"), msg(Role::Assistant, "a")],
            Some("runtime system"),
        );

        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0]["role"], "system");
        assert_eq!(converted[0]["content"], "runtime system");
    }

    #[test]
    fn convert_messages_handles_tool_results() {
        let tool_msg = Message {
            role: Role::Tool,
            content: "tool output".to_string(),
            tool_call_id: Some("call_123".to_string()),
            tool_calls: None,
        };
        let converted = OpenRouterEngine::convert_messages(&[tool_msg], None);

        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["role"], "tool");
        assert_eq!(converted[0]["content"], "tool output");
        assert_eq!(converted[0]["tool_call_id"], "call_123");
    }

    #[test]
    fn convert_messages_handles_assistant_with_tool_calls() {
        let assistant_msg = Message {
            role: Role::Assistant,
            content: String::new(),
            tool_call_id: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_abc".to_string(),
                name: "read_file".to_string(),
                arguments: serde_json::json!({"path": "test.txt"}),
            }]),
        };
        let converted = OpenRouterEngine::convert_messages(&[assistant_msg], None);

        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0]["role"], "assistant");
        assert!(converted[0]["tool_calls"].is_array());
        assert_eq!(
            converted[0]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
    }

    #[test]
    fn success_events_with_tool_calls() {
        let response = OpenRouterChatResponse {
            choices: vec![OpenRouterChoice {
                message: OpenRouterOutputMessage {
                    content: None,
                    tool_calls: Some(vec![OpenRouterResponseToolCall {
                        id: "call_1".to_string(),
                        kind: Some("function".to_string()),
                        function: OpenRouterResponseFunction {
                            name: "read_file".to_string(),
                            arguments: r#"{"path":"test.txt"}"#.to_string(),
                        },
                    }]),
                },
            }],
            usage: Some(OpenRouterUsage {
                prompt_tokens: 50,
                completion_tokens: 10,
            }),
        };

        let events = OpenRouterEngine::success_events(&response);
        assert_eq!(events.len(), 5);
        assert!(
            matches!(events[0], StreamEvent::ToolCallStart { ref name, .. } if name == "read_file")
        );
        assert!(matches!(events[1], StreamEvent::ToolCallDelta { .. }));
        assert!(matches!(events[2], StreamEvent::ToolCallEnd { .. }));
        assert!(matches!(events[3], StreamEvent::Usage { .. }));
        assert!(matches!(events[4], StreamEvent::Done));
    }

    #[test]
    fn success_events_order_text_usage_done() {
        let response = OpenRouterChatResponse {
            choices: vec![OpenRouterChoice {
                message: OpenRouterOutputMessage {
                    content: Some("hello".to_string()),
                    tool_calls: None,
                },
            }],
            usage: Some(OpenRouterUsage {
                prompt_tokens: 11,
                completion_tokens: 6,
            }),
        };

        let events = OpenRouterEngine::success_events(&response);
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
    fn convert_tools_maps_correctly() {
        let tools = vec![ToolDef {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            parameters: serde_json::json!({"type": "object"}),
            policy: None,
        }];
        let converted = OpenRouterEngine::convert_tools(&tools);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].kind, "function");
        assert_eq!(converted[0].function.name, "read_file");
    }

    #[test]
    fn diagnostics_report_endpoint_model_and_transport() {
        let engine = OpenRouterEngine::new(
            "https://openrouter.ai/api/",
            "anthropic/claude-sonnet-4",
            "test-key",
            None,
            None,
        );
        let diagnostics = engine.diagnostics();

        assert_eq!(diagnostics.engine_id, "openrouter");
        assert_eq!(
            diagnostics.configured_model.as_deref(),
            Some("anthropic/claude-sonnet-4")
        );
        assert_eq!(
            diagnostics.endpoint.as_deref(),
            Some("https://openrouter.ai/api")
        );
        assert_eq!(diagnostics.transport.as_deref(), Some("http-json"));
    }

    #[test]
    fn context_window_detects_claude_models() {
        assert_eq!(
            OpenRouterEngine::resolve_context_window_tokens("anthropic/claude-sonnet-4", None),
            200_000
        );
        assert_eq!(
            OpenRouterEngine::resolve_context_window_tokens("anthropic/claude-3.5-sonnet", None),
            200_000
        );
    }

    #[test]
    fn context_window_detects_openai_models() {
        assert_eq!(
            OpenRouterEngine::resolve_context_window_tokens("openai/gpt-4o", None),
            128_000
        );
        assert_eq!(
            OpenRouterEngine::resolve_context_window_tokens("openai/gpt-4.1", None),
            1_047_576
        );
    }

    #[test]
    fn context_window_detects_gemini_models() {
        assert_eq!(
            OpenRouterEngine::resolve_context_window_tokens("google/gemini-2.5-pro", None),
            1_000_000
        );
    }

    #[test]
    fn context_window_override_takes_precedence() {
        assert_eq!(
            OpenRouterEngine::resolve_context_window_tokens(
                "anthropic/claude-sonnet-4",
                Some(65_536)
            ),
            65_536
        );
    }

    #[test]
    fn max_output_tokens_uses_fallback_and_override() {
        assert_eq!(
            OpenRouterEngine::resolve_max_output_tokens(128_000, None),
            8_192
        );
        assert_eq!(
            OpenRouterEngine::resolve_max_output_tokens(8_192, None),
            1_024
        );
        assert_eq!(
            OpenRouterEngine::resolve_max_output_tokens(128_000, Some(12_000)),
            12_000
        );
    }

    #[test]
    fn engine_id_is_openrouter() {
        let engine = OpenRouterEngine::new(
            "https://openrouter.ai/api",
            "anthropic/claude-sonnet-4",
            "key",
            None,
            None,
        );
        assert_eq!(engine.id(), "openrouter");
        assert!(engine.supports_tool_use());
        assert!(!engine.manages_own_workspace());
    }
}
