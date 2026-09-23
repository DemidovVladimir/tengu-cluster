//! The inner tool loop — run engine rounds, execute the model's tool calls
//! through a `ToolExecutor`, feed results back, until a final answer.
//! See `docs/context-management-2026-04-27.md` Layer 3.

use anyhow::Result;
use futures::StreamExt;
use tracing::debug;

use crate::domain::message::{Message, Role, StreamEvent, ToolCall, ToolDef};
use crate::domain::usage::{absorb_turn_usage_snapshot, apply_turn_usage_to_session_totals};
use crate::ports::engine::ToolExecutor;
use crate::ports::engine::{Engine, EngineContext};

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

            let result = match executor.execute(tc, &messages).await {
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
/// Pub(crate) so the run-agent subprocess (Phase 5b) can reuse this stream-
/// draining loop without duplicating the StreamEvent state machine.
pub(crate) async fn run_single_engine_turn(
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
    match crate::domain::token::truncate_at_boundary(result, max_chars) {
        None => result.to_string(),
        Some((prefix, end)) => format!(
            "{}\n\n[truncated — showing {} of {} chars]",
            prefix,
            end,
            result.len()
        ),
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
