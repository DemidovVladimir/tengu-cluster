//! Shared token-estimation helpers.

/// Approximate token count as `text_length / 4`.
pub fn estimate_tokens_approx(text: &str) -> usize {
    text.len() / 4
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
