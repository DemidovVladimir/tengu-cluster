use async_trait::async_trait;
use futures::stream;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tracing::{debug, error};

use tengu_core::types::{Message, ModelInfo, StreamEvent, ToolDef};
use tengu_core::{Engine, EngineContext};

/// Engine backed by a local Ollama instance.
pub struct OllamaEngine {
    base_url: String,
    model: String,
    client: reqwest::Client,
}

#[derive(Debug, Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    stream: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    message: Option<OllamaResponseMessage>,
    done: Option<bool>,
    #[serde(default)]
    eval_count: u32,
    #[serde(default)]
    prompt_eval_count: u32,
}

#[derive(Debug, Deserialize)]
struct OllamaResponseMessage {
    content: String,
}

impl OllamaEngine {
    /// Create an Ollama engine using a base URL and default model id.
    pub fn new(base_url: &str, model: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            client: reqwest::Client::new(),
        }
    }

    fn convert_messages(messages: &[Message]) -> Vec<OllamaMessage> {
        messages
            .iter()
            .map(|m| OllamaMessage {
                role: match m.role {
                    tengu_core::types::message::Role::System => "system",
                    tengu_core::types::message::Role::User => "user",
                    tengu_core::types::message::Role::Assistant => "assistant",
                    tengu_core::types::message::Role::Tool => "tool",
                }
                .to_string(),
                content: m.content.clone(),
            })
            .collect()
    }
}

#[async_trait]
impl Engine for OllamaEngine {
    fn id(&self) -> &str {
        "ollama"
    }

    fn context_window(&self) -> usize {
        // TODO(epic-backend-capabilities): Resolve per-model context windows via
        // model metadata or cached `/api/tags` inspection instead of fixed default.
        8192
    }

    fn supports_tool_use(&self) -> bool {
        // TODO(epic-backend-capabilities): Detect tool-use support per model.
        false // Depends on specific model; conservative default
    }

    fn manages_own_workspace(&self) -> bool {
        false
    }

    fn available_models(&self) -> Vec<ModelInfo> {
        vec![ModelInfo {
            id: self.model.clone(),
            provider: "ollama".to_string(),
            display_name: self.model.clone(),
            context_window: self.context_window(),
            supports_tools: false,
            supports_streaming: true,
        }]
    }

    async fn run(
        &self,
        messages: &[Message],
        _tools: &[ToolDef],
        context: &EngineContext,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>> {
        let mut ollama_messages = Vec::new();

        // Prepend system prompt if provided
        if let Some(ref system) = context.system_prompt {
            ollama_messages.push(OllamaMessage {
                role: "system".to_string(),
                content: system.clone(),
            });
        }

        ollama_messages.extend(Self::convert_messages(messages));

        let request = OllamaChatRequest {
            model: self.model.clone(),
            messages: ollama_messages,
            // TODO(epic-backend-ollama-streaming): Enable true streaming and map
            // chunked responses into incremental `StreamEvent::TextDelta`.
            stream: false, // Non-streaming fallback
        };

        debug!(model = %self.model, "Sending request to Ollama");

        let response = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!(%status, %body, "Ollama request failed");
            return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                message: format!("Ollama error {}: {}", status, body),
            }])));
        }

        let chat_response: OllamaChatResponse = response.json().await?;

        let mut events = Vec::new();

        // TODO(epic-backend-usage-accounting): Improve usage accuracy and include
        // cached/prompt breakdown where backend provides it.
        if let Some(msg) = chat_response.message {
            events.push(StreamEvent::TextDelta { text: msg.content });
        }

        events.push(StreamEvent::Usage {
            input_tokens: chat_response.prompt_eval_count,
            output_tokens: chat_response.eval_count,
        });

        events.push(StreamEvent::Done);

        Ok(Box::pin(stream::iter(events)))
    }
}
