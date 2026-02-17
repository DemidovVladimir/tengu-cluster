//! Shared token-estimation helpers used across runtime and crates.
//!
//! These functions intentionally use a lightweight approximation (`~4 chars/token`)
//! to avoid tokenizer dependencies in the core path.
//!
//! Potential use case:
//! Estimate prompt buckets fast before model call to enforce hard context limits.

/// Approximate token count as `text_length / 4`.
pub fn estimate_tokens_approx(text: &str) -> usize {
    text.len() / 4
}

/// Approximate token count with a minimum of `1`.
pub fn estimate_tokens_approx_min1(text: &str) -> usize {
    estimate_tokens_approx(text).max(1)
}

/// Approximate token count as `u32`, clamped to `u32::MAX`.
pub fn estimate_tokens_approx_u32(text: &str) -> u32 {
    estimate_tokens_approx(text).min(u32::MAX as usize) as u32
}

/// Approximate token count with minimum `1`, returned as `u32`.
pub fn estimate_tokens_approx_min1_u32(text: &str) -> u32 {
    estimate_tokens_approx_min1(text).min(u32::MAX as usize) as u32
}
