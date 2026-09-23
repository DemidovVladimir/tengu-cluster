//! Local engine (`engine = "local"`) — a model served on this machine by an
//! OpenAI-compatible server: Unsloth (`unsloth run`, default
//! `http://127.0.0.1:8888`), Ollama (`:11434`), llama.cpp `llama-server`
//! (`:8080`), vLLM (`:8000`), LM Studio (`:1234`).
//!
//! | Aspect | Behaviour |
//! |---|---|
//! | Endpoint | `POST {base_url}/v1/chat/completions`, OpenAI `tools` |
//! | Auth | `Authorization: Bearer $<api_key_env>` when that env is set (Unsloth `sk-unsloth-…`); none otherwise |
//! | Network | Direct connection, never via the `[egress]` proxy — the server is on this host, not the internet |
//! | Config | `[agents.<n>.local] base_url`, `api_key_env` (`config::AgentLocalConfig`) |

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tracing::{debug, error};

use crate::config::AgentLocalConfig;
use crate::domain::message::{Message, ModelInfo, Role, StreamEvent, ToolDef};
use crate::ports::engine::{Engine, EngineContext, EngineDiagnostics};

pub struct LocalEngine {
    base_url: String,
    model: String,
    /// `None` = keyless server (Ollama, llama.cpp).
    api_key: Option<String>,
    context_window_tokens: usize,
    /// `[limits] max_output_tokens_per_turn`; `None` omits `max_tokens`.
    max_output_tokens_override: Option<u32>,
    client: reqwest::Client,
}

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<serde_json::Value>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: OutputMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OutputMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Deserialize)]
struct ToolCall {
    id: String,
    function: ToolCallFunction,
}

#[derive(Debug, Deserialize)]
struct ToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

impl LocalEngine {
    pub fn new(
        local: &AgentLocalConfig,
        model: &str,
        context_window: usize,
        request_timeout_secs: u64,
        max_output_tokens_override: Option<u32>,
    ) -> Result<Self> {
        let api_key = std::env::var(&local.api_key_env)
            .ok()
            .filter(|k| !k.trim().is_empty());
        let client = reqwest::Client::builder()
            // Direct: ignore HTTP(S)_PROXY env and the egress proxy.
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(30))
            // Non-streaming — the timeout covers the whole (slow, local) generation.
            .timeout(std::time::Duration::from_secs(request_timeout_secs))
            .build()
            .context("build local LLM http client")?;
        Ok(Self {
            base_url: local.base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key,
            context_window_tokens: context_window,
            max_output_tokens_override,
            client,
        })
    }

    fn error(message: String) -> Pin<Box<dyn Stream<Item = StreamEvent> + Send>> {
        Box::pin(stream::iter(vec![StreamEvent::Error { message }]))
    }

    fn success_events(response: &ChatResponse) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let choice = response.choices.first();
        let text = choice
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();
        let truncated = choice
            .and_then(|c| c.finish_reason.as_deref())
            .is_some_and(|r| matches!(r, "length" | "max_tokens"));
        if truncated {
            tracing::warn!("Local model output truncated (finish_reason=length)");
        }
        // Same `[OUTPUT_TRUNCATED]` marker the tool loop auto-continues on.
        if truncated && !text.is_empty() {
            events.push(StreamEvent::TextDelta {
                text: format!("{text}\n\n[OUTPUT_TRUNCATED]"),
            });
        } else if !truncated && !text.is_empty() {
            events.push(StreamEvent::TextDelta { text });
        }
        for tc in choice
            .and_then(|c| c.message.tool_calls.as_ref())
            .into_iter()
            .flatten()
        {
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
impl Engine for LocalEngine {
    fn id(&self) -> &str {
        "local"
    }

    fn context_window(&self) -> usize {
        self.context_window_tokens
    }

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
            provider: "local".to_string(),
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
    ) -> Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>> {
        let request = ChatRequest {
            model: self.model.clone(),
            messages: convert_messages(messages, context.system_prompt.as_deref()),
            stream: false,
            max_tokens: self.max_output_tokens_override,
            tools: tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect(),
        };
        if request.messages.is_empty() {
            return Ok(Self::error("Local LLM request has no messages".into()));
        }
        debug!(model = %self.model, tools = tools.len(), "Sending request to local LLM");

        let mut req = self
            .client
            .post(format!("{}/v1/chat/completions", self.base_url));
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let response = match req.json(&request).send().await {
            Ok(r) => r,
            Err(e) => {
                let e = format!("{:#}", anyhow::Error::from(e));
                error!(base_url = %self.base_url, error = %e, "Local LLM unreachable");
                return Ok(Self::error(format!(
                    "Local LLM at {} unreachable (is the server running?): {e}",
                    self.base_url
                )));
            }
        };
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!(%status, %body, "Local LLM request failed");
            return Ok(Self::error(format!("Local LLM error {status}: {body}")));
        }
        let raw_body = match response.text().await {
            Ok(body) => body,
            Err(e) => {
                let e = format!("{:#}", anyhow::Error::from(e));
                return Ok(Self::error(format!(
                    "Local LLM response body read failed: {e}"
                )));
            }
        };
        match serde_json::from_str::<ChatResponse>(&raw_body) {
            Ok(parsed) => Ok(Box::pin(stream::iter(Self::success_events(&parsed)))),
            Err(e) => {
                error!(model = %self.model, error = %e, "Failed to parse local LLM response");
                Ok(Self::error(format!(
                    "Failed to parse local LLM response: {e}"
                )))
            }
        }
    }
}

/// Runtime messages → OpenAI chat-completions message array.
fn convert_messages(messages: &[Message], system_prompt: Option<&str>) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    if let Some(prompt) = system_prompt.filter(|s| !s.trim().is_empty()) {
        out.push(serde_json::json!({ "role": "system", "content": prompt }));
    }
    for m in messages {
        let msg = match m.role {
            Role::Tool => serde_json::json!({
                "role": "tool",
                "content": m.content,
                "tool_call_id": m.tool_call_id,
            }),
            Role::Assistant if m.tool_calls.is_some() => {
                let calls: Vec<serde_json::Value> = m
                    .tool_calls
                    .iter()
                    .flatten()
                    .map(|tc| {
                        serde_json::json!({
                            "id": tc.id,
                            "type": "function",
                            "function": { "name": tc.name, "arguments": tc.arguments.to_string() }
                        })
                    })
                    .collect();
                let mut msg = serde_json::json!({ "role": "assistant", "tool_calls": calls });
                if !m.content.is_empty() {
                    msg["content"] = serde_json::json!(m.content);
                }
                msg
            }
            _ => {
                let role = match m.role {
                    Role::System => "system",
                    Role::User => "user",
                    _ => "assistant",
                };
                serde_json::json!({ "role": role, "content": m.content })
            }
        };
        out.push(msg);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One-shot fake server; resolves to the raw request text.
    async fn serve_once(body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            let n = sock.read(&mut buf).await.unwrap();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });
        (format!("http://{addr}/"), handle)
    }

    async fn run_hi(engine: &LocalEngine) -> Vec<StreamEvent> {
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
        engine
            .run(&messages, &[], &context)
            .await
            .unwrap()
            .collect()
            .await
    }

    fn cfg(base_url: String, api_key_env: &str) -> AgentLocalConfig {
        AgentLocalConfig {
            base_url,
            api_key_env: api_key_env.to_string(),
        }
    }

    /// Keyless: no Authorization header, model slug verbatim, tool calls parsed.
    #[tokio::test]
    async fn keyless_request_and_tool_call() {
        let (base, server) = serve_once(
            r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"http_request","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        )
        .await;
        let engine = LocalEngine::new(
            &cfg(base, ""),
            "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_XL",
            32_000,
            60,
            None,
        )
        .unwrap();
        let events = run_hi(&engine).await;
        let raw = server.await.unwrap();
        assert!(raw.starts_with("POST /v1/chat/completions "), "{raw}");
        assert!(
            !raw.to_ascii_lowercase().contains("authorization:"),
            "{raw}"
        );
        assert!(raw.contains(r#""model":"unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_XL""#));
        assert!(events.iter().any(
            |e| matches!(e, StreamEvent::ToolCallStart { name, .. } if name == "http_request")
        ));
    }

    #[tokio::test]
    async fn bearer_sent_when_key_env_set() {
        // Unique name; PATH is always set, so reuse it as a stand-in key.
        let key = std::env::var("PATH").unwrap();
        let (base, server) = serve_once(r#"{"choices":[{"message":{"content":"ok"}}]}"#).await;
        let engine = LocalEngine::new(&cfg(base, "PATH"), "m", 8_000, 60, None).unwrap();
        let events = run_hi(&engine).await;
        let raw = server.await.unwrap();
        assert!(
            raw.contains(&format!("authorization: Bearer {key}")),
            "{raw}"
        );
        assert!(matches!(&events[0], StreamEvent::TextDelta { text } if text == "ok"));
    }

    #[tokio::test]
    async fn unreachable_server_is_an_error_event() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let engine = LocalEngine::new(&cfg(base, ""), "m", 8_000, 5, None).unwrap();
        let events = run_hi(&engine).await;
        assert!(
            matches!(&events[0], StreamEvent::Error { message } if message.contains("is the server running?"))
        );
    }
}
