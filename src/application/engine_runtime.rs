//! Engine-turn runtime execution helpers.
//!
//! Supports multi-turn tool calling: if the engine responds with tool_calls,
//! they are executed via the ToolExecutor trait and the results fed back
//! to the engine for up to MAX_TOOL_ROUNDS iterations.

use crate::domain::usage::{absorb_turn_usage_snapshot, apply_turn_usage_to_session_totals};
use anyhow::Result;
use futures::StreamExt;
use tengu_core::types::{Message, Role, StreamEvent, ToolCall, ToolDef};
use tengu_core::{Engine, EngineContext};
use tracing::debug;

/// Maximum number of tool-call round-trips before forcing a text response.
const MAX_TOOL_ROUNDS: usize = 10;

/// Result of a single engine call including response text and token usage delta.
pub(crate) struct EngineResponse {
    pub text: String,
    pub input_tokens_delta: u32,
    pub output_tokens_delta: u32,
}

/// Trait for executing tool calls. Implementations decide how to handle
/// each tool (immediate execution, user confirmation, etc.).
pub(crate) trait ToolExecutor {
    /// Execute a tool call and return the result string.
    /// Returns Err if the tool call cannot be executed.
    fn execute(&self, call: &ToolCall) -> Result<String>;
}

/// Execute one or more engine rounds, handling tool calls automatically.
///
/// If `tool_executor` is None or `tools` is empty, behaves like a single
/// engine call with no tool support.
pub(crate) async fn collect_engine_response(
    engine: &dyn Engine,
    prompt_messages: &[Message],
    tools: &[ToolDef],
    context: &EngineContext,
    tool_executor: Option<&dyn ToolExecutor>,
) -> Result<EngineResponse> {
    let mut messages: Vec<Message> = prompt_messages.to_vec();
    let mut total_input_delta: u32 = 0;
    let mut total_output_delta: u32 = 0;

    for round in 0..MAX_TOOL_ROUNDS {
        let (response_text, tool_calls, input_delta, output_delta) =
            run_single_engine_turn(engine, &messages, tools, context).await?;

        total_input_delta += input_delta;
        total_output_delta += output_delta;

        // If no tool calls, we're done — return the text response.
        if tool_calls.is_empty() || tool_executor.is_none() || tools.is_empty() {
            return Ok(EngineResponse {
                text: response_text,
                input_tokens_delta: total_input_delta,
                output_tokens_delta: total_output_delta,
            });
        }

        let executor = tool_executor.unwrap();
        debug!(round, tool_count = tool_calls.len(), "Executing tool calls");

        // Append the assistant message with tool_calls to the conversation.
        messages.push(Message {
            role: Role::Assistant,
            content: response_text,
            tool_call_id: None,
            tool_calls: Some(tool_calls.clone()),
        });

        // Execute each tool and append results.
        for tc in &tool_calls {
            let result = match executor.execute(tc) {
                Ok(output) => output,
                Err(e) => format!("Error: {}", e),
            };
            messages.push(Message {
                role: Role::Tool,
                content: result,
                tool_call_id: Some(tc.id.clone()),
                tool_calls: None,
            });
        }
    }

    // If we exhaust all rounds, return whatever text we have from the last round.
    let (response_text, _, input_delta, output_delta) =
        run_single_engine_turn(engine, &messages, &[], context).await?;
    total_input_delta += input_delta;
    total_output_delta += output_delta;

    Ok(EngineResponse {
        text: response_text,
        input_tokens_delta: total_input_delta,
        output_tokens_delta: total_output_delta,
    })
}

/// Run a single engine turn and collect text, tool calls, and usage.
async fn run_single_engine_turn(
    engine: &dyn Engine,
    messages: &[Message],
    tools: &[ToolDef],
    context: &EngineContext,
) -> Result<(String, Vec<ToolCall>, u32, u32)> {
    let mut stream = engine.run(messages, tools, context).await?;

    let mut response_text = String::new();
    let mut turn_usage_snapshot: Option<(u32, u32)> = None;
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    // Accumulate tool call data from streaming events
    let mut pending_tool_id: Option<String> = None;
    let mut pending_tool_name: Option<String> = None;
    let mut pending_tool_args = String::new();

    while let Some(event) = stream.next().await {
        match event {
            StreamEvent::TextDelta { text } => {
                response_text.push_str(&text);
            }
            StreamEvent::ToolCallStart { id, name } => {
                // Flush any pending tool call
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

    // Flush any remaining pending tool call
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

/// Flush a pending tool call into the tool_calls vector.
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
