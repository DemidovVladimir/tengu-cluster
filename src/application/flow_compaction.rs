use crate::application::ports::FlowStorePort;
use crate::domain::chat::FlowCompactionPolicy;
use anyhow::Result;
use tengu_core::token::estimate_tokens_approx_min1;
use tengu_core::types::{Message, Role};
use tengu_core::Refiner;
use tracing::info;

#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)]
pub(crate) struct CompactionOutcome {
    applied: bool,
    compacted_messages: usize,
}

pub(crate) async fn maybe_compact_flow(
    flow_store: &dyn FlowStorePort,
    flow_key: &str,
    agent_id: &str,
    messages: &mut Vec<Message>,
    flow_token_usage: &mut u64,
    refiner: &dyn Refiner,
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
    let raw_summary = match refiner
        .summarize(&compaction_source, policy.summary_max_tokens)
        .await
    {
        Ok(text) if !text.trim().is_empty() => text,
        _ => crate::application::prompt_budget::truncate_to_token_budget(
            &compaction_source,
            policy.summary_max_tokens as usize,
        ),
    };
    let summary = crate::application::prompt_budget::truncate_to_token_budget(
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
    messages.insert(0, summary_message.clone());
    *flow_token_usage = messages
        .iter()
        .map(|m| estimate_tokens_approx_min1(&m.content) as u64)
        .sum();

    flow_store.append_message(flow_key, agent_id, &summary_message)?;
    info!(
        flow_key = %flow_key,
        phase,
        compacted_messages,
        flow_tokens = *flow_token_usage,
        threshold_tokens = policy.threshold_tokens,
        "Applied flow compaction summary"
    );

    Ok(CompactionOutcome {
        applied: true,
        compacted_messages,
    })
}

pub(crate) fn compaction_split_index(messages: &[Message], keep_turns: usize) -> Option<usize> {
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
