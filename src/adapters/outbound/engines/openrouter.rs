//! OpenRouter engine — chat completions with tool calls (wire layer:
//! `engines/openai_compat.rs`), via the egress `llm_api_client`.
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
use std::pin::Pin;
use tracing::{debug, warn};

use super::openai_compat::{post_chat, ChatRequest, ChatResponse};
use crate::domain::message::{Message, ModelInfo, StreamEvent, ToolDef};
use crate::domain::secrets::SecretRegistry;
use crate::ports::engine::{Engine, EngineContext, EngineDiagnostics};

/// Names this engine in error text and logs.
const PROVIDER: &str = "OpenRouter";

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

    /// `raw` redacted and cut at a char boundary for a log line.
    fn log_excerpt(&self, raw: &str) -> String {
        let redacted = self.secrets.redact(raw);
        crate::domain::token::truncate_with_suffix(&redacted, EMPTY_TURN_LOG_CHARS, "…")
    }

    /// One POST of `request` with this engine's auth + attribution headers
    /// (`openai_compat::post_chat`).
    async fn post(
        &self,
        request: &ChatRequest,
    ) -> anyhow::Result<std::result::Result<(String, ChatResponse), Vec<StreamEvent>>> {
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
        Ok(post_chat(req, request, PROVIDER).await?)
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
        let request = ChatRequest::new(
            &self.model,
            messages,
            context.system_prompt.as_deref(),
            tools,
            // Sent only when configured; unset -> the model/provider default.
            self.max_output_tokens_override,
        );

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
            if attempt == 1 && parsed.is_failed_turn() {
                // A malformed call repeats when asked the same way: say so
                // (this request only — the caller's history is untouched).
                if parsed.error_finish() {
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
                earlier = parsed.usage();
                continue;
            }
            if attempt == 2 && parsed.is_failed_turn() {
                warn!(
                    model = %self.model,
                    raw = %self.log_excerpt(&raw_body),
                    "OpenRouter turn failed again after the retry"
                );
            }
            return Ok(Box::pin(stream::iter(parsed.events(earlier, PROVIDER))));
        }
        unreachable!("the second attempt always returns")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::Role;
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
                ..
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
    fn budget_reservation_reflects_configured_cap() {
        let capped =
            OpenRouterEngine::new("http://x", "m", "k", 1_000_000, 600, Some(8192)).unwrap();
        assert_eq!(capped.max_output_tokens_per_turn(), 8192);

        // Unset falls back to the context-scaled reservation estimate.
        let unset = OpenRouterEngine::new("http://x", "m", "k", 1_000_000, 600, None).unwrap();
        assert_eq!(unset.max_output_tokens_per_turn(), 16_384);
    }
}
