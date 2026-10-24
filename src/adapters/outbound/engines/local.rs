//! Local engine (`engine = "local"`) — a model served on this machine by an
//! OpenAI-compatible server: Unsloth (`unsloth run`, default
//! `http://127.0.0.1:8888`), Ollama (`:11434`), llama.cpp `llama-server`
//! (`:8080`), vLLM (`:8000`), LM Studio (`:1234`).
//!
//! | Aspect | Behaviour |
//! |---|---|
//! | Endpoint | `POST {base_url}/v1/chat/completions`, OpenAI `tools` (wire layer: `engines/openai_compat.rs`); a `base_url` ending in `/v1` is accepted (the suffix is dropped) |
//! | Auth | `Authorization: Bearer $<api_key_env>` when that env is set (Unsloth `sk-unsloth-…`); none otherwise |
//! | Network | Direct connection, never via the `[egress]` proxy — the server is on this host, not the internet |
//! | Config | `[agents.<n>.local] base_url`, `api_key_env` (`config::AgentLocalConfig`) |
//! | Context fit | One tool result ≤ 1/8 of `limits.context_window` (`tool_result_char_cap`; a typed row above it compacts, one that fits arrives whole); a system message repeating the system prompt is sent once |

use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::stream;
use futures::Stream;
use std::pin::Pin;
use tracing::{debug, error};

use super::openai_compat::{post_chat, ChatRequest};
use crate::config::AgentLocalConfig;
use crate::domain::message::{Message, ModelInfo, StreamEvent, ToolDef};
use crate::domain::token::tool_result_char_budget;
use crate::ports::engine::{Engine, EngineContext, EngineDiagnostics};

/// Names this engine in error text and logs.
const PROVIDER: &str = "Local LLM";

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
            base_url: server_root(&local.base_url),
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

    fn tool_result_char_cap(&self) -> Option<usize> {
        Some(tool_result_char_budget(self.context_window_tokens))
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
        let request = ChatRequest::new(
            &self.model,
            messages,
            context.system_prompt.as_deref(),
            tools,
            self.max_output_tokens_override,
        );
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
        match post_chat(req, &request, PROVIDER).await {
            Ok(Ok((_, parsed))) => Ok(Box::pin(stream::iter(parsed.events((0, 0), PROVIDER)))),
            Ok(Err(events)) => Ok(Box::pin(stream::iter(events))),
            Err(e) => {
                let e = format!("{:#}", anyhow::Error::from(e));
                error!(base_url = %self.base_url, error = %e, "Local LLM unreachable");
                Ok(Self::error(format!(
                    "Local LLM at {} unreachable (is the server running?): {e}",
                    self.base_url
                )))
            }
        }
    }
}

/// `base_url` without trailing `/` or `/v1` — the engine appends
/// `/v1/chat/completions` (Ollama documents its root as `…:11434/v1`).
fn server_root(base_url: &str) -> String {
    let url = base_url.trim_end_matches('/');
    url.strip_suffix("/v1")
        .unwrap_or(url)
        .trim_end_matches('/')
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::Role;
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

    /// Ollama's documented root `…:11434/v1` must not become `/v1/v1/…`.
    #[tokio::test]
    async fn base_url_with_v1_suffix_is_accepted() {
        let (base, server) = serve_once(r#"{"choices":[{"message":{"content":"ok"}}]}"#).await;
        let engine = LocalEngine::new(
            &cfg(format!("{base}v1/"), ""),
            "gemma4:latest",
            16_384,
            60,
            None,
        )
        .unwrap();
        let events = run_hi(&engine).await;
        let raw = server.await.unwrap();
        assert!(raw.starts_with("POST /v1/chat/completions "), "{raw}");
        assert!(matches!(&events[0], StreamEvent::TextDelta { text } if text == "ok"));
        assert_eq!(
            server_root("http://127.0.0.1:11434"),
            "http://127.0.0.1:11434"
        );
        assert_eq!(
            server_root("http://127.0.0.1:8000/api/v1"),
            "http://127.0.0.1:8000/api"
        );
    }

    #[test]
    fn tool_result_cap_is_an_eighth_of_the_window() {
        let engine = LocalEngine::new(
            &cfg("http://127.0.0.1:11434".into(), ""),
            "m",
            16_384,
            60,
            None,
        )
        .unwrap();
        assert_eq!(engine.tool_result_char_cap(), Some(8_192));
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
