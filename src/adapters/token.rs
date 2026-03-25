//! Shared token-estimation helpers.

/// Approximate token count as `text_length / 4`.
pub fn estimate_tokens_approx(text: &str) -> usize {
    text.len() / 4
}

/// Approximate token count with a minimum of `1`.
pub fn estimate_tokens_approx_min1(text: &str) -> usize {
    estimate_tokens_approx(text).max(1)
}
