//! Shared token-estimation helpers.

/// Chars (bytes) per token of the `len / 4` estimate below.
pub const CHARS_PER_TOKEN: usize = 4;

/// One tool result may fill `1 / TOOL_RESULT_WINDOW_DIVISOR` of a
/// small-window model's context (`engine = "local"`).
pub const TOOL_RESULT_WINDOW_DIVISOR: usize = 8;

/// Approximate token count as `text_length / 4`.
pub fn estimate_tokens_approx(text: &str) -> usize {
    text.len() / CHARS_PER_TOKEN
}

/// Max chars of one tool result for a model with `context_window` tokens:
/// 1/8 of the window at 4 chars/token — 16 384 → 8 192, 131 072 → 65 536.
/// `LocalEngine`'s `Engine::tool_result_char_cap`.
pub fn tool_result_char_budget(context_window: usize) -> usize {
    (context_window / TOOL_RESULT_WINDOW_DIVISOR).saturating_mul(CHARS_PER_TOKEN)
}

/// Approximate token count with a minimum of `1`.
pub fn estimate_tokens_approx_min1(text: &str) -> usize {
    estimate_tokens_approx(text).max(1)
}

/// Truncate `s` to the largest char-boundary at or below `max` bytes.
///
/// Returns `None` when `s.len() <= max` (no truncation needed).
/// Returns `Some((prefix, end))` when truncation occurs: `prefix` is the
/// byte slice `&s[..end]` and `end` is the actual boundary-aligned cut
/// point, which may be less than `max` if `max` fell inside a multibyte
/// UTF-8 sequence.
///
/// Use this when the caller needs to build a custom suffix (e.g.
/// `"[truncated — showing {} of {} chars]"`). Use `truncate_with_suffix`
/// for the simpler "just append a fixed suffix" case.
pub fn truncate_at_boundary(s: &str, max: usize) -> Option<(&str, usize)> {
    if s.len() <= max {
        return None;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    Some((&s[..end], end))
}

/// Truncate `s` to at most `max` bytes on a char boundary, appending
/// `suffix` only when truncation actually occurs. When `s` is already
/// within `max`, returns `s.to_string()` unchanged (no suffix).
pub fn truncate_with_suffix(s: &str, max: usize, suffix: &str) -> String {
    match truncate_at_boundary(s, max) {
        None => s.to_string(),
        Some((prefix, _)) => format!("{}{}", prefix, suffix),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_result_budget_is_an_eighth_of_the_window() {
        for (window, chars) in [
            (16_384, 8_192),
            (32_768, 16_384),
            (131_072, 65_536),
            (1_000_000, 500_000),
            (7, 0),
        ] {
            assert_eq!(tool_result_char_budget(window), chars, "window {window}");
        }
        // Round trip through the estimate: a full-budget result is 1/8 of the window.
        let full = "x".repeat(tool_result_char_budget(16_384));
        assert_eq!(
            estimate_tokens_approx(&full),
            16_384 / TOOL_RESULT_WINDOW_DIVISOR
        );
    }
}
