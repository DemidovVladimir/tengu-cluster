use crate::domain::chat::FlowCompactionPolicy;

pub(crate) fn resolve_flow_compaction_policy(
    flow: &tengu_core::config::FlowConfig,
    max_tokens_per_flow: u64,
    context_window: usize,
    output_token_cap: usize,
) -> FlowCompactionPolicy {
    let threshold_ratio = flow
        .compaction_threshold_ratio
        .unwrap_or_else(|| default_compaction_threshold_ratio_for_scope(&flow.scope))
        .clamp(0.1, 1.0);
    let threshold_tokens = ((max_tokens_per_flow as f32) * threshold_ratio) as u64;

    let keep_turns = flow
        .compaction_keep_turns
        .map(|v| v.max(1) as usize)
        .unwrap_or_else(|| default_compaction_keep_turns_for_scope(&flow.scope));

    let max_input_budget = crate::application::prompt_budget::compute_total_input_budget(
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

pub(crate) fn default_compaction_threshold_ratio_for_scope(scope: &str) -> f32 {
    match scope {
        "main" => 0.88,
        "per-group" => 0.86,
        "per-pipe-sender" => 0.84,
        _ => 0.82,
    }
}

pub(crate) fn default_compaction_keep_turns_for_scope(scope: &str) -> usize {
    match scope {
        "main" => 60,
        "per-group" => 40,
        "per-pipe-sender" => 32,
        _ => 24,
    }
}
