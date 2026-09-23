//! Prompt budgeting helpers for runtime turns.

use crate::domain::message::Message;
use crate::domain::session::HistoryAssembly;
use crate::domain::token::estimate_tokens_approx_min1;

/// Assemble newest contiguous history suffix that fits the token budget.
pub(crate) fn assemble_recent_history(
    messages: &[Message],
    history_budget: usize,
) -> HistoryAssembly {
    const MAX_HISTORY_MESSAGES: usize = 20;

    if history_budget == 0 || messages.is_empty() {
        return HistoryAssembly::default();
    }

    let mut selected_rev: Vec<Message> = Vec::new();
    let mut used = 0usize;
    let start = messages.len().saturating_sub(MAX_HISTORY_MESSAGES);
    let recent = &messages[start..];

    for msg in recent.iter().rev() {
        let msg_tokens = estimate_tokens_approx_min1(&msg.content);
        if used + msg_tokens > history_budget {
            break;
        }
        used += msg_tokens;
        selected_rev.push(msg.clone());
    }

    selected_rev.reverse();
    HistoryAssembly {
        dropped_messages: recent.len().saturating_sub(selected_rev.len()),
        messages: selected_rev,
        used_tokens: used,
    }
}

/// Truncate string content using the shared `~4 chars/token` approximation.
pub(crate) fn truncate_to_token_budget(content: &str, max_tokens: usize) -> String {
    let max_chars = max_tokens.saturating_mul(4);
    if content.len() <= max_chars {
        content.to_string()
    } else {
        let mut truncated = content.chars().take(max_chars).collect::<String>();
        truncated.push_str("\n\n[truncated]");
        truncated
    }
}

/// Reserve output tokens from explicit engine output cap with safety headroom.
pub(crate) fn reserved_output_tokens(context_window: usize, output_token_cap: usize) -> usize {
    if context_window == 0 {
        return 0;
    }

    let capped_output = output_token_cap.max(1).min(context_window);
    let headroom = (capped_output / 4).max(64);
    let adaptive_floor = (context_window / 50).clamp(64, 2_048);

    capped_output
        .saturating_add(headroom)
        .max(adaptive_floor)
        .min(context_window)
}

/// Compute input budget after output reserve and static system prompt footprint.
pub(crate) fn compute_base_input_budget(
    context_window: usize,
    output_token_cap: usize,
    system_prompt: &str,
    remaining_flow_tokens: u64,
) -> usize {
    let total_budget =
        compute_total_input_budget(context_window, output_token_cap, remaining_flow_tokens);
    let system_tokens = estimate_tokens_approx_min1(system_prompt);
    total_budget.saturating_sub(system_tokens)
}

/// Compute maximum input budget before prompt-bucket allocation.
pub(crate) fn compute_total_input_budget(
    context_window: usize,
    output_token_cap: usize,
    remaining_flow_tokens: u64,
) -> usize {
    let reserved_output = reserved_output_tokens(context_window, output_token_cap);
    context_window
        .saturating_sub(reserved_output)
        .min(remaining_flow_tokens as usize)
}
