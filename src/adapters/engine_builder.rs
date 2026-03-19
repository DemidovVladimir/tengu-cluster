//! Engine builder: OpenRouter backend, factory functions, and tool-loop runtime.
//!
//! Consolidates engine construction, the OpenRouter API adapter, message/tool
//! conversion helpers, and the multi-turn tool-call execution loop.

use anyhow::Result;
use async_trait::async_trait;
use futures::stream;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::pin::Pin;
use tracing::{debug, error};

use crate::adapters::types::{Message, ModelInfo, Role, StreamEvent, ToolCall, ToolDef};
use crate::adapters::usage::{absorb_turn_usage_snapshot, apply_turn_usage_to_session_totals};
use crate::adapters::{Engine, EngineContext, EngineDiagnostics};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Sensible fallback when no override is provided and we cannot query the model.
const DEFAULT_CONTEXT_WINDOW: usize = 128_000;

/// Maximum number of tool-call round-trips before forcing a text response.
const MAX_TOOL_ROUNDS: usize = 30;

/// Maximum characters kept per successful tool result.
const MAX_TOOL_RESULT_CHARS: usize = 6_000;

/// Maximum characters kept for error tool results.
const MAX_ERROR_RESULT_CHARS: usize = 1_500;


/// Maximum seconds to wait for a single stream event before treating the stream as dead.
const STREAM_EVENT_TIMEOUT_SECS: u64 = 120;

/// Number of consecutive HTTP error results from the same tool before abort.
const MAX_CONSECUTIVE_TOOL_ERRORS: usize = 3;

// ---------------------------------------------------------------------------
// Factory — build engines from config
// ---------------------------------------------------------------------------

/// Build a lightweight engine for the planner/classifier from explicit engine + model strings.
pub(crate) fn build_planner_engine(_engine_type: &str, model: &str) -> Result<Box<dyn Engine>> {
    build_openrouter_engine(model, None)
}

/// Build configured engine instance for one agent.
pub(crate) fn build_engine(
    _agent_id: &str,
    agent_config: &crate::adapters::config::AgentConfig,
) -> Result<Box<dyn Engine>> {
    let context_window_override = agent_config
        .limits
        .context_window_override
        .map(|value| value.max(1) as usize);

    build_openrouter_engine(&agent_config.model, context_window_override)
}

fn build_openrouter_engine(
    model: &str,
    context_window_override: Option<usize>,
) -> Result<Box<dyn Engine>> {
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| anyhow::anyhow!("OPENROUTER_API_KEY is required"))?;
    let base_url = std::env::var("OPENROUTER_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api".to_string());
    Ok(Box::new(OpenRouterEngine::new(
        &base_url,
        model,
        &api_key,
        context_window_override,
    )))
}

// ---------------------------------------------------------------------------
// OpenRouter engine
// ---------------------------------------------------------------------------

pub struct OpenRouterEngine {
    base_url: String,
    model: String,
    api_key: String,
    context_window_tokens: usize,
    referer: Option<String>,
    title: Option<String>,
    client: reqwest::Client,
}

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
        context_window_override: Option<usize>,
    ) -> Self {
        let context_window_tokens = context_window_override
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW);
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key: api_key.to_string(),
            context_window_tokens,
            referer: std::env::var("OPENROUTER_REFERER").ok(),
            title: std::env::var("OPENROUTER_TITLE").ok(),
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(30))
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
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
            tracing::warn!("Model output truncated by output token limit (finish_reason=length/max_tokens)");
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
            max_tokens: self.max_output_tokens_per_turn(),
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

        let raw_body = response.text().await?;
        debug!(
            model = %self.model,
            body_len = raw_body.len(),
            body_preview = %if raw_body.len() > 500 { &raw_body[..500] } else { &raw_body },
            "OpenRouter raw response"
        );
        let parsed: OpenRouterChatResponse = serde_json::from_str(&raw_body).map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse OpenRouter response: {} — body: {}",
                e,
                if raw_body.len() > 300 {
                    &raw_body[..300]
                } else {
                    &raw_body
                }
            )
        })?;
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

// ---------------------------------------------------------------------------
// Engine runtime — tool-loop execution
// ---------------------------------------------------------------------------

/// Result of a single engine call including response text and token usage delta.
pub(crate) struct EngineResponse {
    pub text: String,
    pub input_tokens_delta: u32,
    pub output_tokens_delta: u32,
    /// Tool call outcomes collected during the turn (name, result).
    pub tool_outcomes: Vec<(String, String)>,
}

/// Trait for executing tool calls.
pub(crate) trait ToolExecutor: Send + Sync {
    fn execute(&self, call: &ToolCall) -> Result<String>;
}

/// Decorator that redacts registered secret values from all tool output.
pub(crate) struct SanitizedToolExecutor<'a> {
    inner: &'a dyn ToolExecutor,
    registry: &'a crate::adapters::secret_builder::SecretRegistry,
}

impl<'a> SanitizedToolExecutor<'a> {
    pub fn new(
        inner: &'a dyn ToolExecutor,
        registry: &'a crate::adapters::secret_builder::SecretRegistry,
    ) -> Self {
        Self { inner, registry }
    }
}

impl<'a> ToolExecutor for SanitizedToolExecutor<'a> {
    fn execute(&self, call: &ToolCall) -> Result<String> {
        let result = self.inner.execute(call)?;
        Ok(self.registry.redact(&result))
    }
}

/// Optional callback invoked after each tool execution.
pub(crate) type ToolResultObserver<'a> = &'a (dyn Fn(&ToolCall, &str) + Send + Sync);

/// Execute one or more engine rounds, handling tool calls automatically.
pub(crate) async fn collect_engine_response(
    engine: &dyn Engine,
    prompt_messages: &[Message],
    tools: &[ToolDef],
    context: &EngineContext,
    tool_executor: Option<&dyn ToolExecutor>,
    tool_observer: Option<ToolResultObserver<'_>>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    token_budget: Option<u32>,
) -> Result<EngineResponse> {
    let mut messages: Vec<Message> = prompt_messages.to_vec();
    let mut total_input_delta: u32 = 0;
    let mut total_output_delta: u32 = 0;
    let mut tool_outcomes: Vec<(String, String)> = Vec::new();
    let mut executed_tool_results: HashMap<String, String> = HashMap::new();
    let mut consecutive_errors: HashMap<String, usize> = HashMap::new();

    let is_cancelled = || cancel.map_or(false, |f| f.load(std::sync::atomic::Ordering::Relaxed));

    for round in 0..MAX_TOOL_ROUNDS {
        if is_cancelled() {
            debug!("Turn cancelled before round {}", round);
            return Ok(EngineResponse {
                text: String::new(),
                input_tokens_delta: total_input_delta,
                output_tokens_delta: total_output_delta,
                tool_outcomes,
            });
        }

        let (response_text, tool_calls, input_delta, output_delta) =
            run_single_engine_turn(engine, &messages, tools, context, cancel).await?;

        total_input_delta += input_delta;
        total_output_delta += output_delta;

        if let Some(budget) = token_budget {
            let total = total_input_delta + total_output_delta;
            if total > budget {
                tracing::warn!(
                    total_tokens = total,
                    budget,
                    pending_tool_calls = tool_calls.len(),
                    round,
                    "Token budget exceeded — stopping tool loop (increase max_tokens_per_flow to allow more rounds)"
                );
                return Ok(EngineResponse {
                    text: response_text,
                    input_tokens_delta: total_input_delta,
                    output_tokens_delta: total_output_delta,
                    tool_outcomes,
                });
            }
        }

        if tool_calls.is_empty() || tool_executor.is_none() || tools.is_empty() {
            // If the model was truncated mid-response, auto-continue.
            if tool_calls.is_empty()
                && !tools.is_empty()
                && round < MAX_TOOL_ROUNDS - 1
                && response_text.ends_with("[OUTPUT_TRUNCATED]")
            {
                let clean_text = response_text
                    .trim_end_matches("[OUTPUT_TRUNCATED]")
                    .trim()
                    .to_string();
                debug!(round, "Output truncated — auto-continuing");
                messages.push(Message {
                    role: Role::Assistant,
                    content: clean_text,
                    tool_call_id: None,
                    tool_calls: None,
                });
                messages.push(Message {
                    role: Role::User,
                    content: "Your output was truncated. Continue from where you left off, using tool calls to execute the remaining steps.".to_string(),
                    tool_call_id: None,
                    tool_calls: None,
                });
                continue;
            }
            return Ok(EngineResponse {
                text: response_text,
                input_tokens_delta: total_input_delta,
                output_tokens_delta: total_output_delta,
                tool_outcomes,
            });
        }

        let executor = tool_executor.unwrap();
        debug!(round, tool_count = tool_calls.len(), "Executing tool calls");

        let compact_cutoff = messages.len();

        messages.push(Message {
            role: Role::Assistant,
            content: response_text.clone(),
            tool_call_id: None,
            tool_calls: Some(tool_calls.clone()),
        });

        for tc in &tool_calls {
            if is_cancelled() {
                debug!("Turn cancelled before executing tool {}", tc.name);
                return Ok(EngineResponse {
                    text: String::new(),
                    input_tokens_delta: total_input_delta,
                    output_tokens_delta: total_output_delta,
                    tool_outcomes,
                });
            }
            let signature = tool_call_signature(tc);
            if let Some(previous_result) = executed_tool_results.get(&signature) {
                let is_idempotent = matches!(
                    tc.name.as_str(),
                    "list_directory" | "read_file" | "get_wallet_address" | "write_file"
                );
                if is_idempotent {
                    tracing::info!(
                        tool = %tc.name,
                        "Duplicate idempotent tool call — returning cached result"
                    );
                    let content = truncate_tool_result(previous_result, MAX_TOOL_RESULT_CHARS);
                    messages.push(Message {
                        role: Role::Tool,
                        content,
                        tool_call_id: Some(tc.id.clone()),
                        tool_calls: None,
                    });
                    continue;
                }
                let previous_was_error = previous_result.contains("\"status\":\"error\"")
                    || previous_result.contains("\"status\": \"error\"")
                    || previous_result.contains("\"errors\":")
                    || previous_result.starts_with("HTTP 4")
                    || previous_result.starts_with("HTTP 5");
                if !previous_was_error {
                    return Err(anyhow::anyhow!(
                        "Tool '{}' was requested multiple times with identical arguments in the same turn. Aborting to prevent unintended retries.\nPrevious result:\n{}",
                        tc.name,
                        truncate_tool_result(previous_result, MAX_ERROR_RESULT_CHARS)
                    ));
                }
                tracing::info!(
                    tool = %tc.name,
                    "Duplicate tool call after error — allowing retry"
                );
            }
            let result = match executor.execute(tc) {
                Ok(output) => output,
                Err(e) => {
                    let is_non_fatal = matches!(
                        tc.name.as_str(),
                        "read_file" | "list_directory" | "get_wallet_address"
                    );
                    if is_non_fatal {
                        tracing::warn!(
                            tool = %tc.name,
                            error = %e,
                            "Non-fatal tool error — returning to LLM as result"
                        );
                        format!("ERROR: {}", e)
                    } else {
                        tracing::error!(tool = %tc.name, error = %e, "Tool execution failed");
                        let assistant_context = if response_text.trim().is_empty() {
                            String::new()
                        } else {
                            format!(
                                "\nAssistant context before failure:\n{}",
                                truncate_tool_result(&response_text, MAX_ERROR_RESULT_CHARS)
                            )
                        };
                        return Err(anyhow::anyhow!(
                            "Tool '{}' failed: {}{}",
                            tc.name,
                            e,
                            assistant_context
                        ));
                    }
                }
            };
            executed_tool_results.insert(signature, result.clone());

            let is_error_result = result.starts_with("HTTP 4")
                || result.starts_with("HTTP 5")
                || result.contains("\"status\": \"error\"")
                || result.contains("\"status\":\"error\"");
            if is_error_result {
                let count = consecutive_errors.entry(tc.name.clone()).or_insert(0);
                *count += 1;
                if *count >= MAX_CONSECUTIVE_TOOL_ERRORS {
                    return Err(anyhow::anyhow!(
                        "Tool '{}' returned {} consecutive errors. Aborting to prevent token drain.\nLast error:\n{}",
                        tc.name,
                        count,
                        truncate_tool_result(&result, MAX_ERROR_RESULT_CHARS)
                    ));
                }
            } else {
                consecutive_errors.remove(&tc.name);
            }

            if let Some(observer) = &tool_observer {
                observer(tc, &result);
            }
            tool_outcomes.push((tc.name.clone(), result.clone()));
            let limit = if is_error_result {
                MAX_ERROR_RESULT_CHARS
            } else {
                MAX_TOOL_RESULT_CHARS
            };
            let content = truncate_tool_result(&result, limit);
            messages.push(Message {
                role: Role::Tool,
                content,
                tool_call_id: Some(tc.id.clone()),
                tool_calls: None,
            });
        }

        // Compact old tool results — the model already consumed them and
        // persisted values to shared_cache. We keep the message to satisfy
        // the API contract (every tool_call needs a result) but strip content.
        if round >= 1 {
            for msg in &mut messages[..compact_cutoff] {
                if matches!(msg.role, Role::Tool) && msg.content != "ok" {
                    msg.content = "ok".to_string();
                }
            }
        }
    }

    // Exhaust all rounds — force a text response.
    let (response_text, _, input_delta, output_delta) =
        run_single_engine_turn(engine, &messages, &[], context, cancel).await?;
    total_input_delta += input_delta;
    total_output_delta += output_delta;

    Ok(EngineResponse {
        text: response_text,
        input_tokens_delta: total_input_delta,
        output_tokens_delta: total_output_delta,
        tool_outcomes,
    })
}

/// Run a single engine turn and collect text, tool calls, and usage.
async fn run_single_engine_turn(
    engine: &dyn Engine,
    messages: &[Message],
    tools: &[ToolDef],
    context: &EngineContext,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<(String, Vec<ToolCall>, u32, u32)> {
    let mut stream = engine.run(messages, tools, context).await?;

    let mut response_text = String::new();
    let mut turn_usage_snapshot: Option<(u32, u32)> = None;
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut pending_tool_id: Option<String> = None;
    let mut pending_tool_name: Option<String> = None;
    let mut pending_tool_args = String::new();

    let is_cancelled = || cancel.map_or(false, |f| f.load(std::sync::atomic::Ordering::Relaxed));

    loop {
        if is_cancelled() {
            debug!("Stream cancelled by user");
            break;
        }
        let event = match tokio::time::timeout(
            std::time::Duration::from_secs(STREAM_EVENT_TIMEOUT_SECS),
            stream.next(),
        )
        .await
        {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "Engine stream timed out — no data for {}s",
                    STREAM_EVENT_TIMEOUT_SECS
                ));
            }
        };
        match event {
            StreamEvent::TextDelta { text } => {
                response_text.push_str(&text);
            }
            StreamEvent::ToolCallStart { id, name } => {
                flush_pending_tool_call(
                    &mut tool_calls,
                    &mut pending_tool_id,
                    &mut pending_tool_name,
                    &mut pending_tool_args,
                );
                pending_tool_id = Some(id);
                pending_tool_name = Some(name);
                pending_tool_args.clear();
            }
            StreamEvent::ToolCallDelta {
                arguments_delta, ..
            } => {
                pending_tool_args.push_str(&arguments_delta);
            }
            StreamEvent::ToolCallEnd { .. } => {
                flush_pending_tool_call(
                    &mut tool_calls,
                    &mut pending_tool_id,
                    &mut pending_tool_name,
                    &mut pending_tool_args,
                );
            }
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                absorb_turn_usage_snapshot(&mut turn_usage_snapshot, input_tokens, output_tokens);
            }
            StreamEvent::Error { message } => {
                if response_text.is_empty() {
                    return Err(anyhow::anyhow!("{}", message));
                }
            }
            _ => {}
        }
    }

    flush_pending_tool_call(
        &mut tool_calls,
        &mut pending_tool_id,
        &mut pending_tool_name,
        &mut pending_tool_args,
    );

    // Fallback: parse XML tool calls from response text.
    if tool_calls.is_empty() {
        let xml_calls = extract_xml_tool_calls(&response_text);
        if !xml_calls.is_empty() {
            tool_calls = xml_calls;
            response_text = strip_xml_tool_calls(&response_text);
        }
    }

    let mut input_delta: u32 = 0;
    let mut output_delta: u32 = 0;
    apply_turn_usage_to_session_totals(&mut input_delta, &mut output_delta, turn_usage_snapshot);

    Ok((response_text, tool_calls, input_delta, output_delta))
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn truncate_tool_result(result: &str, limit: usize) -> String {
    if result.len() <= limit {
        return result.to_string();
    }
    let mut end = limit;
    while end > 0 && !result.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n\n[truncated — showing {} of {} chars]",
        &result[..end],
        end,
        result.len()
    )
}

fn tool_call_signature(call: &ToolCall) -> String {
    format!("{}:{}", call.name, canonicalize_json(&call.arguments))
}

fn canonicalize_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => value.to_string(),
        serde_json::Value::Array(items) => {
            let rendered: Vec<String> = items.iter().map(canonicalize_json).collect();
            format!("[{}]", rendered.join(","))
        }
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&str> = map.keys().map(|k| k.as_str()).collect();
            keys.sort_unstable();
            let rendered: Vec<String> = keys
                .into_iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::Value::String(key.to_string()),
                        canonicalize_json(&map[key])
                    )
                })
                .collect();
            format!("{{{}}}", rendered.join(","))
        }
    }
}

fn flush_pending_tool_call(
    tool_calls: &mut Vec<ToolCall>,
    pending_id: &mut Option<String>,
    pending_name: &mut Option<String>,
    pending_args: &mut String,
) {
    if let (Some(id), Some(name)) = (pending_id.take(), pending_name.take()) {
        let arguments =
            serde_json::from_str(pending_args.as_str()).unwrap_or_else(|_| serde_json::json!({}));
        tool_calls.push(ToolCall {
            id,
            name,
            arguments,
        });
        pending_args.clear();
    }
}

fn extract_xml_tool_calls(text: &str) -> Vec<ToolCall> {
    const OPEN: &str = "<tool_call>";
    const CLOSE: &str = "</tool_call>";
    const ARG_KEY_OPEN: &str = "<arg_key>";
    const ARG_KEY_CLOSE: &str = "</arg_key>";
    const ARG_VAL_OPEN: &str = "<arg_value>";
    const ARG_VAL_CLOSE: &str = "</arg_value>";

    let mut calls = Vec::new();
    let mut search_from = 0;

    while let Some(start) = text[search_from..].find(OPEN) {
        let abs_start = search_from + start;
        let inner_start = abs_start + OPEN.len();

        let Some(end) = text[inner_start..].find(CLOSE) else {
            break;
        };
        let inner = &text[inner_start..inner_start + end];
        search_from = inner_start + end + CLOSE.len();

        let (tool_name, args_section) = match inner.find(ARG_KEY_OPEN) {
            Some(pos) => (inner[..pos].trim(), &inner[pos..]),
            None => (inner.trim(), ""),
        };

        if tool_name.is_empty() {
            continue;
        }

        let mut args = serde_json::Map::new();
        let mut arg_cursor = 0;
        while let Some(k_start) = args_section[arg_cursor..].find(ARG_KEY_OPEN) {
            let k_inner = arg_cursor + k_start + ARG_KEY_OPEN.len();
            let Some(k_end) = args_section[k_inner..].find(ARG_KEY_CLOSE) else {
                break;
            };
            let key = &args_section[k_inner..k_inner + k_end];
            let after_key = k_inner + k_end + ARG_KEY_CLOSE.len();

            let Some(v_offset) = args_section[after_key..].find(ARG_VAL_OPEN) else {
                break;
            };
            let v_inner = after_key + v_offset + ARG_VAL_OPEN.len();
            let Some(v_end) = args_section[v_inner..].find(ARG_VAL_CLOSE) else {
                break;
            };
            let value = &args_section[v_inner..v_inner + v_end];

            args.insert(
                key.to_string(),
                serde_json::Value::String(value.to_string()),
            );
            arg_cursor = v_inner + v_end + ARG_VAL_CLOSE.len();
        }

        calls.push(ToolCall {
            id: format!("xmlcall_{}", calls.len()),
            name: tool_name.to_string(),
            arguments: serde_json::Value::Object(args),
        });
    }

    calls
}

fn strip_xml_tool_calls(text: &str) -> String {
    const OPEN: &str = "<tool_call>";
    const CLOSE: &str = "</tool_call>";

    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;

    while let Some(start) = text[cursor..].find(OPEN) {
        result.push_str(&text[cursor..cursor + start]);
        let after_open = cursor + start + OPEN.len();
        match text[after_open..].find(CLOSE) {
            Some(end) => cursor = after_open + end + CLOSE.len(),
            None => {
                result.push_str(&text[cursor + start..]);
                return result.trim().to_string();
            }
        }
    }
    result.push_str(&text[cursor..]);
    result.trim().to_string()
}
