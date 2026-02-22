//! Prompt budgeting and retrieval-packing helpers for runtime turns.
//!
//! Potential use case:
//! Keep prompt assembly policy centralized so history/retrieval limits stay
//! deterministic across CLI loop, diagnostics, and tests.

use crate::{
    HistoryAssembly, PromptAssemblyReport, RetrievalAssembly, RETRIEVAL_CONTEXT_HEADER,
    RETRIEVAL_CONTEXT_SEPARATOR,
};
use tengu_core::token::estimate_tokens_approx_min1;
use tengu_core::types::Message;
use tengu_core::Lens;
use tengu_memory::RetrievedKnowledge;
use tracing::info;

/// Build bounded static system prompt from workspace identity/profile/context files.
pub(crate) fn build_system_prompt(
    agent_config: &tengu_core::config::AgentConfig,
) -> Option<String> {
    const MAX_FILE_TOKENS: usize = 1200;
    const MAX_TOTAL_TOKENS: usize = 2400;

    let workspace = agent_config.workspace.as_ref()?;
    let mut parts = Vec::new();
    let mut total_tokens = 0usize;

    // Load workspace files in order: IDENTITY.md, PROFILE.md, CONTEXT.md
    for filename in &["IDENTITY.md", "PROFILE.md", "CONTEXT.md"] {
        let path = workspace.join(filename);
        if let Ok(content) = std::fs::read_to_string(&path) {
            if !content.trim().is_empty() {
                let truncated = truncate_to_token_budget(&content, MAX_FILE_TOKENS);
                let chunk = format!("# {}\n\n{}", filename, truncated);
                let chunk_tokens = estimate_tokens_approx_min1(&chunk);
                if total_tokens + chunk_tokens > MAX_TOTAL_TOKENS {
                    break;
                }
                total_tokens += chunk_tokens;
                parts.push(chunk);
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n---\n\n"))
    }
}

/// Assemble newest contiguous history suffix that fits the token budget.
pub(crate) fn assemble_recent_history(
    messages: &[Message],
    history_budget: usize,
) -> HistoryAssembly {
    const MAX_HISTORY_MESSAGES: usize = 120;

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
///
/// This avoids over-reserving on very large context windows while still keeping
/// room for provider overhead and streamed terminal frames.
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
    system_prompt: Option<&str>,
    remaining_flow_tokens: u64,
) -> usize {
    let total_budget =
        compute_total_input_budget(context_window, output_token_cap, remaining_flow_tokens);
    let system_tokens = system_prompt.map(estimate_tokens_approx_min1).unwrap_or(0);
    total_budget.saturating_sub(system_tokens)
}

/// Compute maximum input budget before prompt-bucket allocation.
///
/// Output reserve is derived from effective output cap, not from context ratio.
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

/// Compute retrieval bucket budget from lens settings and overall input budget.
pub(crate) fn compute_retrieval_bucket_budget(
    lens: Lens,
    lens_cfg: &tengu_core::config::LensConfig,
    base_input_budget: usize,
) -> usize {
    if base_input_budget == 0 {
        return 0;
    }

    let desired = match lens {
        Lens::Eco => lens_cfg.eco_max_tokens as usize,
        Lens::Standard => ((base_input_budget as f32) * 0.2) as usize,
        Lens::Precise => ((base_input_budget as f32) * lens_cfg.precise_budget) as usize,
    };
    let hard_cap = (base_input_budget / 2).max(32).min(base_input_budget);
    desired.min(hard_cap).max(32.min(hard_cap))
}

/// Build retrieval context block under a fixed token budget.
pub(crate) fn build_retrieval_block(
    hits: &[RetrievedKnowledge],
    max_tokens: usize,
) -> RetrievalAssembly {
    if hits.is_empty() || max_tokens == 0 {
        return RetrievalAssembly::default();
    }

    let header_tokens = estimate_tokens_approx_min1(RETRIEVAL_CONTEXT_HEADER);
    let separator_tokens = estimate_tokens_approx_min1(RETRIEVAL_CONTEXT_SEPARATOR);
    if header_tokens >= max_tokens {
        return RetrievalAssembly {
            block: None,
            used_tokens: 0,
            dropped_items: hits.len(),
        };
    }

    let mut chunks = Vec::new();
    let mut used = 0usize;
    let mut dropped = 0usize;

    for (idx, hit) in hits.iter().enumerate() {
        let section = format!(
            "[{} | {} | score {:.2}]\n{}",
            hit.source.display(),
            if hit.is_summary { "summary" } else { "full" },
            hit.score,
            hit.content
        );
        let section_tokens = estimate_tokens_approx_min1(&section);
        let additional_tokens = if chunks.is_empty() {
            header_tokens.saturating_add(section_tokens)
        } else {
            separator_tokens.saturating_add(section_tokens)
        };
        if additional_tokens > max_tokens {
            dropped += 1;
            continue;
        }
        if used.saturating_add(additional_tokens) > max_tokens {
            dropped += hits.len().saturating_sub(idx);
            break;
        }
        used = used.saturating_add(additional_tokens);
        chunks.push(section);
    }

    RetrievalAssembly {
        block: if chunks.is_empty() {
            None
        } else {
            Some(format!(
                "{}{}",
                RETRIEVAL_CONTEXT_HEADER,
                chunks.join(RETRIEVAL_CONTEXT_SEPARATOR)
            ))
        },
        used_tokens: if chunks.is_empty() { 0 } else { used },
        dropped_items: dropped,
    }
}

/// Emit per-request prompt budget telemetry grouped by prompt assembly bucket.
pub(crate) fn log_prompt_budget_report(
    flow_key: &str,
    lens: Lens,
    context_window: usize,
    report: &PromptAssemblyReport,
    history_messages_selected: usize,
    retrieval_candidates: usize,
) {
    info!(
        flow_key = %flow_key,
        lens = lens.as_str(),
        context_window,
        output_token_cap = report.output_token_cap,
        total_input_budget = report.total_input_budget,
        reserved_output_tokens = report.reserved_output_tokens,
        flow_budget_remaining = report.flow_budget_remaining,
        system_tokens = report.system_tokens,
        history_tokens = report.history_tokens,
        history_messages_selected,
        dropped_history_messages = report.dropped_history_messages,
        retrieval_tokens = report.retrieval_tokens,
        retrieval_candidates,
        retrieval_budget_requested = report.retrieval_budget_requested,
        retrieval_budget_effective = report.retrieval_budget_effective,
        dropped_retrieval_items = report.dropped_retrieval_items,
        compaction_applied = report.compaction_applied,
        compacted_messages = report.compacted_messages,
        "Prompt budget report"
    );
}

/// Merge static system prompt with dynamic per-turn retrieval context.
pub(crate) fn merge_system_prompt(
    base: Option<String>,
    retrieval_block: Option<&str>,
) -> Option<String> {
    match (base, retrieval_block) {
        (None, None) => None,
        (Some(b), None) => Some(b),
        (None, Some(r)) => Some(r.to_string()),
        (Some(b), Some(r)) => Some(format!("{b}\n\n---\n\n{r}")),
    }
}
