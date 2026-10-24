//! OpenAI chat-completions wire layer (non-streaming), shared by the
//! `openrouter` and `local` engines: the request, the response, message +
//! tool conversion, one POST, a response → `StreamEvent`s. Each engine keeps
//! its client, auth headers and policy (OpenRouter's one retry).
//!
//! | Piece | Rule |
//! |---|---|
//! | Messages | the context system prompt first; a system message equal to it is skipped (`run-agent`, `tengu tool turn`, the doctor and webhooks send it both ways — paid twice otherwise); a blank prompt is none; a tool result carries its `tool_call_id`; an assistant turn with calls sends them as `function` calls, its text only when non-empty |
//! | Tools | `{"type": "function", "function": {name, description, parameters}}` |
//! | `max_tokens` | sent only when configured (`[limits] max_output_tokens_per_turn`) |
//! | POST | `{base}/v1/chat/completions` on the caller's builder; a non-2xx is `"<provider> error <status>: <body>"`, a body read failure keeps reqwest's source chain, an unparsable body is `"Failed to parse <provider> response: …"` — each an `Error` event; a send failure goes back to the caller |
//! | Events | text (`finish_reason` `length` / `max_tokens` ⇒ `[OUTPUT_TRUNCATED]` appended — the tool loop continues), tool calls (start / delta / end), `Usage` (+ a retried attempt's tokens), `Done`; an upstream `error` object in a 200 or an `error` finish is an `Error` event, its text never an answer |

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{debug, error, info, warn};

use crate::domain::message::{Message, Role, StreamEvent, ToolDef};

/// One chat-completions request.
#[derive(Debug, Serialize)]
pub(super) struct ChatRequest {
    pub model: String,
    pub messages: Vec<Value>,
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
}

impl ChatRequest {
    /// A non-streaming request (module table).
    pub(super) fn new(
        model: &str,
        messages: &[Message],
        system_prompt: Option<&str>,
        tools: &[ToolDef],
        max_tokens: Option<u32>,
    ) -> Self {
        Self {
            model: model.to_string(),
            messages: convert_messages(messages, system_prompt),
            stream: false,
            max_tokens,
            tools: tools.iter().map(tool_spec).collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct ChatResponse {
    #[serde(default)]
    pub choices: Vec<Choice>,
    #[serde(default)]
    pub usage: Option<Usage>,
    /// An upstream failure reported inside a 200 body (OpenRouter).
    #[serde(default)]
    pub error: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub(super) struct Choice {
    pub message: OutputMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
    /// The provider's own reason (Gemini: `STOP`, `MALFORMED_FUNCTION_CALL`, …).
    #[serde(default)]
    pub native_finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct OutputMessage {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ResponseToolCall>>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ResponseToolCall {
    pub id: String,
    pub function: ResponseFunction,
}

#[derive(Debug, Deserialize)]
pub(super) struct ResponseFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}

impl ChatResponse {
    /// `(prompt, completion)` tokens reported.
    pub(super) fn usage(&self) -> (u32, u32) {
        self.usage
            .as_ref()
            .map_or((0, 0), |u| (u.prompt_tokens, u.completion_tokens))
    }

    /// `finish_reason = "error"` without a tool call: the provider failed
    /// mid-turn; any text is not an answer.
    pub(super) fn error_finish(&self) -> bool {
        self.choices.first().is_some_and(|c| {
            c.finish_reason.as_deref() == Some("error")
                && c.message.tool_calls.as_ref().map_or(true, |t| t.is_empty())
        })
    }

    /// Nothing to act on: no tool call and either no text (not cut by
    /// length / filtered), an `error` finish, or only an upstream `error`.
    pub(super) fn is_failed_turn(&self) -> bool {
        let choice = self.choices.first();
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
        self.error_finish() || (!deterministic && !has_text)
    }

    /// The turn's events (module table: Events); `earlier` = the tokens of
    /// an attempt retried before this one, added to `Usage`.
    pub(super) fn events(&self, earlier: (u32, u32), provider: &str) -> Vec<StreamEvent> {
        let mut events = Vec::new();
        let choice = self.choices.first();
        let mut text = choice
            .and_then(|c| c.message.content.clone())
            .unwrap_or_default();

        if self.choices.is_empty() {
            if let Some(err) = &self.error {
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map_or_else(|| err.to_string(), str::to_string);
                events.push(StreamEvent::Error {
                    message: format!("{provider} upstream error: {message}"),
                });
            }
        }
        if self.error_finish() {
            let native = choice
                .and_then(|c| c.native_finish_reason.as_deref())
                .unwrap_or("-");
            events.push(StreamEvent::Error {
                message: format!(
                    "{provider}: the provider ended the turn with an error (native_finish_reason {native})"
                ),
            });
            text.clear();
        }

        let finish = choice.and_then(|c| c.finish_reason.as_deref());
        if let Some(reason) = finish {
            info!(
                provider,
                finish_reason = %reason,
                native_finish_reason = choice
                    .and_then(|c| c.native_finish_reason.as_deref())
                    .unwrap_or("-"),
                text_len = text.len(),
                "finish reason"
            );
        }
        let truncated = matches!(finish, Some("length" | "max_tokens"));
        if truncated {
            warn!(provider, "model output truncated by the output token limit");
        }
        // The marker the tool loop auto-continues on.
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

        if self.usage.is_some() || earlier != (0, 0) {
            let (input, output) = self.usage();
            events.push(StreamEvent::Usage {
                input_tokens: input.saturating_add(earlier.0),
                output_tokens: output.saturating_add(earlier.1),
                cached_input_tokens: 0,
            });
        }
        events.push(StreamEvent::Done);
        events
    }
}

/// One POST of `body` on `req` (the caller's builder: URL + auth): the raw
/// body and its parse, or the error events the turn ends with (module
/// table: POST). A send failure is the caller's to word.
pub(super) async fn post_chat(
    req: reqwest::RequestBuilder,
    body: &ChatRequest,
    provider: &str,
) -> Result<Result<(String, ChatResponse), Vec<StreamEvent>>, reqwest::Error> {
    let response = req.json(body).send().await?;
    let fail = |message: String| Ok(Err(vec![StreamEvent::Error { message }]));
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        error!(provider, %status, body = %text, "chat completion request failed");
        return fail(format!("{provider} error {status}: {text}"));
    }
    let raw = match response.text().await {
        Ok(raw) => raw,
        Err(e) => {
            // reqwest's Display is always "error decoding response body";
            // the cause (timeout, reset) is in the chain `{:#}` prints.
            let e = format!("{:#}", anyhow::Error::from(e));
            error!(provider, model = %body.model, error = %e, "chat completion body read failed");
            return fail(format!("{provider} response body read failed: {e}"));
        }
    };
    let preview = |n: usize| {
        crate::domain::token::truncate_at_boundary(&raw, n)
            .map_or(raw.as_str(), |(prefix, _)| prefix)
            .to_string()
    };
    debug!(provider, model = %body.model, body_len = raw.len(), body_preview = %preview(500), "chat completion raw response");
    match serde_json::from_str::<ChatResponse>(&raw) {
        Ok(parsed) => Ok(Ok((raw, parsed))),
        Err(e) => {
            error!(provider, model = %body.model, error = %e, body_preview = %preview(300), "chat completion response does not parse");
            fail(format!("Failed to parse {provider} response: {e}"))
        }
    }
}

fn tool_spec(t: &ToolDef) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": t.name,
            "description": t.description,
            "parameters": t.parameters,
        }
    })
}

/// Runtime messages → the chat-completions message array (module table).
pub(super) fn convert_messages(messages: &[Message], system_prompt: Option<&str>) -> Vec<Value> {
    let mut out = Vec::new();
    let system_prompt = system_prompt.filter(|s| !s.trim().is_empty());
    if let Some(prompt) = system_prompt {
        out.push(json!({ "role": "system", "content": prompt }));
    }
    for m in messages {
        if matches!(m.role, Role::System) && Some(m.content.as_str()) == system_prompt {
            continue;
        }
        let msg = match m.role {
            Role::Tool => {
                let mut msg = json!({ "role": "tool", "content": m.content });
                if let Some(id) = &m.tool_call_id {
                    msg["tool_call_id"] = json!(id);
                }
                msg
            }
            Role::Assistant if m.tool_calls.is_some() => {
                let calls: Vec<Value> = m
                    .tool_calls
                    .iter()
                    .flatten()
                    .map(|tc| {
                        json!({
                            "id": tc.id,
                            "type": "function",
                            "function": { "name": tc.name, "arguments": tc.arguments.to_string() }
                        })
                    })
                    .collect();
                let mut msg = json!({ "role": "assistant", "tool_calls": calls });
                if !m.content.is_empty() {
                    msg["content"] = json!(m.content);
                }
                msg
            }
            Role::System => json!({ "role": "system", "content": m.content }),
            Role::User => json!({ "role": "user", "content": m.content }),
            Role::Assistant => json!({ "role": "assistant", "content": m.content }),
        };
        out.push(msg);
    }
    out
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

    /// A system message equal to the context's `system_prompt` goes out
    /// once; other system messages stay, and without a context prompt (or
    /// a blank one) every one does.
    #[test]
    fn system_prompt_is_sent_once() {
        let messages = vec![
            msg(Role::System, "SYS"),
            msg(Role::User, "goal"),
            msg(Role::System, "memory block"),
        ];
        let out = convert_messages(&messages, Some("SYS"));
        let roles: Vec<(&str, &str)> = out
            .iter()
            .map(|m| (m["role"].as_str().unwrap(), m["content"].as_str().unwrap()))
            .collect();
        assert_eq!(
            roles,
            vec![
                ("system", "SYS"),
                ("user", "goal"),
                ("system", "memory block")
            ]
        );
        assert_eq!(convert_messages(&messages, None).len(), 3);
        assert_eq!(
            convert_messages(&messages, Some("  ")).len(),
            3,
            "a blank context prompt is no prompt"
        );
    }

    #[test]
    fn max_tokens_only_when_configured_and_tools_as_functions() {
        let tools = [ToolDef::new("read_file", "d", json!({"type": "object"}))];
        let unset = ChatRequest::new("m", &[msg(Role::User, "hi")], None, &tools, None);
        let v = serde_json::to_value(&unset).unwrap();
        assert!(v.get("max_tokens").is_none());
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["name"], "read_file");
        let set = ChatRequest::new("m", &[], None, &[], Some(8192));
        let v = serde_json::to_value(&set).unwrap();
        assert_eq!(v["max_tokens"], json!(8192));
        assert!(v.get("tools").is_none(), "no tools, no key");
    }

    #[test]
    fn a_truncated_turn_is_marked_and_tool_calls_stream() {
        let r: ChatResponse = serde_json::from_str(
            r#"{"choices":[{"message":{"content":"part","tool_calls":[{"id":"c1","type":"function","function":{"name":"x","arguments":"{}"}}]},"finish_reason":"length"}],"usage":{"prompt_tokens":5,"completion_tokens":7}}"#,
        )
        .unwrap();
        let ev = r.events((1, 1), "Local LLM");
        assert!(
            matches!(&ev[0], StreamEvent::TextDelta { text } if text == "part\n\n[OUTPUT_TRUNCATED]")
        );
        assert!(matches!(&ev[1], StreamEvent::ToolCallStart { name, .. } if name == "x"));
        assert!(matches!(
            ev[4],
            StreamEvent::Usage {
                input_tokens: 6,
                output_tokens: 8,
                ..
            }
        ));
        assert!(matches!(ev.last(), Some(StreamEvent::Done)));
    }
}
