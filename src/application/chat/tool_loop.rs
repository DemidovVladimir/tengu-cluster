//! The inner tool loop — run engine rounds, execute the model's tool calls
//! through a `ToolExecutor`, feed results back, until a final answer.
//! See `docs/context-management-2026-04-27.md` Layer 3.
//!
//! | Engine | Tool result entering the context (`tool_result_content`) |
//! |---|---|
//! | `tool_result_char_cap() = None` (OpenRouter, Claude Code) | text capped at `limits.max_tool_result_chars` |
//! | `Some(cap)` (local) | `fit_tool_result`, ≤ `min(cap, max_tool_result_chars)` footer included: a result that fits stays whole; above it a typed row's `data` → store-key pointer, then the cut |
//!
//! A tool that returns outside text (`domain::untrusted::tool_origin`:
//! `http_request`, `a2a`, `[[mcp_servers]]` tools, `agentic_memory`,
//! `memory_search`) has its context text in one `untrusted` fence (after the
//! cut; compaction keeps it). A decision loop parses tool output unfenced.
//!
//! Older rounds (from round 1 on): every earlier tool result keeps its
//! first line, ≤ `limits.compact_result_limit` (`compact_older_tool_results`).
//! A `run-agent` step runs this same loop (`LoopHooks`: its
//! `compress_and_store` and per-turn metrics), for every engine.
//!
//! At `max_tool_rounds` the engine gets one last call with no tools, so a
//! loop always ends in an answer.
//!
//! Activity: `EngineResponse.tool_runs` lists every call with its outcome —
//! the ones run here and the ones the engine ran itself
//! (`StreamEvent::ToolRan`, Claude Code through `tengu mcp-bridge`).
//!
//! Call ids: the executor sees `<prefix>:<loop nonce>:<round>:<i>:<provider id>`
//! (`LoopOpts::call_ids`: `chat` in-process, `step:<session>:<step>` in
//! `run-agent`; `loop_call_id` → `ToolCtx.call_id`, an exec tool's default
//! `client_order_id`); the messages keep the provider's id.

use anyhow::Result;
use futures::StreamExt;
use tracing::debug;

use crate::domain::message::{Message, Role, StreamEvent, ToolCall, ToolDef, ToolRun};
use crate::domain::metrics::MetricsLayer;
use crate::domain::observation::Observation;
use crate::domain::token::truncate_at_boundary;
use crate::domain::untrusted;
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
    /// Of `input_tokens_delta`, the tokens read from the provider's prompt
    /// cache (`StreamEvent::Usage::cached_input_tokens`).
    pub cached_input_tokens_delta: u32,
    /// Tool call outcomes collected during the turn (name, result).
    pub tool_outcomes: Vec<(String, String)>,
    /// Every tool call of the turn in order, with its outcome: the ones run
    /// here and the ones the engine ran itself (`StreamEvent::ToolRan` —
    /// Claude Code, which leaves `tool_outcomes` empty).
    pub tool_runs: Vec<ToolRun>,
}

/// One drained engine turn (`run_single_engine_turn`).
pub(crate) struct EngineTurn {
    pub text: String,
    /// Calls for the harness to run.
    pub tool_calls: Vec<ToolCall>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cached_input_tokens: u32,
    /// Calls the engine already ran (`StreamEvent::ToolRan`).
    pub engine_runs: Vec<ToolRun>,
}

/// Optional callback invoked after each tool execution.
pub type ToolResultObserver<'a> = &'a (dyn Fn(&ToolCall, &str) + Send + Sync);

/// One loop's knobs: an agent's `[limits]` (`from_limits`), the call-id
/// prefix, and the optional observer, cancel flag and hooks.
pub struct LoopOpts<'a> {
    pub max_tool_rounds: u32,
    pub max_tool_result_chars: u32,
    pub stream_event_timeout_secs: u64,
    pub compact_result_limit: u32,
    /// First part of every `ToolCtx.call_id` the loop mints
    /// (`loop_call_id`): `chat`, or `step:<session>:<step>` in `run-agent`.
    pub call_ids: String,
    pub observer: Option<ToolResultObserver<'a>>,
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
    pub hooks: Option<&'a dyn LoopHooks>,
}

impl LoopOpts<'_> {
    /// The agent's limits, `chat` call ids, no observer, cancel or hooks.
    pub fn from_limits(limits: &crate::config::LimitsConfig) -> Self {
        Self {
            max_tool_rounds: limits.max_tool_rounds,
            max_tool_result_chars: limits.max_tool_result_chars,
            stream_event_timeout_secs: limits.stream_event_timeout_secs,
            compact_result_limit: limits.compact_result_limit,
            call_ids: "chat".into(),
            observer: None,
            cancel: None,
            hooks: None,
        }
    }
}

/// One engine turn of the loop, as `LoopHooks::on_turn` sees it.
pub struct TurnStats<'t> {
    /// The turn's text (empty when it only called tools).
    pub text: &'t str,
    /// Size of the messages the turn was sent (chars, UTF-8 bytes).
    pub prompt_chars: u32,
    pub prompt_bytes: u32,
    /// The same size per message role (`system`, `user`, `assistant`,
    /// `tool`; empty roles left out) — the step's metrics layers.
    pub layers: Vec<MetricsLayer>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cached_input_tokens: u32,
    pub latency_ms: u64,
}

/// A `run-agent` step's two seams in the loop.
pub trait LoopHooks: Send + Sync {
    /// A call the loop answers itself, without the executor: `Some(result)`.
    /// The loop then ends after this round (the step's `compress_and_store`).
    fn intercept(&self, _call: &ToolCall) -> Option<String> {
        None
    }
    /// After every engine turn (the step's per-turn metrics).
    fn on_turn(&self, _turn: &TurnStats<'_>) {}
}

/// Execute one or more engine rounds, handling tool calls automatically.
pub async fn collect_engine_response(
    engine: &dyn Engine,
    prompt_messages: &[Message],
    tools: &[ToolDef],
    context: &EngineContext,
    tool_executor: Option<&dyn ToolExecutor>,
    opts: &LoopOpts<'_>,
) -> Result<EngineResponse> {
    let tool_rounds = opts.max_tool_rounds as usize;
    let result_chars_limit = opts.max_tool_result_chars as usize;
    let engine_result_cap = engine.tool_result_char_cap();
    let compact_limit = opts.compact_result_limit as usize;
    let (tool_observer, cancel, hooks) = (opts.observer, opts.cancel, opts.hooks);
    let mut messages: Vec<Message> = prompt_messages.to_vec();
    let mut total_input_delta: u32 = 0;
    let mut total_output_delta: u32 = 0;
    let mut total_cached_delta: u32 = 0;
    let mut tool_outcomes: Vec<(String, String)> = Vec::new();
    let mut tool_runs: Vec<ToolRun> = Vec::new();
    let turn_nonce = uuid::Uuid::new_v4().simple().to_string();

    let is_cancelled = || cancel.map_or(false, |f| f.load(std::sync::atomic::Ordering::Relaxed));

    for round in 0..tool_rounds {
        if is_cancelled() {
            debug!("Turn cancelled before round {}", round);
            return Ok(EngineResponse {
                text: String::new(),
                input_tokens_delta: total_input_delta,
                output_tokens_delta: total_output_delta,
                cached_input_tokens_delta: total_cached_delta,
                tool_outcomes,
                tool_runs,
            });
        }

        let turn = reported_turn(engine, &messages, tools, context, opts).await?;
        let (response_text, tool_calls) = (turn.text, turn.tool_calls);
        tool_runs.extend(turn.engine_runs);

        total_input_delta += turn.input_tokens;
        total_output_delta += turn.output_tokens;

        total_cached_delta += turn.cached_input_tokens;

        if tool_calls.is_empty() || tools.is_empty() || (tool_executor.is_none() && hooks.is_none())
        {
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
                cached_input_tokens_delta: total_cached_delta,
                tool_outcomes,
                tool_runs,
            });
        }

        debug!(round, tool_count = tool_calls.len(), "Executing tool calls");

        let compact_cutoff = messages.len();
        let mut stop = false;

        messages.push(Message {
            role: Role::Assistant,
            content: response_text.clone(),
            tool_call_id: None,
            tool_calls: Some(tool_calls.clone()),
        });

        for (i, tc) in tool_calls.iter().enumerate() {
            if is_cancelled() {
                debug!("Turn cancelled before executing tool {}", tc.name);
                return Ok(EngineResponse {
                    text: String::new(),
                    input_tokens_delta: total_input_delta,
                    output_tokens_delta: total_output_delta,
                    cached_input_tokens_delta: total_cached_delta,
                    tool_outcomes,
                    tool_runs,
                });
            }

            let call = ToolCall {
                id: loop_call_id(&opts.call_ids, &turn_nonce, round, i, &tc.id),
                name: tc.name.clone(),
                arguments: tc.arguments.clone(),
            };
            let intercepted = hooks.and_then(|h| h.intercept(tc));
            stop |= intercepted.is_some();
            let (result, observation, ok) = match (intercepted, tool_executor) {
                (Some(result), _) => (result, None, true),
                (None, Some(executor)) => match executor.execute_typed(&call, &messages).await {
                    Ok(output) => (output.text, output.observation, true),
                    Err(e) => {
                        // Redacted when the executor is a `SanitizedToolExecutor`.
                        let args = tc.arguments.to_string();
                        let args = truncate_at_boundary(&args, 300)
                            .map_or(args.as_str(), |(prefix, _)| prefix);
                        tracing::warn!(tool = %tc.name, error = %e, arguments = %args, "tool call failed");
                        (format!("ERROR: {}", e), None, false)
                    }
                },
                (None, None) => (
                    format!("ERROR: tool '{}' is not available to this agent", tc.name),
                    None,
                    false,
                ),
            };
            tool_runs.push(ToolRun {
                name: tc.name.clone(),
                ok,
            });

            if let Some(observer) = &tool_observer {
                observer(tc, &result);
            }
            tool_outcomes.push((tc.name.clone(), result.clone()));
            let content = tool_result_content(
                &tc.name,
                &result,
                observation.as_ref(),
                engine_result_cap,
                result_chars_limit,
            );
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
        if stop {
            debug!(round, "an intercepted call ends the loop");
            return Ok(EngineResponse {
                text: response_text,
                input_tokens_delta: total_input_delta,
                output_tokens_delta: total_output_delta,
                cached_input_tokens_delta: total_cached_delta,
                tool_outcomes,
                tool_runs,
            });
        }
    }

    // Exhaust all rounds — force a text response.
    tracing::warn!(
        rounds = tool_rounds,
        "tool loop hit max_tool_rounds: one last call without tools"
    );
    let turn = reported_turn(engine, &messages, &[], context, opts).await?;
    total_input_delta += turn.input_tokens;
    total_output_delta += turn.output_tokens;
    total_cached_delta += turn.cached_input_tokens;
    tool_runs.extend(turn.engine_runs);

    Ok(EngineResponse {
        text: turn.text,
        input_tokens_delta: total_input_delta,
        output_tokens_delta: total_output_delta,
        cached_input_tokens_delta: total_cached_delta,
        tool_outcomes,
        tool_runs,
    })
}

/// `run_single_engine_turn`, reported to `opts.hooks` (`LoopHooks::on_turn`).
async fn reported_turn(
    engine: &dyn Engine,
    messages: &[Message],
    tools: &[ToolDef],
    context: &EngineContext,
    opts: &LoopOpts<'_>,
) -> Result<EngineTurn> {
    let layers = opts.hooks.map(|_| role_layers(messages));
    let started = std::time::Instant::now();
    let turn = run_single_engine_turn(
        engine,
        messages,
        tools,
        context,
        opts.cancel,
        opts.stream_event_timeout_secs,
    )
    .await?;
    if let (Some(h), Some(layers)) = (opts.hooks, layers) {
        h.on_turn(&TurnStats {
            text: &turn.text,
            prompt_chars: layers.iter().map(|l| l.chars).sum(),
            prompt_bytes: layers.iter().map(|l| l.bytes).sum(),
            layers,
            input_tokens: turn.input_tokens,
            output_tokens: turn.output_tokens,
            cached_input_tokens: turn.cached_input_tokens,
            latency_ms: started.elapsed().as_millis() as u64,
        });
    }
    Ok(turn)
}

/// The size of `messages` per role, in role order; empty roles left out.
fn role_layers(messages: &[Message]) -> Vec<MetricsLayer> {
    let mut layers = ["system", "user", "assistant", "tool"].map(|name| MetricsLayer {
        name: name.to_string(),
        chars: 0,
        bytes: 0,
    });
    for m in messages {
        let layer = &mut layers[match m.role {
            Role::System => 0,
            Role::User => 1,
            Role::Assistant => 2,
            Role::Tool => 3,
        }];
        layer.chars += m.content.chars().count() as u32;
        layer.bytes += m.content.len() as u32;
    }
    layers.into_iter().filter(|l| l.bytes > 0).collect()
}

/// Run a single engine turn and collect text, tool calls, usage and the
/// tools the engine ran itself (`EngineTurn`).
/// Pub(crate) so the run-agent subprocess can reuse this stream-draining
/// loop without duplicating the StreamEvent state machine.
pub(crate) async fn run_single_engine_turn(
    engine: &dyn Engine,
    messages: &[Message],
    tools: &[ToolDef],
    context: &EngineContext,
    cancel: Option<&std::sync::atomic::AtomicBool>,
    stream_event_timeout_secs: u64,
) -> Result<EngineTurn> {
    let mut stream = engine.run(messages, tools, context).await?;

    let mut response_text = String::new();
    let mut turn_usage_snapshot: Option<(u32, u32)> = None;
    let mut cached_input_tokens: u32 = 0;
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut engine_runs: Vec<ToolRun> = Vec::new();
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
                cached_input_tokens: cached,
            } => {
                absorb_turn_usage_snapshot(&mut turn_usage_snapshot, input_tokens, output_tokens);
                cached_input_tokens = cached;
            }
            StreamEvent::ToolRan { name, ok } => engine_runs.push(ToolRun { name, ok }),
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

    let mut input_tokens: u32 = 0;
    let mut output_tokens: u32 = 0;
    apply_turn_usage_to_session_totals(&mut input_tokens, &mut output_tokens, turn_usage_snapshot);

    Ok(EngineTurn {
        text: response_text,
        tool_calls,
        input_tokens,
        output_tokens,
        cached_input_tokens,
        engine_runs,
    })
}

/// `ToolCtx.call_id` of the model's `index`-th call in `round` of one loop:
/// `<prefix>:<turn nonce>:<round>:<index>` (prefix `chat`, or
/// `step:<session>:<step>` in `run-agent`), then `:<provider id>` when it has
/// one. The provider's id is unique only as far as its provider makes it (a
/// local server may number every response's calls `call_0`, `call_1`, …), so
/// an exec tool keying its order on it could replay an older order's fill.
/// This id never repeats across processes (`turn_nonce`, a uuid per loop),
/// rounds or calls — an exec call from chat places its own order unless the
/// model passes a `client_order_id`. Loops, feeds and the bridge mint their
/// own (`ports::tool::ToolCtx::call_id`).
fn loop_call_id(
    prefix: &str,
    turn_nonce: &str,
    round: usize,
    index: usize,
    provider_id: &str,
) -> String {
    if provider_id.is_empty() {
        format!("{prefix}:{turn_nonce}:{round}:{index}")
    } else {
        format!("{prefix}:{turn_nonce}:{round}:{index}:{provider_id}")
    }
}

// ---------------------------------------------------------------------------
// Tool result compaction
// ---------------------------------------------------------------------------

/// Compact old tool results to manage context size: every `Role::Tool`
/// message in `older` (the rounds before the current one) keeps a short
/// summary instead of "ok" so the model remembers what happened (hashes,
/// IDs, status) and doesn't repeat steps. `run-agent` calls it the same way
/// (from turn 1 on, every engine).
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

/// Context text of one `tool` result as every tool loop feeds it back
/// (`collect_engine_response` and `run-agent`, module table; `tengu
/// mcp-bridge` too): an engine with a per-result cap (`engine_cap`, local
/// models) gets [`fit_tool_result`] within `min(cap, max_chars)`; any other
/// engine the text cut at `max_chars` (`limits.max_tool_result_chars`),
/// footer after the cut. A tool that returns outside text
/// (`untrusted::tool_origin`: web, A2A, MCP, memory) has it fenced after the
/// cut.
pub(crate) fn tool_result_content(
    tool: &str,
    result: &str,
    observation: Option<&Observation>,
    engine_cap: Option<usize>,
    max_chars: usize,
) -> String {
    let content = match engine_cap {
        Some(cap) => fit_tool_result(result, observation, cap.min(max_chars)),
        None => truncate_tool_result(result, max_chars),
    };
    match untrusted::tool_origin(tool) {
        Some(origin) => untrusted::fence(&origin, &content),
        None => content,
    }
}

/// Context text of one tool result for an engine with a per-result cap
/// (`Engine::tool_result_char_cap` — local models), truncation footer
/// included, within `max_chars`:
///
/// | Result | Context text |
/// |---|---|
/// | fits | whole — a typed row keeps its `data` (the ids, sides, sizes and deadlines a follow-up call needs: no tool reads a row back by key) |
/// | typed row above the cap | `data` swapped for its store-key pointer (`Observation::compact_text`), then cut if still above |
/// | other text above the cap | cut |
pub(crate) fn fit_tool_result(
    text: &str,
    observation: Option<&Observation>,
    max_chars: usize,
) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
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
    // A fenced result keeps its fence around the compacted text.
    if let Some((origin, inner)) = untrusted::unwrap(content) {
        return untrusted::fence(origin, &compact_tool_result(inner, limit));
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
        let arguments = serde_json::from_str(pending_args.as_str()).unwrap_or_else(|e| {
            // Empty = no arguments; anything else is a model error the tool
            // will report as missing fields — say what really happened.
            if !pending_args.trim().is_empty() {
                let shown = truncate_at_boundary(pending_args, 300)
                    .map_or(pending_args.as_str(), |(prefix, _)| prefix);
                tracing::warn!(tool = %name, error = %e, arguments = %shown, "tool call arguments are not valid JSON — sent as {{}}");
            }
            serde_json::json!({})
        });
        tool_calls.push(ToolCall {
            id,
            name,
            arguments,
        });
        pending_args.clear();
    }
}

#[cfg(test)]
#[path = "tool_loop_tests.rs"]
mod tests;
