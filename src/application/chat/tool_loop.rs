//! The inner tool loop — run engine rounds, execute the model's tool calls
//! through a `ToolExecutor`, feed results back, until a final answer.
//! See `docs/context-management-2026-04-27.md` Layer 3.
//!
//! | Engine | Tool result entering the context |
//! |---|---|
//! | `tool_result_char_cap() = None` (OpenRouter, Claude Code) | text capped at `limits.max_tool_result_chars` |
//! | `Some(cap)` (local) | `fit_tool_result`: typed row `data` → store-key pointer, then ≤ `min(cap, max_tool_result_chars)` footer included |

use anyhow::Result;
use futures::StreamExt;
use tracing::debug;

use crate::domain::message::{Message, Role, StreamEvent, ToolCall, ToolDef};
use crate::domain::observation::Observation;
use crate::domain::token::truncate_at_boundary;
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
    let engine_result_cap = engine.tool_result_char_cap();
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

            let (result, observation) = match executor.execute_typed(tc, &messages).await {
                Ok(output) => (output.text, output.observation),
                Err(e) => (format!("ERROR: {}", e), None),
            };

            if let Some(observer) = &tool_observer {
                observer(tc, &result);
            }
            tool_outcomes.push((tc.name.clone(), result.clone()));
            let content = match engine_result_cap {
                Some(cap) => {
                    fit_tool_result(&result, observation.as_ref(), cap.min(result_chars_limit))
                }
                None => truncate_tool_result(&result, result_chars_limit),
            };
            messages.push(Message {
                role: Role::Tool,
                content,
                tool_call_id: Some(tc.id.clone()),
                tool_calls: None,
            });
        }

        if round >= 1 {
            compact_older_tool_results(&mut messages[..compact_cutoff], compact_limit);
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

/// Compact old tool results to manage context size: every `Role::Tool`
/// message in `older` (the rounds before the current one) keeps a short
/// summary instead of "ok" so the model remembers what happened (hashes,
/// IDs, status) and doesn't repeat steps. `run-agent` calls it for local
/// engines too.
pub(crate) fn compact_older_tool_results(older: &mut [Message], limit: usize) {
    for msg in older {
        if matches!(msg.role, Role::Tool) {
            let compacted = compact_tool_result(&msg.content, limit);
            if compacted != msg.content {
                msg.content = compacted;
            }
        }
    }
}

/// Context text of one tool result for an engine with a per-result cap
/// (`Engine::tool_result_char_cap` — local models): a typed row's `data`
/// swapped for its store-key pointer (`Observation::compact_text`), then the
/// whole — truncation footer included — within `max_chars`.
pub(crate) fn fit_tool_result(
    text: &str,
    observation: Option<&Observation>,
    max_chars: usize,
) -> String {
    let text = match observation {
        Some(obs) => obs.compact_text(text),
        None => text.to_string(),
    };
    if text.len() <= max_chars {
        return text;
    }
    // Footer digits never exceed those of `max_chars`, so this reserve holds.
    let reserve = truncation_footer(max_chars, text.len()).len();
    if reserve >= max_chars {
        // A cap smaller than the footer itself: the bare cut.
        return truncate_at_boundary(&text, max_chars)
            .map_or_else(String::new, |(prefix, _)| prefix.to_string());
    }
    match truncate_at_boundary(&text, max_chars - reserve) {
        Some((prefix, end)) => format!("{prefix}{}", truncation_footer(end, text.len())),
        None => text,
    }
}

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
    match truncate_at_boundary(result, max_chars) {
        None => result.to_string(),
        Some((prefix, end)) => format!("{prefix}{}", truncation_footer(end, result.len())),
    }
}

fn truncation_footer(shown: usize, total: usize) -> String {
    format!("\n\n[truncated — showing {shown} of {total} chars]")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::message::ModelInfo;
    use crate::domain::observation::{ObsSource, ObsStatus};
    use crate::domain::token::tool_result_char_budget;
    use crate::ports::tool::ToolOutput;
    use futures::Stream;
    use std::pin::Pin;
    use std::sync::Mutex;

    const KEY: &str = "mkt_ctx/1:hyperliquid:xyz:TSLA";

    fn typed_obs() -> Observation {
        Observation {
            key: KEY.into(),
            schema: "mkt_ctx/1".into(),
            tool: "hl_ctx".into(),
            observed_at_ms: 0,
            slot: None,
            ttl_ms: 15_000,
            source: ObsSource::Live,
            status: ObsStatus::Ok,
            errors: vec![],
            headline: "hl_ctx hyperliquid:xyz:TSLA mark=436.12 oi_usd=1234567".into(),
            features: [("mark".to_string(), serde_json::json!(436.12))].into(),
            data: serde_json::json!({"levels": "x".repeat(12_000)}),
        }
    }

    /// Round 0 calls `big` (100 000 bytes, multibyte) and `typed` (an
    /// observation with 12 KB of `data`); round 1 answers. Records what
    /// each round was sent.
    struct ScriptedEngine {
        cap: Option<usize>,
        seen: Mutex<Vec<Vec<Message>>>,
    }

    #[async_trait::async_trait]
    impl Engine for ScriptedEngine {
        fn id(&self) -> &str {
            "scripted"
        }
        fn context_window(&self) -> usize {
            16_384
        }
        fn supports_tool_use(&self) -> bool {
            true
        }
        fn manages_own_workspace(&self) -> bool {
            false
        }
        fn available_models(&self) -> Vec<ModelInfo> {
            Vec::new()
        }
        fn tool_result_char_cap(&self) -> Option<usize> {
            self.cap
        }
        async fn run(
            &self,
            messages: &[Message],
            _tools: &[ToolDef],
            _context: &EngineContext,
        ) -> Result<Pin<Box<dyn Stream<Item = StreamEvent> + Send>>> {
            let mut seen = self.seen.lock().unwrap();
            let call = |id: &str, name: &str| {
                [
                    StreamEvent::ToolCallStart {
                        id: id.into(),
                        name: name.into(),
                    },
                    StreamEvent::ToolCallEnd { id: id.into() },
                ]
            };
            let mut events: Vec<StreamEvent> = if seen.is_empty() {
                call("c1", "big")
                    .into_iter()
                    .chain(call("c2", "typed"))
                    .collect()
            } else {
                vec![StreamEvent::TextDelta {
                    text: "done".into(),
                }]
            };
            events.push(StreamEvent::Done);
            seen.push(messages.to_vec());
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    struct Tools;

    #[async_trait::async_trait]
    impl ToolExecutor for Tools {
        async fn execute(&self, call: &ToolCall, m: &[Message]) -> Result<String> {
            Ok(self.execute_typed(call, m).await?.text)
        }
        async fn execute_typed(&self, call: &ToolCall, _m: &[Message]) -> Result<ToolOutput> {
            Ok(match call.name.as_str() {
                "big" => ToolOutput::from("é".repeat(50_000)),
                _ => ToolOutput::observed(typed_obs(), 0),
            })
        }
    }

    /// Tool messages the engine received in round 1, plus the loop's outcomes.
    async fn run_turn(cap: Option<usize>) -> (Vec<String>, Vec<(String, String)>) {
        let engine = ScriptedEngine {
            cap,
            seen: Mutex::new(Vec::new()),
        };
        let prompt = [Message {
            role: Role::User,
            content: "go".into(),
            tool_call_id: None,
            tool_calls: None,
        }];
        let tools = [
            ToolDef::new("big", "d", serde_json::json!({"type": "object"})),
            ToolDef::new("typed", "d", serde_json::json!({"type": "object"})),
        ];
        let context = EngineContext {
            workspace: None,
            system_prompt: None,
            bridge_tools: None,
            max_tool_rounds: None,
            max_mcp_result_chars: None,
            mcp_servers: Vec::new(),
        };
        let resp = collect_engine_response(
            &engine,
            &prompt,
            &tools,
            &context,
            Some(&Tools),
            None,
            None,
            None,
            5,
            300_000,
            30,
            200,
        )
        .await
        .unwrap();
        assert_eq!(resp.text, "done");
        let seen = engine.seen.lock().unwrap();
        let tool_msgs = seen[1]
            .iter()
            .filter(|m| matches!(m.role, Role::Tool))
            .map(|m| m.content.clone())
            .collect();
        (tool_msgs, resp.tool_outcomes)
    }

    #[tokio::test]
    async fn local_16k_agent_never_receives_a_result_above_its_budget() {
        let budget = tool_result_char_budget(16_384);
        let (msgs, outcomes) = run_turn(Some(budget)).await;
        assert_eq!(msgs.len(), 2);
        for m in &msgs {
            assert!(m.len() <= budget, "{} bytes > {budget}", m.len());
        }
        assert!(msgs[0].ends_with(" of 100000 chars]"), "no footer");
        let typed: Vec<&str> = msgs[1].lines().collect();
        assert_eq!(
            typed[0],
            "hl_ctx hyperliquid:xyz:TSLA mark=436.12 oi_usd=1234567 | ok 0s live"
        );
        assert_eq!(typed[1], "mark=436.12");
        assert_eq!(
            typed[2],
            format!(
                "data: {} bytes in observation {KEY}",
                typed_obs().data.to_string().len()
            )
        );
        // Observers / outcomes keep the raw result.
        assert_eq!(outcomes[0].1.len(), 100_000);
        assert_eq!(outcomes[1].1, typed_obs().render_text(0));
    }

    #[tokio::test]
    async fn uncapped_engines_are_unchanged() {
        let (msgs, _) = run_turn(None).await;
        assert_eq!(msgs[0], "é".repeat(50_000));
        assert_eq!(msgs[1], typed_obs().render_text(0));
        assert!(msgs[1].contains(&"x".repeat(12_000)));
    }

    #[test]
    fn fit_counts_the_footer_inside_the_cap() {
        let text = "é".repeat(50_000);
        for cap in [8_192usize, 8_191, 4_097, 100, 10, 0] {
            let fitted = fit_tool_result(&text, None, cap);
            assert!(fitted.len() <= cap, "cap {cap}: {} bytes", fitted.len());
        }
        assert!(fit_tool_result(&text, None, 8_192).contains("[truncated — showing"));
        assert_eq!(fit_tool_result("short", None, 8_192), "short");
        // Uncapped engines keep the footer-after-cut behaviour.
        assert!(truncate_tool_result(&text, 100).len() > 100);
    }

    #[test]
    fn older_rounds_keep_line1_of_tool_results_only() {
        let msg = |role, content: &str| Message {
            role,
            content: content.to_string(),
            tool_call_id: None,
            tool_calls: None,
        };
        let long = format!("line one\n{}", "y".repeat(500));
        let mut older = vec![msg(Role::User, &long), msg(Role::Tool, &long)];
        compact_older_tool_results(&mut older, 200);
        assert_eq!(older[0].content, long, "non-tool messages untouched");
        assert_eq!(older[1].content, "line one");
    }
}
