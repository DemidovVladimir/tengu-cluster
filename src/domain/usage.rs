//! Token-usage bookkeeping from engine `StreamEvent::Usage` frames: per-turn
//! snapshots folded into session totals.

use tracing::debug;

pub(crate) fn absorb_turn_usage_snapshot(
    snapshot: &mut Option<(u32, u32)>,
    input_tokens: u32,
    output_tokens: u32,
) {
    if let Some((prev_input, prev_output)) = *snapshot {
        if input_tokens < prev_input || output_tokens < prev_output {
            debug!(
                prev_input,
                prev_output,
                input_tokens,
                output_tokens,
                "Usage snapshot is non-monotonic; replacing with latest frame"
            );
        }
    }
    *snapshot = Some((input_tokens, output_tokens));
}

pub(crate) fn apply_turn_usage_to_session_totals(
    total_input_tokens: &mut u32,
    total_output_tokens: &mut u32,
    turn_usage_snapshot: Option<(u32, u32)>,
) {
    if let Some((input_tokens, output_tokens)) = turn_usage_snapshot {
        *total_input_tokens = total_input_tokens.saturating_add(input_tokens);
        *total_output_tokens = total_output_tokens.saturating_add(output_tokens);
    }
}
