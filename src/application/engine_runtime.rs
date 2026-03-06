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
/// Kept low to avoid re-sending the entire conversation on each round.
const MAX_TOOL_ROUNDS: usize = 15;

/// Maximum characters kept per tool result to prevent context explosion.
/// Tool results exceeding this limit are truncated with a suffix note.
const MAX_TOOL_RESULT_CHARS: usize = 4_000;

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

/// Decorator that redacts registered secret values from all tool output.
///
/// Wraps any `ToolExecutor` and runs the result through `SecretRegistry::redact`
/// before returning, preventing secrets from entering conversation messages.
pub(crate) struct SanitizedToolExecutor<'a> {
    inner: &'a dyn ToolExecutor,
    registry: &'a crate::domain::secret_registry::SecretRegistry,
}

impl<'a> SanitizedToolExecutor<'a> {
    pub fn new(
        inner: &'a dyn ToolExecutor,
        registry: &'a crate::domain::secret_registry::SecretRegistry,
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

/// Optional callback invoked after each tool execution, before feeding the
/// result back to the engine. Callers (e.g. Telegram) use this to show
/// tool results to the user for debugging.
pub(crate) type ToolResultObserver<'a> = &'a dyn Fn(&ToolCall, &str);

/// Execute one or more engine rounds, handling tool calls automatically.
///
/// If `tool_executor` is None or `tools` is empty, behaves like a single
/// engine call with no tool support.
///
/// `tool_observer` is called after each tool execution with the call and
/// its result string. Pass `None` to skip observation.
///
/// `cancel` is checked between tool rounds and between individual tool
/// executions. When set, processing stops and returns whatever text has
/// been collected so far.
pub(crate) async fn collect_engine_response(
    engine: &dyn Engine,
    prompt_messages: &[Message],
    tools: &[ToolDef],
    context: &EngineContext,
    tool_executor: Option<&dyn ToolExecutor>,
    tool_observer: Option<ToolResultObserver<'_>>,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<EngineResponse> {
    let mut messages: Vec<Message> = prompt_messages.to_vec();
    let mut total_input_delta: u32 = 0;
    let mut total_output_delta: u32 = 0;

    let is_cancelled = || {
        cancel.map_or(false, |f| f.load(std::sync::atomic::Ordering::Relaxed))
    };

    for round in 0..MAX_TOOL_ROUNDS {
        if is_cancelled() {
            debug!("Turn cancelled before round {}", round);
            return Ok(EngineResponse {
                text: String::new(),
                input_tokens_delta: total_input_delta,
                output_tokens_delta: total_output_delta,
            });
        }

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

        // Execute each tool and append results (truncated to cap context growth).
        for tc in &tool_calls {
            if is_cancelled() {
                debug!("Turn cancelled before executing tool {}", tc.name);
                return Ok(EngineResponse {
                    text: String::new(),
                    input_tokens_delta: total_input_delta,
                    output_tokens_delta: total_output_delta,
                });
            }
            let result = match executor.execute(tc) {
                Ok(output) => output,
                Err(e) => format!("Error: {}", e),
            };
            if let Some(observer) = &tool_observer {
                observer(tc, &result);
            }
            let content = truncate_tool_result(&result);
            messages.push(Message {
                role: Role::Tool,
                content,
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

    // Fallback: parse XML tool calls from response text when model
    // emits tool invocations as text instead of structured events.
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

/// Truncate a tool result to prevent context explosion in multi-round tool loops.
fn truncate_tool_result(result: &str) -> String {
    if result.len() <= MAX_TOOL_RESULT_CHARS {
        return result.to_string();
    }
    let mut end = MAX_TOOL_RESULT_CHARS;
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

/// Extract tool calls encoded as XML in the response text.
///
/// Some models (e.g. GLM-4 via HuggingFace) emit tool invocations as XML
/// in the content field instead of using structured `tool_calls`:
///
/// ```text
/// <tool_call>list_directory<arg_key>path</arg_key><arg_value>crates</arg_value></tool_call>
/// ```
///
/// Returns an empty vec when no XML tool calls are found or parsing fails.
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

        // Tool name is everything before the first <arg_key> (or the whole
        // inner string if there are no arguments).
        let (tool_name, args_section) = match inner.find(ARG_KEY_OPEN) {
            Some(pos) => (inner[..pos].trim(), &inner[pos..]),
            None => (inner.trim(), ""),
        };

        if tool_name.is_empty() {
            continue;
        }

        // Parse key-value pairs.
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

            args.insert(key.to_string(), serde_json::Value::String(value.to_string()));
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

/// Remove all `<tool_call>...</tool_call>` blocks from the text.
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
                // Unmatched opening tag — keep the rest as-is.
                result.push_str(&text[cursor + start..]);
                return result.trim().to_string();
            }
        }
    }
    result.push_str(&text[cursor..]);
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_xml_tool_calls_single() {
        let text = "<tool_call>list_directory<arg_key>path</arg_key><arg_value>crates</arg_value></tool_call>";
        let calls = extract_xml_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "xmlcall_0");
        assert_eq!(calls[0].name, "list_directory");
        assert_eq!(calls[0].arguments["path"], "crates");
    }

    #[test]
    fn test_extract_xml_tool_calls_multiple() {
        let text = "\
            <tool_call>read_file<arg_key>path</arg_key><arg_value>src/main.rs</arg_value></tool_call>\
            <tool_call>list_directory<arg_key>path</arg_key><arg_value>.</arg_value></tool_call>";
        let calls = extract_xml_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments["path"], "src/main.rs");
        assert_eq!(calls[0].id, "xmlcall_0");
        assert_eq!(calls[1].name, "list_directory");
        assert_eq!(calls[1].arguments["path"], ".");
        assert_eq!(calls[1].id, "xmlcall_1");
    }

    #[test]
    fn test_extract_xml_tool_calls_mixed_text() {
        let text = "Here is my plan:\n<tool_call>run_command<arg_key>cmd</arg_key><arg_value>ls</arg_value></tool_call>\nDone.";
        let calls = extract_xml_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "run_command");
        assert_eq!(calls[0].arguments["cmd"], "ls");
    }

    #[test]
    fn test_extract_xml_tool_calls_no_xml() {
        let calls = extract_xml_tool_calls("Just a normal response with no tools.");
        assert!(calls.is_empty());
    }

    #[test]
    fn test_strip_xml_tool_calls() {
        let text = "Before\n<tool_call>foo<arg_key>a</arg_key><arg_value>1</arg_value></tool_call>\nAfter";
        let stripped = strip_xml_tool_calls(text);
        assert_eq!(stripped, "Before\n\nAfter");
    }

    #[test]
    fn test_strip_xml_tool_calls_only_xml() {
        let text = "<tool_call>foo<arg_key>a</arg_key><arg_value>1</arg_value></tool_call>";
        let stripped = strip_xml_tool_calls(text);
        assert_eq!(stripped, "");
    }

    #[test]
    fn test_extract_xml_tool_calls_no_args() {
        let text = "<tool_call>stop_all</tool_call>";
        let calls = extract_xml_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "stop_all");
        assert!(calls[0].arguments.as_object().unwrap().is_empty());
    }

    #[test]
    fn test_extract_xml_tool_calls_multiple_args() {
        let text = "<tool_call>search<arg_key>query</arg_key><arg_value>hello</arg_value><arg_key>limit</arg_key><arg_value>10</arg_value></tool_call>";
        let calls = extract_xml_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "search");
        assert_eq!(calls[0].arguments["query"], "hello");
        assert_eq!(calls[0].arguments["limit"], "10");
    }
}
