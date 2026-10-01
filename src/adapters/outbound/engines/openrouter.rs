//! OpenRouter engine — OpenAI-compatible chat completions with streaming
//! and tool calls, via the egress `llm_api_client`.
//!
//! | Turn | Handling |
//! |---|---|
//! | text and / or tool calls | events as returned; `finish_reason` + the provider's `native_finish_reason` logged at info |
//! | failed — no tool call and either no text (`finish_reason` not `length` / `max_tokens` / `content_filter`), `finish_reason = "error"` (whatever text came with it), or a 200 carrying only an `error` object | logged at warn with both finish reasons and the raw body (secrets redacted, ≤ 2 000 chars), then the request is **retried once** — after an `error` finish with a user line saying the malformed call was discarded (`MALFORMED_CALL_HINT`, that request only); the turn's `Usage` sums both attempts. A second empty answer is returned as is (`run-agent` fails the step, chat shows nothing); a second `error` finish or `error` object becomes `StreamEvent::Error` — never its text as an answer |
//!
//! Why retry: such a turn is a provider-side flake — Gemini's malformed or
//! unexpected function call (`MALFORMED_FUNCTION_CALL` came back as
//! `finish_reason = "error"` with the text `<ctrl46>`), an upstream error
//! inside a 200; 2 of 28 engine-matrix legs on `google/gemini-2.5-flash-lite`
//! (2026-09-30), 1 of 39 (2026-10-01). No tool ran, so the repeat has no side
//! effect, costs one prompt, and otherwise the whole step fails and is re-run
//! from scratch (or junk becomes the answer). Truncation and content
//! filtering are deterministic — never retried.

use anyhow::Result;
use async_trait::async_trait;
use futures::stream;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tracing::{debug, error, info, warn};

use crate::domain::message::{Message, ModelInfo, Role, StreamEvent, ToolDef};
use crate::domain::secrets::SecretRegistry;
use crate::ports::engine::{Engine, EngineContext, EngineDiagnostics};

/// Raw-body excerpt logged for an empty turn (after redaction).
const EMPTY_TURN_LOG_CHARS: usize = 2_000;

/// Appended to the retry of an `error` finish (module table).
const MALFORMED_CALL_HINT: &str = "Your previous reply ended in a malformed tool call and was \
discarded. Call the tool again with valid JSON arguments, or reply in plain text.";

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
    /// The process secrets + this engine's API key: redacts the raw body an
    /// empty turn logs.
    secrets: SecretRegistry,
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
    /// An upstream failure OpenRouter reports inside a 200 body.
    #[serde(default)]
    error: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterChoice {
    message: OpenRouterOutputMessage,
    #[serde(default)]
    finish_reason: Option<String>,
    /// The provider's own reason (Gemini: `STOP`, `MALFORMED_FUNCTION_CALL`, …).
    #[serde(default)]
    native_finish_reason: Option<String>,
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
        let mut secrets = crate::adapters::outbound::secrets::process_secret_registry(None);
        secrets.register(api_key.to_string());
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key: api_key.to_string(),
            context_window_tokens,
            max_output_tokens_override,
            referer: std::env::var("OPENROUTER_REFERER").ok(),
            title: std::env::var("OPENROUTER_TITLE").ok(),
            client,
            secrets,
        })
    }

    /// The turn produced nothing to act on (module table): no tool call and
    /// either no text (not cut by length / filtered), an `error` finish, or
    /// only an upstream `error` object.
    fn is_failed_turn(response: &OpenRouterChatResponse) -> bool {
        let choice = response.choices.first();
        let has_calls = choice
            .and_then(|c| c.message.tool_calls.as_ref())
            .is_some_and(|calls| !calls.is_empty());
        if has_calls {
            return false;
        }
        let finish = choice.and_then(|c| c.finish_reason.as_deref());
        let deterministic = matches!(finish, Some("length" | "max_tokens" | "content_filter"));
        let has_text = choice
            .and_then(|c| c.message.content.as_deref())
            .is_some_and(|t| !t.is_empty());
        Self::error_finish(response) || (!deterministic && !has_text)
    }

    /// `finish_reason = "error"` without a tool call: the provider failed
    /// mid-turn; any text is not an answer.
    fn error_finish(response: &OpenRouterChatResponse) -> bool {
        response.choices.first().is_some_and(|c| {
            c.finish_reason.as_deref() == Some("error")
                && c.message.tool_calls.as_ref().map_or(true, |t| t.is_empty())
        })
    }

    /// `(prompt, completion)` tokens the response reports.
    fn usage_of(response: &OpenRouterChatResponse) -> (u32, u32) {
        response
            .usage
            .as_ref()
            .map_or((0, 0), |u| (u.prompt_tokens, u.completion_tokens))
    }

    /// `raw` redacted and cut at a char boundary for a log line.
    fn log_excerpt(&self, raw: &str) -> String {
        let redacted = self.secrets.redact(raw);
        match crate::domain::token::truncate_at_boundary(&redacted, EMPTY_TURN_LOG_CHARS) {
            Some((prefix, _)) => format!("{prefix}…"),
            None => redacted,
        }
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

    /// Events of one parsed response; `earlier` = the tokens of an empty
    /// attempt retried before it, added to this turn's `Usage`.
    fn success_events(response: &OpenRouterChatResponse, earlier: (u32, u32)) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let mut text = Self::extract_text(response);

        // An upstream failure inside a 200 (no choices): an error, not an
        // empty answer.
        if response.choices.is_empty() {
            if let Some(err) = &response.error {
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map_or_else(|| err.to_string(), str::to_string);
                events.push(StreamEvent::Error {
                    message: format!("OpenRouter upstream error: {message}"),
                });
            }
        }
        // An `error` finish: the provider failed mid-turn — its text (Gemini:
        // `<ctrl46>`) is no answer.
        if Self::error_finish(response) {
            let native = response
                .choices
                .first()
                .and_then(|c| c.native_finish_reason.as_deref())
                .unwrap_or("-");
            events.push(StreamEvent::Error {
                message: format!(
                    "OpenRouter: the provider ended the turn with an error (native_finish_reason {native})"
                ),
            });
            text.clear();
        }

        // Log finish_reason so we can detect output-token truncation.
        let truncated = if let Some(choice) = response.choices.first() {
            if let Some(ref reason) = choice.finish_reason {
                info!(
                    finish_reason = %reason,
                    native_finish_reason = choice.native_finish_reason.as_deref().unwrap_or("-"),
                    text_len = text.len(),
                    "OpenRouter finish reason"
                );
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

        if response.usage.is_some() || earlier != (0, 0) {
            let (input, output) = Self::usage_of(response);
            events.push(StreamEvent::Usage {
                input_tokens: input.saturating_add(earlier.0),
                output_tokens: output.saturating_add(earlier.1),
            });
        }
        events.push(StreamEvent::Done);
        events
    }

    /// One POST of `request`: the raw body and its parse, or the error
    /// events this turn ends with (HTTP status, body read, JSON parse).
    async fn post(
        &self,
        request: &OpenRouterChatRequest,
    ) -> anyhow::Result<std::result::Result<(String, OpenRouterChatResponse), Vec<StreamEvent>>>
    {
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

        let response = req.json(request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!(%status, %body, "OpenRouter request failed");
            return Ok(Err(vec![StreamEvent::Error {
                message: format!("OpenRouter error {}: {}", status, body),
            }]));
        }

        let raw_body = match response.text().await {
            Ok(body) => body,
            Err(e) => {
                // reqwest's Display is always "error decoding response body";
                // the real cause (timeout, reset) lives in the source chain,
                // which anyhow's `{:#}` prints.
                let e = format!("{:#}", anyhow::Error::from(e));
                error!(model = %self.model, error = %e, "Failed to read OpenRouter response body");
                return Ok(Err(vec![StreamEvent::Error {
                    message: format!("OpenRouter response body read failed: {}", e),
                }]));
            }
        };
        let preview = |n: usize| {
            crate::domain::token::truncate_at_boundary(&raw_body, n)
                .map_or(raw_body.as_str(), |(prefix, _)| prefix)
                .to_string()
        };
        debug!(
            model = %self.model,
            body_len = raw_body.len(),
            body_preview = %preview(500),
            "OpenRouter raw response"
        );
        match serde_json::from_str::<OpenRouterChatResponse>(&raw_body) {
            Ok(parsed) => Ok(Ok((raw_body, parsed))),
            Err(e) => {
                error!(
                    model = %self.model,
                    error = %e,
                    body_preview = %preview(300),
                    "Failed to parse OpenRouter response"
                );
                Ok(Err(vec![StreamEvent::Error {
                    message: format!("Failed to parse OpenRouter response: {}", e),
                }]))
            }
        }
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

        // A failed turn is retried once (module table).
        let mut request = request;
        let mut earlier = (0u32, 0u32);
        for attempt in 1..=2 {
            let (raw_body, parsed) = match self.post(&request).await? {
                Ok(ok) => ok,
                Err(events) => return Ok(Box::pin(stream::iter(events))),
            };
            if attempt == 1 && Self::is_failed_turn(&parsed) {
                // A malformed call repeats when asked the same way: say so
                // (this request only — the caller's history is untouched).
                if Self::error_finish(&parsed) {
                    request.messages.push(serde_json::json!({
                        "role": "user",
                        "content": MALFORMED_CALL_HINT,
                    }));
                }
                let choice = parsed.choices.first();
                warn!(
                    model = %self.model,
                    finish_reason = choice.and_then(|c| c.finish_reason.as_deref()).unwrap_or("-"),
                    native_finish_reason = choice
                        .and_then(|c| c.native_finish_reason.as_deref())
                        .unwrap_or("-"),
                    raw = %self.log_excerpt(&raw_body),
                    "OpenRouter turn failed (no tool call; no text or an error finish) — retrying once"
                );
                earlier = Self::usage_of(&parsed);
                continue;
            }
            if attempt == 2 && Self::is_failed_turn(&parsed) {
                warn!(
                    model = %self.model,
                    raw = %self.log_excerpt(&raw_body),
                    "OpenRouter turn failed again after the retry"
                );
            }
            return Ok(Box::pin(stream::iter(Self::success_events(
                &parsed, earlier,
            ))));
        }
        unreachable!("the second attempt always returns")
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

    /// Loopback server answering every request with the next of `replies`
    /// (the last repeats); `count` = requests served.
    async fn replying(
        replies: Vec<&'static str>,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let served = std::sync::Arc::clone(&log);
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 65_536];
                let read = sock.read(&mut buf).await.unwrap_or(0);
                let n = {
                    let mut log = served.lock().unwrap();
                    log.push(String::from_utf8_lossy(&buf[..read]).into_owned());
                    log.len() - 1
                };
                let body = replies[n.min(replies.len() - 1)];
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (format!("http://{addr}"), log, handle)
    }

    async fn turn(replies: Vec<&'static str>) -> (Vec<StreamEvent>, usize) {
        let (events, requests) = turn_logged(replies).await;
        (events, requests.len())
    }

    /// `turn` + the raw requests the server saw.
    async fn turn_logged(replies: Vec<&'static str>) -> (Vec<StreamEvent>, Vec<String>) {
        let (base, log, server) = replying(replies).await;
        let engine = OpenRouterEngine::new(&base, "m", "k", 8_000, 30, None).unwrap();
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
        let events = engine
            .run(&messages, &[], &context)
            .await
            .unwrap()
            .collect()
            .await;
        server.abort();
        let requests = log.lock().unwrap().clone();
        (events, requests)
    }

    const EMPTY: &str = r#"{"choices":[{"message":{"content":""},"finish_reason":"stop","native_finish_reason":"MALFORMED_FUNCTION_CALL"}],"usage":{"prompt_tokens":10,"completion_tokens":0}}"#;

    fn text_of(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn usage_of(events: &[StreamEvent]) -> Option<(u32, u32)> {
        events.iter().find_map(|e| match e {
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
            } => Some((*input_tokens, *output_tokens)),
            _ => None,
        })
    }

    /// An empty turn is asked again once; the turn's usage covers both.
    #[tokio::test]
    async fn empty_turn_is_retried_once_with_usage_summed() {
        let (events, n) = turn(vec![
            EMPTY,
            r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":3}}"#,
        ])
        .await;
        assert_eq!(n, 2);
        assert_eq!(text_of(&events), "ok");
        assert_eq!(usage_of(&events), Some((22, 3)));
        assert!(matches!(events.last(), Some(StreamEvent::Done)));
    }

    /// A second empty answer is the turn — no third request.
    #[tokio::test]
    async fn second_empty_turn_is_returned_as_is() {
        let (events, n) = turn(vec![EMPTY, EMPTY]).await;
        assert_eq!(n, 2);
        assert_eq!(text_of(&events), "");
        assert_eq!(usage_of(&events), Some((20, 0)));
        assert!(!events.iter().any(|e| matches!(
            e,
            StreamEvent::ToolCallStart { .. } | StreamEvent::Error { .. }
        )));
    }

    /// Filtering, truncation and tool calls are answers: never retried.
    #[tokio::test]
    async fn filtered_truncated_or_tool_call_turns_are_not_retried() {
        for reply in [
            r#"{"choices":[{"message":{"content":""},"finish_reason":"content_filter"}]}"#,
            r#"{"choices":[{"message":{"content":""},"finish_reason":"length"}]}"#,
            r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"read_file","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
        ] {
            let (_, n) = turn(vec![reply]).await;
            assert_eq!(n, 1, "{reply}");
        }
    }

    /// An `error` finish (Gemini `MALFORMED_FUNCTION_CALL`, live 2026-10-01)
    /// is retried; its junk text is never the answer — a repeat is an error.
    #[tokio::test]
    async fn error_finish_is_retried_and_its_text_never_answers() {
        const BAD: &str = r#"{"choices":[{"message":{"content":"<ctrl46>"},"finish_reason":"error","native_finish_reason":"MALFORMED_FUNCTION_CALL"}],"usage":{"prompt_tokens":10,"completion_tokens":2}}"#;
        let (events, requests) = turn_logged(vec![
            BAD,
            r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#,
        ])
        .await;
        assert_eq!((requests.len(), text_of(&events)), (2, "ok".to_string()));
        assert!(!requests[0].contains("malformed tool call"));
        assert!(
            requests[1].contains("malformed tool call"),
            "the retry says why: {}",
            requests[1]
        );
        // A plain empty turn is asked again unchanged.
        let (_, requests) = turn_logged(vec![EMPTY, EMPTY]).await;
        assert!(!requests[1].contains("malformed tool call"));

        let (events, n) = turn(vec![BAD, BAD]).await;
        assert_eq!(n, 2);
        assert_eq!(text_of(&events), "");
        assert!(
            matches!(&events[0], StreamEvent::Error { message } if message.contains("MALFORMED_FUNCTION_CALL")),
            "{events:?}"
        );
    }

    /// An upstream error inside a 200 is retried, then surfaces as an error.
    #[tokio::test]
    async fn upstream_error_in_a_200_is_retried_then_an_error() {
        const ERR: &str = r#"{"error":{"message":"Provider returned error","code":502}}"#;
        let (events, n) = turn(vec![ERR, ERR]).await;
        assert_eq!(n, 2);
        assert!(
            matches!(&events[0], StreamEvent::Error { message } if message.contains("Provider returned error")),
            "{events:?}"
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
