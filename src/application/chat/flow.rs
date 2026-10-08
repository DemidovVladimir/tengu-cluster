//! Flow management: key resolution, history turn limits, compaction policy, and compaction.

use crate::config::FlowConfig;
use crate::domain::message::{Message, Recipient, Role};
use crate::domain::session::FlowCompactionPolicy;
use crate::domain::token::estimate_tokens_approx_min1;
use anyhow::Result;
use tracing::info;

// ---------------------------------------------------------------------------
// Flow key resolution
// ---------------------------------------------------------------------------

/// Resolve a deterministic flow key from scope and sender identity.
pub(crate) fn resolve_flow_key(
    agent_id: &str,
    scope: &str,
    sender: &Recipient,
    manual_session_id: Option<&str>,
) -> String {
    let base = match scope {
        "main" => format!("{}:main", agent_id),
        "per-pipe-sender" => format!("{}:{}:{}", agent_id, sender.pipe_id, sender.peer_id),
        "per-group" => format!(
            "{}:{}:{}",
            agent_id,
            sender.pipe_id,
            sender
                .thread_id
                .as_deref()
                .or(sender.account_id.as_deref())
                .unwrap_or(sender.peer_id.as_str())
        ),
        _ => format!("{}:{}", agent_id, sender.peer_id), // per-sender default
    };

    match manual_session_id {
        Some(session) if !session.is_empty() => format!("{}:{}", base, session),
        _ => base,
    }
}

// ---------------------------------------------------------------------------
// History turn limits
// ---------------------------------------------------------------------------

pub(crate) fn resolve_history_turn_limit(flow: &FlowConfig) -> usize {
    flow.max_history_turns
        .map(|v| v.max(1) as usize)
        .unwrap_or_else(|| default_history_turn_limit_for_scope(&flow.scope))
}

fn default_history_turn_limit_for_scope(scope: &str) -> usize {
    match scope {
        "main" => 40,
        "per-group" => 30,
        "per-pipe-sender" => 25,
        _ => 20,
    }
}

pub(crate) fn enforce_history_turn_limit(messages: &mut Vec<Message>, max_turns: usize) -> usize {
    if max_turns == 0 || messages.is_empty() {
        let dropped = messages.len();
        messages.clear();
        return dropped;
    }

    let mut seen_user_turns = 0usize;
    let mut start_index = None;

    for (idx, message) in messages.iter().enumerate().rev() {
        if matches!(message.role, Role::User) {
            seen_user_turns += 1;
            if seen_user_turns == max_turns {
                start_index = Some(idx);
                break;
            }
        }
    }

    let Some(start) = start_index else {
        return 0;
    };

    if start == 0 {
        return 0;
    }

    messages.drain(0..start);
    start
}

// ---------------------------------------------------------------------------
// Compaction policy resolution
// ---------------------------------------------------------------------------

pub(crate) fn resolve_flow_compaction_policy(
    flow: &FlowConfig,
    max_tokens_per_flow: u64,
    context_window: usize,
    output_token_cap: usize,
) -> FlowCompactionPolicy {
    let threshold_ratio = flow
        .compaction_threshold_ratio
        .unwrap_or_else(|| default_compaction_threshold_ratio_for_scope(&flow.scope))
        .clamp(0.1, 1.0);
    let threshold_tokens = ((max_tokens_per_flow as f32) * threshold_ratio) as u64;

    // Compaction keeps the newest `keep_turns` user turns and summarises the
    // rest, so it can only run while fewer turns are kept than the history
    // limit leaves (that limit drops older turns first). Default: half the
    // limit; a configured value is clamped below it (the old scope defaults,
    // 24–60, were always above it, so compaction never ran).
    let history_limit = resolve_history_turn_limit(flow);
    let keep_turns = flow
        .compaction_keep_turns
        .map(|v| v as usize)
        .unwrap_or(history_limit / 2)
        .min(history_limit.saturating_sub(1))
        .max(1);

    let max_input_budget = crate::application::chat::prompt_budget::compute_total_input_budget(
        context_window,
        output_token_cap,
        max_tokens_per_flow,
    );
    let summary_max_tokens = flow
        .compaction_summary_max_tokens
        .unwrap_or_else(|| default_compaction_summary_max_tokens(max_input_budget));

    FlowCompactionPolicy {
        threshold_tokens: threshold_tokens.max(1),
        keep_turns,
        summary_max_tokens,
    }
}

fn default_compaction_summary_max_tokens(max_input_budget: usize) -> u32 {
    let target = ((max_input_budget as f32) * 0.15).round() as usize;
    let max_cap = (max_input_budget / 3).clamp(256, 4096);
    target.clamp(128, max_cap) as u32
}

fn default_compaction_threshold_ratio_for_scope(scope: &str) -> f32 {
    match scope {
        "main" => 0.88,
        "per-group" => 0.86,
        "per-pipe-sender" => 0.84,
        _ => 0.82,
    }
}

// ---------------------------------------------------------------------------
// Flow compaction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
pub(crate) struct CompactionOutcome {
    applied: bool,
    compacted_messages: usize,
}

pub(crate) async fn maybe_compact_flow(
    flow_key: &str,
    messages: &mut Vec<Message>,
    flow_token_usage: &mut u64,
    policy: FlowCompactionPolicy,
    phase: &str,
) -> Result<CompactionOutcome> {
    if messages.is_empty() {
        return Ok(CompactionOutcome::default());
    }

    if *flow_token_usage < policy.threshold_tokens {
        return Ok(CompactionOutcome::default());
    }

    let Some(split_idx) = compaction_split_index(messages, policy.keep_turns) else {
        return Ok(CompactionOutcome::default());
    };

    if split_idx == 0 {
        return Ok(CompactionOutcome::default());
    }

    let compacted_slice = &messages[..split_idx];
    if !compacted_slice.iter().any(|m| matches!(m.role, Role::User)) {
        return Ok(CompactionOutcome::default());
    }
    let compaction_source = build_compaction_source(compacted_slice);
    let raw_summary = crate::application::chat::prompt_budget::truncate_to_token_budget(
        &compaction_source,
        policy.summary_max_tokens as usize,
    );
    let summary = crate::application::chat::prompt_budget::truncate_to_token_budget(
        raw_summary.trim(),
        policy.summary_max_tokens as usize,
    );

    let compacted_messages = compacted_slice.len();
    let summary_message = Message {
        role: Role::Assistant,
        content: format!(
            "[Flow compaction summary]\n{}\n\n[compacted_messages={}, phase={}]",
            summary.trim(),
            compacted_messages,
            phase
        ),
        tool_call_id: None,
        tool_calls: None,
    };

    messages.drain(0..split_idx);
    messages.insert(0, summary_message);
    *flow_token_usage = messages
        .iter()
        .map(|m| estimate_tokens_approx_min1(&m.content) as u64)
        .sum();

    info!(
        flow_key = %flow_key,
        phase,
        compacted_messages,
        flow_tokens = *flow_token_usage,
        threshold_tokens = policy.threshold_tokens,
        "Applied flow compaction"
    );

    Ok(CompactionOutcome {
        applied: true,
        compacted_messages,
    })
}

fn compaction_split_index(messages: &[Message], keep_turns: usize) -> Option<usize> {
    if keep_turns == 0 || messages.is_empty() {
        return Some(messages.len());
    }

    let mut seen_user_turns = 0usize;
    for (idx, message) in messages.iter().enumerate().rev() {
        if matches!(message.role, Role::User) {
            seen_user_turns += 1;
            if seen_user_turns == keep_turns {
                return Some(idx);
            }
        }
    }

    None
}

fn build_compaction_source(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|msg| format!("[{}] {}", role_label(&msg.role), msg.content))
        .collect::<Vec<_>>()
        .join("\n")
}

fn role_label(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(text: &str, role: Role) -> Message {
        Message {
            role,
            content: text.to_string(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    /// `keep_turns` stays below the history limit: half of it by default, a
    /// larger configured value is clamped (else compaction never ran).
    #[test]
    fn keep_turns_stays_below_the_history_limit() {
        let flow = FlowConfig::default(); // per-sender: 20 turns of history
        let p = resolve_flow_compaction_policy(&flow, 100_000, 200_000, 8_000);
        assert_eq!(p.keep_turns, 10);
        let flow = FlowConfig {
            compaction_keep_turns: Some(30),
            ..FlowConfig::default()
        };
        let p = resolve_flow_compaction_policy(&flow, 100_000, 200_000, 8_000);
        assert_eq!(p.keep_turns, 19);
    }

    /// On defaults, a long flow over the threshold is compacted after the
    /// history limit ran — and its token count drops back.
    #[tokio::test]
    async fn default_policy_compacts_a_long_flow() {
        let flow = FlowConfig::default();
        let policy = resolve_flow_compaction_policy(&flow, 1_000, 200_000, 8_000);
        let mut messages = Vec::new();
        for i in 0..25 {
            messages.push(turn(&format!("question {i}"), Role::User));
            messages.push(turn(&format!("answer {i}"), Role::Assistant));
        }
        enforce_history_turn_limit(&mut messages, resolve_history_turn_limit(&flow));
        let mut usage = 950; // over 82 % of 1 000
        let out = maybe_compact_flow("f", &mut messages, &mut usage, policy, "test")
            .await
            .unwrap();
        assert!(out.applied, "compaction ran");
        assert!(messages[0].content.starts_with("[Flow compaction summary]"));
        let kept_users = messages
            .iter()
            .filter(|m| matches!(m.role, Role::User))
            .count();
        assert_eq!(kept_users, policy.keep_turns);
        assert!(
            usage < 950,
            "usage recomputed from the kept messages: {usage}"
        );
    }
}
