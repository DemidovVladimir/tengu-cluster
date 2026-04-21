//! Engine builder: OpenRouter backend, factory functions, and tool-loop runtime.
//!
//! Consolidates engine construction, the OpenRouter API adapter, message/tool
//! conversion helpers, and the multi-turn tool-call execution loop.

use anyhow::Result;
use async_trait::async_trait;
use futures::stream;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tracing::{debug, error};

use crate::adapters::types::{Message, ModelInfo, Role, StreamEvent, ToolCall, ToolDef};
use crate::adapters::usage::{absorb_turn_usage_snapshot, apply_turn_usage_to_session_totals};
use crate::adapters::{Engine, EngineContext, EngineDiagnostics};

// ---------------------------------------------------------------------------
// Factory — build engines from config
// ---------------------------------------------------------------------------

/// Build a lightweight engine for the planner/classifier from explicit engine + model strings.
pub(crate) fn build_planner_engine(
    engine_type: &str,
    model: &str,
    claude_code_config: Option<&crate::adapters::config::ClaudeCodeConfig>,
) -> Result<Box<dyn Engine>> {
    match engine_type {
        "claude_code" => {
            #[cfg(feature = "claude_code")]
            {
                let cc = claude_code_config.cloned().unwrap_or_default();
                let model_opt = if model.is_empty() {
                    None
                } else {
                    Some(model.to_string())
                };
                Ok(Box::new(
                    crate::adapters::claude_code_engine::ClaudeCodeEngine::new(
                        std::path::PathBuf::from(&cc.cli_path),
                        crate::adapters::claude_code_engine::BuiltinToolsProfile::ReadOnly,
                        model_opt,
                        cc.timeout_secs,
                    ),
                ))
            }
            #[cfg(not(feature = "claude_code"))]
            {
                let _ = (model, claude_code_config);
                anyhow::bail!("claude_code engine requires --features claude_code")
            }
        }
        _ => {
            let defaults = crate::adapters::config::LimitsConfig::default();
            build_openrouter_engine(model, defaults.context_window as usize)
        }
    }
}

/// Build configured engine instance for one agent.
pub(crate) fn build_engine(
    _agent_id: &str,
    agent_config: &crate::adapters::config::AgentConfig,
    claude_code_config: Option<&crate::adapters::config::ClaudeCodeConfig>,
) -> Result<Box<dyn Engine>> {
    match agent_config.engine.as_str() {
        "claude_code" => {
            #[cfg(feature = "claude_code")]
            {
                let cc = claude_code_config.cloned().unwrap_or_default();
                let profile = agent_config
                    .claude_code
                    .as_ref()
                    .map(|c| c.builtin_tools_profile.as_str())
                    .unwrap_or("editor_shell");
                let model_opt = if agent_config.model.is_empty() {
                    None
                } else {
                    Some(agent_config.model.clone())
                };
                let timeout = agent_config.limits.stream_event_timeout_secs;
                Ok(Box::new(
                    crate::adapters::claude_code_engine::ClaudeCodeEngine::new(
                        std::path::PathBuf::from(&cc.cli_path),
                        crate::adapters::claude_code_engine::BuiltinToolsProfile::from_str(profile),
                        model_opt,
                        timeout,
                    ),
                ))
            }
            #[cfg(not(feature = "claude_code"))]
            {
                let _ = claude_code_config;
                anyhow::bail!("claude_code engine requires --features claude_code")
            }
        }
        _ => {
            let context_window = agent_config.limits.context_window.max(1) as usize;
            build_openrouter_engine(&agent_config.model, context_window)
        }
    }
}

pub fn build_openrouter_engine(model: &str, context_window: usize) -> Result<Box<dyn Engine>> {
    let api_key = std::env::var("OPENROUTER_API_KEY")
        .map_err(|_| anyhow::anyhow!("OPENROUTER_API_KEY is required"))?;
    let base_url = std::env::var("OPENROUTER_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api".to_string());
    Ok(Box::new(OpenRouterEngine::new(
        &base_url,
        model,
        &api_key,
        context_window,
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
    pub fn new(base_url: &str, model: &str, api_key: &str, context_window: usize) -> Self {
        let context_window_tokens = context_window;
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

        let raw_body = match response.text().await {
            Ok(body) => body,
            Err(e) => {
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

// ---------------------------------------------------------------------------
// Engine runtime — tool-loop execution
// ---------------------------------------------------------------------------

/// Result of a single engine call including response text and token usage delta.
pub struct EngineResponse {
    pub text: String,
    pub input_tokens_delta: u32,
    pub output_tokens_delta: u32,
    /// Tool call outcomes collected during the turn (name, result).
    pub tool_outcomes: Vec<(String, String)>,
}

/// Trait for executing tool calls.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, call: &ToolCall) -> Result<String>;
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

#[async_trait]
impl<'a> ToolExecutor for SanitizedToolExecutor<'a> {
    async fn execute(&self, call: &ToolCall) -> Result<String> {
        let result = self.inner.execute(call).await?;
        Ok(self.registry.redact(&result))
    }
}

/// Owned variant of `SanitizedToolExecutor` that holds its dependencies behind
/// `Arc`s. Used by the orchestrator chat-factory path, where the factory
/// closure must return a `'static` `Arc<dyn ToolExecutor>` — borrowing into
/// a per-turn stack-local `SanitizedToolExecutor<'a>` is not possible there.
pub(crate) struct OwnedSanitizedToolExecutor {
    inner: std::sync::Arc<dyn ToolExecutor>,
    registry: std::sync::Arc<crate::adapters::secret_builder::SecretRegistry>,
}

impl OwnedSanitizedToolExecutor {
    pub fn new(
        inner: std::sync::Arc<dyn ToolExecutor>,
        registry: std::sync::Arc<crate::adapters::secret_builder::SecretRegistry>,
    ) -> Self {
        Self { inner, registry }
    }
}

#[async_trait]
impl ToolExecutor for OwnedSanitizedToolExecutor {
    async fn execute(&self, call: &ToolCall) -> Result<String> {
        let result = self.inner.execute(call).await?;
        Ok(self.registry.redact(&result))
    }
}

/// Optional callback invoked after each tool execution.
pub type ToolResultObserver<'a> = &'a (dyn Fn(&ToolCall, &str) + Send + Sync);

/// Execute one or more engine rounds, handling tool calls automatically.
pub async fn collect_engine_response(
    engine: &dyn Engine,
    prompt_messages: &[Message],
    tools: &[ToolDef],
    context: &EngineContext,
    tool_executor: Option<&dyn ToolExecutor>,
    tool_observer: Option<ToolResultObserver<'_>>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    token_budget: Option<u32>,
    max_tool_rounds: u32,
    max_tool_result_chars: u32,
    stream_event_timeout_secs: u64,
    compact_result_limit: u32,
) -> Result<EngineResponse> {
    let tool_rounds = max_tool_rounds as usize;
    let result_chars_limit = max_tool_result_chars as usize;
    let stream_timeout = stream_event_timeout_secs;
    let compact_limit = compact_result_limit as usize;
    let mut messages: Vec<Message> = prompt_messages.to_vec();
    let mut total_input_delta: u32 = 0;
    let mut total_output_delta: u32 = 0;
    let mut tool_outcomes: Vec<(String, String)> = Vec::new();

    let is_cancelled = || cancel.map_or(false, |f| f.load(std::sync::atomic::Ordering::Relaxed));

    for round in 0..tool_rounds {
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
            run_single_engine_turn(engine, &messages, tools, context, cancel, stream_timeout)
                .await?;

        total_input_delta += input_delta;
        total_output_delta += output_delta;

        if let Some(budget) = token_budget {
            let total = total_input_delta + total_output_delta;
            if total > budget {
                tracing::warn!(
                    total_tokens = total,
                    budget,
                    round,
                    "Token budget exceeded — stopping tool loop"
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
            // Auto-continue on truncated output.
            if tool_calls.is_empty()
                && !tools.is_empty()
                && round < tool_rounds - 1
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
                    content: "Your output was truncated. Continue from where you left off."
                        .to_string(),
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

            let result = match executor.execute(tc).await {
                Ok(output) => output,
                Err(e) => format!("ERROR: {}", e),
            };

            if let Some(observer) = &tool_observer {
                observer(tc, &result);
            }
            tool_outcomes.push((tc.name.clone(), result.clone()));
            let content = truncate_tool_result(&result, result_chars_limit);
            messages.push(Message {
                role: Role::Tool,
                content,
                tool_call_id: Some(tc.id.clone()),
                tool_calls: None,
            });
        }

        // Compact old tool results to manage context size.
        // Keep a short summary instead of "ok" so the model remembers
        // what happened (hashes, IDs, status) and doesn't repeat steps.
        if round >= 1 {
            for msg in &mut messages[..compact_cutoff] {
                if matches!(msg.role, Role::Tool) {
                    let compacted = compact_tool_result(&msg.content, compact_limit);
                    if compacted != msg.content {
                        msg.content = compacted;
                    }
                }
            }
        }
    }

    // Exhaust all rounds — force a text response.
    let (response_text, _, input_delta, output_delta) =
        run_single_engine_turn(engine, &messages, &[], context, cancel, stream_timeout).await?;
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
    stream_event_timeout_secs: u64,
) -> Result<(String, Vec<ToolCall>, u32, u32)> {
    let mut stream = engine.run(messages, tools, context).await?;

    let mut response_text = String::new();
    let mut turn_usage_snapshot: Option<(u32, u32)> = None;
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut pending_tool_id: Option<String> = None;
    let mut pending_tool_name: Option<String> = None;
    let mut pending_tool_args = String::new();

    let is_cancelled = || cancel.map_or(false, |f| f.load(std::sync::atomic::Ordering::Relaxed));
    let cancel_poll_secs: u64 = 2;
    let mut idle_secs: u64 = 0;

    loop {
        if is_cancelled() {
            debug!("Stream cancelled by user");
            break;
        }
        let event = match tokio::time::timeout(
            std::time::Duration::from_secs(cancel_poll_secs),
            stream.next(),
        )
        .await
        {
            Ok(Some(event)) => {
                idle_secs = 0;
                event
            }
            Ok(None) => break,
            Err(_) => {
                idle_secs += cancel_poll_secs;
                if idle_secs >= stream_event_timeout_secs {
                    return Err(anyhow::anyhow!(
                        "Engine stream timed out — no data for {}s",
                        stream_event_timeout_secs
                    ));
                }
                continue;
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

    let mut input_delta: u32 = 0;
    let mut output_delta: u32 = 0;
    apply_turn_usage_to_session_totals(&mut input_delta, &mut output_delta, turn_usage_snapshot);

    Ok((response_text, tool_calls, input_delta, output_delta))
}

// ---------------------------------------------------------------------------
// Tool result compaction
// ---------------------------------------------------------------------------

/// Compact a tool result for older rounds. Preserves the first line (which
/// typically contains key outputs like tx hashes, addresses, status) and
/// truncates the rest. Results already short enough are returned unchanged.
fn compact_tool_result(content: &str, limit: usize) -> String {
    if content.len() <= limit {
        return content.to_string();
    }

    // Take the first line — most tool results put key info there.
    let first_line = content.lines().next().unwrap_or(content);
    if first_line.len() <= limit {
        return first_line.to_string();
    }

    // First line itself is too long — truncate it.
    let mut end = limit;
    while end > 0 && !first_line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &first_line[..end])
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn truncate_tool_result(result: &str, max_chars: usize) -> String {
    if result.len() <= max_chars {
        return result.to_string();
    }
    let mut end = max_chars;
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
