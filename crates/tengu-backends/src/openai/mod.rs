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

use crate::tooling::{
    append_tool_call_events, convert_messages_openai_compatible, map_function_tools,
};
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

// ── Request types ──────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct OpenAIChatRequest {
    model: String,
    messages: Vec<serde_json::Value>,
    stream: bool,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<OpenAIToolDef>,
}

#[derive(Debug, Serialize)]
struct OpenAIToolDef {
    #[serde(rename = "type")]
    kind: String,
    function: OpenAIFunction,
}

#[derive(Debug, Serialize)]
struct OpenAIFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

// ── Response types ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct OpenAIChatResponse {
    #[serde(default)]
    choices: Vec<OpenAIChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Debug, Deserialize)]
struct OpenAIChoice {
    message: OpenAIOutputMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAIOutputMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OpenAIResponseToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIResponseToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: Option<String>,
    function: OpenAIResponseFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenAIResponseFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

impl OpenAIEngine {
    /// Create a new OpenAI engine using API endpoint, model, and API key.
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

    fn default_max_output_tokens(context_window_tokens: usize) -> u32 {
        ((context_window_tokens / 8).clamp(512, 8_192)) as u32
    }

    /// Convert tengu ToolDef array to OpenAI function-calling format.
    fn convert_tools(tools: &[ToolDef]) -> Vec<OpenAIToolDef> {
        map_function_tools(tools, |t| OpenAIToolDef {
            kind: "function".to_string(),
            function: OpenAIFunction {
                name: t.name,
                description: t.description,
                parameters: t.parameters,
            },
        })
    }

    /// Convert core messages to OpenAI JSON format.
    ///
    /// Handles regular messages, tool result messages, and assistant messages
    /// that contain tool_calls (needed for the multi-turn tool loop).
    fn convert_messages(
        messages: &[Message],
        system_prompt: Option<&str>,
    ) -> Vec<serde_json::Value> {
        convert_messages_openai_compatible(messages, system_prompt)
    }

    fn extract_text(response: &OpenAIChatResponse) -> String {
        response
            .choices
            .iter()
            .filter_map(|choice| choice.message.content.as_deref())
            .next()
            .unwrap_or_default()
            .to_string()
    }

    /// Build stream events from the response, including tool calls if present.
    fn success_events(response: &OpenAIChatResponse) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let text = Self::extract_text(response);
        if !text.is_empty() {
            events.push(StreamEvent::TextDelta { text });
        }

        // Emit tool call events if the model requested tool use
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
        tools: &[ToolDef],
        context: &EngineContext,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>> {
        let request = OpenAIChatRequest {
            model: self.model.clone(),
            messages: Self::convert_messages(messages, context.system_prompt.as_deref()),
            stream: false,
            max_tokens: self.max_output_tokens,
            tools: Self::convert_tools(tools),
        };

        if request.messages.is_empty() {
            return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                message: "OpenAI request has no messages".to_string(),
            }])));
        }

        debug!(model = %self.model, tools = tools.len(), "Sending request to OpenAI");

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
        let converted = OpenAIEngine::convert_messages(
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
        let converted = OpenAIEngine::convert_messages(
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
        let converted = OpenAIEngine::convert_messages(&[tool_msg], None);

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
        let converted = OpenAIEngine::convert_messages(&[assistant_msg], None);

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
        let response = OpenAIChatResponse {
            choices: vec![OpenAIChoice {
                message: OpenAIOutputMessage {
                    content: None,
                    tool_calls: Some(vec![OpenAIResponseToolCall {
                        id: "call_1".to_string(),
                        kind: Some("function".to_string()),
                        function: OpenAIResponseFunction {
                            name: "read_file".to_string(),
                            arguments: r#"{"path":"test.txt"}"#.to_string(),
                        },
                    }]),
                },
            }],
            usage: Some(OpenAIUsage {
                prompt_tokens: 50,
                completion_tokens: 10,
            }),
        };

        let events = OpenAIEngine::success_events(&response);
        // Should have: ToolCallStart, ToolCallDelta, ToolCallEnd, Usage, Done
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
        let response = OpenAIChatResponse {
            choices: vec![OpenAIChoice {
                message: OpenAIOutputMessage {
                    content: Some("hello".to_string()),
                    tool_calls: None,
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
    fn convert_tools_maps_correctly() {
        let tools = vec![ToolDef {
            name: "read_file".to_string(),
            description: "Read a file".to_string(),
            parameters: serde_json::json!({"type": "object"}),
            policy: None,
        }];
        let converted = OpenAIEngine::convert_tools(&tools);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].kind, "function");
        assert_eq!(converted[0].function.name, "read_file");
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
