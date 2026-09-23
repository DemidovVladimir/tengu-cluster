//! OpenRouter engine — OpenAI-compatible chat completions with streaming
//! and tool calls, via the egress `llm_api_client`.

use anyhow::Result;
use async_trait::async_trait;
use futures::stream;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tracing::{debug, error};

use crate::domain::message::{Message, ModelInfo, Role, StreamEvent, ToolDef};
use crate::ports::engine::{Engine, EngineContext, EngineDiagnostics};

// ---------------------------------------------------------------------------
// OpenRouter engine
// ---------------------------------------------------------------------------

pub struct OpenRouterEngine {
    base_url: String,
    model: String,
    api_key: String,
    context_window_tokens: usize,
    /// Per-agent output cap from `[limits] max_output_tokens_per_turn`. `None`
    /// omits `max_tokens` from the request entirely, letting the model/provider
    /// use its own default (no synthetic ceiling).
    max_output_tokens_override: Option<u32>,
    referer: Option<String>,
    title: Option<String>,
    client: reqwest::Client,
}

#[derive(Debug, Serialize)]
struct OpenRouterChatRequest {
    model: String,
    messages: Vec<serde_json::Value>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
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
    #[serde(default)]
    finish_reason: Option<String>,
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
    pub fn new(
        base_url: &str,
        model: &str,
        api_key: &str,
        context_window: usize,
        request_timeout_secs: u64,
        max_output_tokens_override: Option<u32>,
    ) -> Result<Self> {
        let context_window_tokens = context_window;
        // Proxied iff `[egress] route_llm_api`.
        let client = crate::adapters::outbound::egress::policy().llm_api_client(
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                // Total timeout covers the body read. With `stream: false`
                // OpenRouter sends 200 immediately and holds the body until
                // generation ends, so long reasoning turns need headroom.
                // Sourced from `[limits] request_timeout_secs`.
                .timeout(std::time::Duration::from_secs(request_timeout_secs)),
        )?;
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key: api_key.to_string(),
            context_window_tokens,
            max_output_tokens_override,
            referer: std::env::var("OPENROUTER_REFERER").ok(),
            title: std::env::var("OPENROUTER_TITLE").ok(),
            client,
        })
    }

    fn convert_tools(tools: &[ToolDef]) -> Vec<OpenRouterToolDef> {
        tools
            .iter()
            .map(|t| OpenRouterToolDef {
                kind: "function".to_string(),
                function: OpenRouterFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                },
            })
            .collect()
    }

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

    fn success_events(response: &OpenRouterChatResponse) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let text = Self::extract_text(response);

        // Log finish_reason so we can detect output-token truncation.
        let truncated = if let Some(choice) = response.choices.first() {
            if let Some(ref reason) = choice.finish_reason {
                tracing::info!(finish_reason = %reason, text_len = text.len(), "OpenRouter finish reason");
                matches!(reason.as_str(), "length" | "max_tokens")
            } else {
                false
            }
        } else {
            false
        };
        if truncated {
            tracing::warn!(
                "Model output truncated by output token limit (finish_reason=length/max_tokens)"
            );
        }

        // Signal truncation via a special marker in the text stream so the tool
        // loop can detect it and auto-continue.
        if truncated && text.is_empty() {
            // Truncated with no text at all — nothing useful to send.
        } else if truncated {
            events.push(StreamEvent::TextDelta {
                text: format!("{}\n\n[OUTPUT_TRUNCATED]", text),
            });
        } else if !text.is_empty() {
            events.push(StreamEvent::TextDelta { text });
        }

        if let Some(tool_calls) = response
            .choices
            .first()
            .and_then(|c| c.message.tool_calls.as_ref())
        {
            for tc in tool_calls {
                events.push(StreamEvent::ToolCallStart {
                    id: tc.id.clone(),
                    name: tc.function.name.clone(),
                });
                events.push(StreamEvent::ToolCallDelta {
                    id: tc.id.clone(),
                    arguments_delta: tc.function.arguments.clone(),
                });
                events.push(StreamEvent::ToolCallEnd { id: tc.id.clone() });
            }
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

    /// Budget-reservation value only (prompt budget, TUI display). When a cap
    /// is configured we reserve exactly that; when unset we fall back to the
    /// trait's context-scaled estimate. The request itself omits `max_tokens`
    /// unless configured (see `run`), so this is not a synthetic send-ceiling.
    fn max_output_tokens_per_turn(&self) -> u32 {
        match self.max_output_tokens_override {
            Some(cap) => cap.clamp(1, self.context_window_tokens.max(1) as u32),
            None => ((self.context_window() / 8).clamp(256, 16_384)) as u32,
        }
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
            // Sent only when configured; unset -> the model/provider default.
            max_tokens: self.max_output_tokens_override,
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

        let raw_body = match response.text().await {
            Ok(body) => body,
            Err(e) => {
                // reqwest's Display is always "error decoding response body";
                // the real cause (timeout, reset) lives in the source chain,
                // which anyhow's `{:#}` prints.
                let e = format!("{:#}", anyhow::Error::from(e));
                error!(model = %self.model, error = %e, "Failed to read OpenRouter response body");
                return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                    message: format!("OpenRouter response body read failed: {}", e),
                }])));
            }
        };
        debug!(
            model = %self.model,
            body_len = raw_body.len(),
            body_preview = %if raw_body.len() > 500 { &raw_body[..500] } else { &raw_body },
            "OpenRouter raw response"
        );
        let parsed: OpenRouterChatResponse = match serde_json::from_str(&raw_body) {
            Ok(p) => p,
            Err(e) => {
                error!(
                    model = %self.model,
                    error = %e,
                    body_preview = %if raw_body.len() > 300 { &raw_body[..300] } else { &raw_body },
                    "Failed to parse OpenRouter response"
                );
                return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                    message: format!("Failed to parse OpenRouter response: {}", e),
                }])));
            }
        };
        Ok(Box::pin(stream::iter(Self::success_events(&parsed))))
    }
}

// ---------------------------------------------------------------------------
// Message conversion helpers (OpenAI-compatible format)
// ---------------------------------------------------------------------------

/// Convert runtime messages to an OpenAI-compatible JSON message array.
fn convert_messages_openai_compatible(
    messages: &[Message],
    system_prompt: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut result: Vec<serde_json::Value> = Vec::new();

    if let Some(prompt) = system_prompt.filter(|s| !s.trim().is_empty()) {
        result.push(serde_json::json!({
            "role": "system",
            "content": prompt
        }));
    }

    for m in messages {
        match m.role {
            Role::Tool => {
                let mut msg = serde_json::json!({
                    "role": "tool",
                    "content": m.content
                });
                if let Some(ref id) = m.tool_call_id {
                    msg["tool_call_id"] = serde_json::json!(id);
                }
                result.push(msg);
            }
            Role::Assistant if m.tool_calls.is_some() => {
                let tool_calls: Vec<serde_json::Value> = m
                    .tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|tc| {
                                serde_json::json!({
                                    "id": tc.id,
                                    "type": "function",
                                    "function": {
                                        "name": tc.name,
                                        "arguments": tc.arguments.to_string()
                                    }
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let mut msg = serde_json::json!({
                    "role": "assistant",
                    "tool_calls": tool_calls
                });
                if !m.content.is_empty() {
                    msg["content"] = serde_json::json!(m.content);
                }
                result.push(msg);
            }
            _ => {
                let role = match m.role {
                    Role::System => "system",
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => unreachable!(),
                };
                result.push(serde_json::json!({
                    "role": role,
                    "content": m.content
                }));
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Server sends 200 + headers, then drops mid-body. The surfaced error
    /// must carry reqwest's source chain, not just "error decoding response body".
    #[tokio::test]
    async fn body_read_failure_surfaces_source_chain() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 8192];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{")
                .await;
        });

        let engine =
            OpenRouterEngine::new(&format!("http://{addr}"), "m", "k", 8_000, 600, None).unwrap();
        let messages = vec![Message {
            role: Role::User,
            content: "hi".to_string(),
            tool_call_id: None,
            tool_calls: None,
        }];
        let context = EngineContext {
            workspace: None,
            system_prompt: None,
            bridge_tools: None,
            max_tool_rounds: None,
            max_mcp_result_chars: None,
            mcp_servers: Vec::new(),
        };
        let events: Vec<StreamEvent> = engine
            .run(&messages, &[], &context)
            .await
            .unwrap()
            .collect()
            .await;

        let Some(StreamEvent::Error { message }) = events.first() else {
            panic!("expected an Error event");
        };
        assert!(
            message.starts_with(
                "OpenRouter response body read failed: error decoding response body: "
            ),
            "source chain missing: {message}"
        );
    }

    #[test]
    fn max_tokens_omitted_when_unset_sent_when_configured() {
        let mk = |req: &OpenRouterChatRequest| serde_json::to_value(req).unwrap();

        let unset = OpenRouterChatRequest {
            model: "m".into(),
            messages: vec![],
            stream: false,
            max_tokens: None,
            tools: vec![],
        };
        assert!(
            mk(&unset).get("max_tokens").is_none(),
            "max_tokens must be omitted when no cap is configured"
        );

        let set = OpenRouterChatRequest {
            max_tokens: Some(8192),
            ..unset
        };
        assert_eq!(mk(&set)["max_tokens"], serde_json::json!(8192));
    }

    #[test]
    fn budget_reservation_reflects_configured_cap() {
        let capped =
            OpenRouterEngine::new("http://x", "m", "k", 1_000_000, 600, Some(8192)).unwrap();
        assert_eq!(capped.max_output_tokens_per_turn(), 8192);

        // Unset falls back to the context-scaled reservation estimate.
        let unset = OpenRouterEngine::new("http://x", "m", "k", 1_000_000, 600, None).unwrap();
        assert_eq!(unset.max_output_tokens_per_turn(), 16_384);
    }
}
