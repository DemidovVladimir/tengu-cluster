//! Ollama backend implementation.
//!
//! Potential use case:
//! Run a local model server (`OLLAMA_HOST`) and stream/collect responses in CLI mode.

use async_trait::async_trait;
use futures::stream;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error};

use tengu_core::types::{Message, ModelInfo, StreamEvent, ToolDef};
use tengu_core::{Engine, EngineContext, EngineDiagnostics};

/// Engine implementation backed by a local Ollama instance.
pub struct OllamaEngine {
    base_url: String,
    model: String,
    client: reqwest::Client,
}

#[derive(Debug, Serialize)]
struct OllamaChatRequest {
    /// Ollama model identifier (for example: `llama3.2`).
    model: String,
    /// Ordered conversation messages sent to `/api/chat`.
    messages: Vec<OllamaMessage>,
    /// Enable NDJSON incremental streaming mode.
    stream: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaMessage {
    /// Ollama role label (`system`/`user`/`assistant`/`tool`).
    role: String,
    /// Message text payload.
    content: String,
}

#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    /// Optional response chunk text in streaming frames.
    #[serde(default)]
    message: Option<OllamaResponseMessage>,
    /// Final-frame marker from Ollama stream.
    #[serde(default)]
    done: bool,
    /// Output token count reported by Ollama (usually terminal frame).
    #[serde(default)]
    eval_count: u32,
    /// Input token count reported by Ollama (usually terminal frame).
    #[serde(default)]
    prompt_eval_count: u32,
}

#[derive(Debug, Deserialize)]
struct OllamaResponseMessage {
    /// Incremental text chunk emitted by the model.
    content: String,
}

/// Rolling stream state accumulated while consuming Ollama NDJSON frames.
#[derive(Debug, Default)]
struct OllamaStreamState {
    /// Last known input token count from stream frames.
    input_tokens: u32,
    /// Last known output token count from stream frames.
    output_tokens: u32,
    /// Whether any processed frame had `done = true`.
    saw_done: bool,
}

impl OllamaStreamState {
    /// Merge usage/done flags from one parsed response frame.
    fn absorb(&mut self, frame: &OllamaChatResponse) {
        if frame.prompt_eval_count > 0 || frame.eval_count > 0 {
            self.input_tokens = frame.prompt_eval_count;
            self.output_tokens = frame.eval_count;
        }
        self.saw_done |= frame.done;
    }
}

impl OllamaEngine {
    /// Create a new Ollama engine from base URL and default model.
    pub fn new(base_url: &str, model: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            client: reqwest::Client::new(),
        }
    }

    /// Convert core `Message` values into Ollama request message format.
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

    /// Build Ollama-compatible message list from system prompt + conversation.
    fn assemble_ollama_messages(
        system_prompt: Option<&str>,
        messages: &[Message],
    ) -> Vec<OllamaMessage> {
        system_prompt
            .iter()
            .map(|system| OllamaMessage {
                role: "system".to_string(),
                content: (*system).to_string(),
            })
            .chain(Self::convert_messages(messages))
            .collect()
    }

    /// Normalize one Ollama stream line into JSON payload text.
    ///
    /// Ollama usually returns NDJSON lines, but this also tolerates `data: ...`
    /// framing to be robust if transport wrappers are introduced.
    fn normalize_stream_payload(line: &str) -> Option<&str> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return None;
        }
        Some(trimmed.strip_prefix("data:").unwrap_or(trimmed).trim())
    }

    /// Extract one complete line from a rolling stream buffer.
    fn take_next_line(buffer: &mut String) -> Option<String> {
        let idx = buffer.find('\n')?;
        let line = buffer[..idx].to_string();
        buffer.drain(..=idx);
        Some(line)
    }

    /// Send a stream error event and convert it into an early-return sentinel.
    fn send_stream_error(
        tx: &UnboundedSender<StreamEvent>,
        message: impl Into<String>,
    ) -> Result<(), ()> {
        let _ = tx.send(StreamEvent::Error {
            message: message.into(),
        });
        Err(())
    }

    /// Parse one payload frame and emit `TextDelta`/usage updates.
    fn process_payload_frame(
        payload: &str,
        tx: &UnboundedSender<StreamEvent>,
        state: &mut OllamaStreamState,
    ) -> Result<(), ()> {
        let parsed = serde_json::from_str::<OllamaChatResponse>(payload).map_err(|err| {
            Self::send_stream_error(tx, format!("Ollama stream parse error: {}", err)).err();
        });
        let Ok(parsed) = parsed else {
            return Err(());
        };

        parsed
            .message
            .as_ref()
            .map(|message| message.content.as_str())
            .filter(|content| !content.is_empty())
            .into_iter()
            .for_each(|text| {
                let _ = tx.send(StreamEvent::TextDelta {
                    text: text.to_string(),
                });
            });

        state.absorb(&parsed);
        Ok(())
    }

    /// Consume all complete lines currently buffered and process normalized payload frames.
    fn process_complete_buffer_lines(
        buffer: &mut String,
        tx: &UnboundedSender<StreamEvent>,
        state: &mut OllamaStreamState,
    ) -> Result<(), ()> {
        std::iter::from_fn(|| Self::take_next_line(buffer))
            .filter_map(|line| Self::normalize_stream_payload(&line).map(ToString::to_string))
            .try_for_each(|payload| Self::process_payload_frame(&payload, tx, state))
    }

    /// Process final tail payload (if any) after byte stream ends.
    fn process_tail_payload(
        buffer: &str,
        tx: &UnboundedSender<StreamEvent>,
        state: &mut OllamaStreamState,
    ) -> Result<(), ()> {
        match Self::normalize_stream_payload(buffer) {
            Some(payload) => Self::process_payload_frame(payload, tx, state),
            None => Ok(()),
        }
    }

    /// Emit terminal success frames in stable order: `Usage` then `Done`.
    fn emit_success_terminal_events(tx: &UnboundedSender<StreamEvent>, state: &OllamaStreamState) {
        let _ = tx.send(StreamEvent::Usage {
            input_tokens: state.input_tokens,
            output_tokens: state.output_tokens,
        });
        let _ = tx.send(StreamEvent::Done);
    }
}

#[async_trait]
impl Engine for OllamaEngine {
    fn id(&self) -> &str {
        "ollama"
    }

    fn context_window(&self) -> usize {
        8192
    }

    fn supports_tool_use(&self) -> bool {
        false // Depends on specific model; conservative default
    }

    fn manages_own_workspace(&self) -> bool {
        false
    }

    fn supports_streaming(&self) -> bool {
        true
    }

    fn diagnostics(&self) -> EngineDiagnostics {
        EngineDiagnostics {
            engine_id: self.id().to_string(),
            configured_model: Some(self.model.clone()),
            endpoint: Some(self.base_url.clone()),
            transport: Some("http-ndjson".to_string()),
            capabilities: self.capabilities(),
        }
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
        // 1) Build one `/api/chat` request with system prompt + turn history.
        let request = OllamaChatRequest {
            model: self.model.clone(),
            messages: Self::assemble_ollama_messages(context.system_prompt.as_deref(), messages),
            stream: true,
        };

        debug!(model = %self.model, "Sending request to Ollama");

        let response = self
            .client
            .post(format!("{}/api/chat", self.base_url))
            .json(&request)
            .send()
            .await?;

        // 2) Surface HTTP-level failures as one terminal `Error` event.
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            error!(%status, %body, "Ollama request failed");
            return Ok(Box::pin(stream::iter(vec![StreamEvent::Error {
                message: format!("Ollama error {}: {}", status, body),
            }])));
        }

        // 3) Stream NDJSON bytes on a background task and bridge them to `StreamEvent`s.
        let mut bytes_stream = response.bytes_stream();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<StreamEvent>();

        tokio::spawn(async move {
            let mut buffer = String::new();
            let mut state = OllamaStreamState::default();

            while let Some(chunk_res) = bytes_stream.next().await {
                let chunk = match chunk_res {
                    Ok(c) => c,
                    Err(err) => {
                        let _ = Self::send_stream_error(
                            &tx,
                            format!("Ollama stream read error: {err}"),
                        );
                        return;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&chunk));
                if Self::process_complete_buffer_lines(&mut buffer, &tx, &mut state).is_err() {
                    return;
                }
            }

            if Self::process_tail_payload(&buffer, &tx, &mut state).is_err() {
                return;
            }

            // 4) Always emit terminal usage + done events, even with zero usage counters.
            Self::emit_success_terminal_events(&tx, &state);

            if !state.saw_done {
                debug!("Ollama stream ended without explicit done=true frame");
            }
        });

        let event_stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        });
        // 5) Return transport stream consumed by runtime loop.
        Ok(Box::pin(event_stream))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::UnboundedReceiver;

    /// Drain currently buffered stream events from a test receiver.
    fn drain_events(rx: &mut UnboundedReceiver<StreamEvent>) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    #[test]
    fn normalize_stream_payload_handles_plain_and_data_prefix() {
        assert_eq!(
            OllamaEngine::normalize_stream_payload(r#"{"done":false}"#),
            Some(r#"{"done":false}"#)
        );
        assert_eq!(
            OllamaEngine::normalize_stream_payload(r#"data: {"done":true}"#),
            Some(r#"{"done":true}"#)
        );
        assert_eq!(OllamaEngine::normalize_stream_payload("   "), None);
    }

    #[test]
    fn take_next_line_splits_buffer_incrementally() {
        let mut buffer = "a\nb\nc".to_string();
        assert_eq!(
            OllamaEngine::take_next_line(&mut buffer),
            Some("a".to_string())
        );
        assert_eq!(
            OllamaEngine::take_next_line(&mut buffer),
            Some("b".to_string())
        );
        assert_eq!(OllamaEngine::take_next_line(&mut buffer), None);
        assert_eq!(buffer, "c");
    }

    #[test]
    fn diagnostics_report_endpoint_model_and_transport() {
        let engine = OllamaEngine::new("http://localhost:11434/", "llama3.2");
        let diagnostics = engine.diagnostics();

        assert_eq!(diagnostics.engine_id, "ollama");
        assert_eq!(diagnostics.configured_model.as_deref(), Some("llama3.2"));
        assert_eq!(
            diagnostics.endpoint.as_deref(),
            Some("http://localhost:11434")
        );
        assert_eq!(diagnostics.transport.as_deref(), Some("http-ndjson"));
        assert!(diagnostics.capabilities.supports_streaming);
    }

    #[test]
    fn stream_fixture_orders_text_then_usage_then_done_on_success() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = OllamaStreamState::default();

        assert!(OllamaEngine::process_payload_frame(
            r#"{"message":{"content":"hello"},"done":false}"#,
            &tx,
            &mut state
        )
        .is_ok());
        assert!(OllamaEngine::process_payload_frame(
            r#"{"message":{"content":" world"},"done":true,"prompt_eval_count":42,"eval_count":7}"#,
            &tx,
            &mut state
        )
        .is_ok());

        OllamaEngine::emit_success_terminal_events(&tx, &state);
        drop(tx);
        let events = drain_events(&mut rx);

        assert_eq!(events.len(), 4);
        assert!(matches!(events[0], StreamEvent::TextDelta { ref text } if text == "hello"));
        assert!(matches!(events[1], StreamEvent::TextDelta { ref text } if text == " world"));
        assert!(matches!(
            events[2],
            StreamEvent::Usage {
                input_tokens: 42,
                output_tokens: 7
            }
        ));
        assert!(matches!(events[3], StreamEvent::Done));
    }

    #[test]
    fn stream_fixture_parse_error_emits_terminal_error_only() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut state = OllamaStreamState::default();

        assert!(OllamaEngine::process_payload_frame("{not-json", &tx, &mut state).is_err());
        drop(tx);
        let events = drain_events(&mut rx);

        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], StreamEvent::Error { .. }));
    }
}
