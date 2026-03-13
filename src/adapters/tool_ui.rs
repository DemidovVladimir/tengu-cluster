//! Shared UI helpers for tool approval dialogs and activity summaries.
//!
//! Generic implementations that work from tool metadata — no tool name matching.
//! Used by both TUI and Telegram adapters.

use tengu_core::types::ToolCall;

/// Build title, description, and preview text for a tool approval dialog.
///
/// Works generically from the tool call's arguments — no hardcoded tool names.
pub(crate) fn build_approval_text(call: &ToolCall) -> (String, String, String) {
    let title = prettify_tool_name(&call.name);
    let description = format!("Allow '{}' to run?", call.name);
    let preview = format_args_preview(&call.arguments);
    (title, description, preview)
}

/// Build a short human-readable summary of tool arguments for activity lines.
///
/// Shows the first short string argument value, or key=value pairs as fallback.
pub(crate) fn summarize_tool_args(args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(m) if !m.is_empty() => m,
        _ => return String::new(),
    };

    // If there's a single required-looking argument, show just its value.
    if obj.len() == 1 {
        if let Some(val) = obj.values().next().and_then(|v| v.as_str()) {
            return truncate_detail(val, 120);
        }
    }

    // Try to find the most compact meaningful argument (skip long content fields).
    let mut short_args: Vec<(&str, &str)> = obj
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.as_str(), s)))
        .collect();
    short_args.sort_by_key(|(_, v)| v.len());

    if let Some(&(_, val)) = short_args.first() {
        if val.len() <= 120 {
            return truncate_detail(val, 120);
        }
    }

    // Fallback: show all as key=value pairs.
    let parts: Vec<String> = obj
        .iter()
        .filter_map(|(k, v)| {
            v.as_str()
                .map(|s| format!("{}={}", k, truncate_detail(s, 60)))
        })
        .collect();
    parts.join(" ")
}

/// Maximum number of characters to show in the approval preview.
const PREVIEW_MAX_CHARS: usize = 300;

/// Format tool arguments as a readable preview for approval dialogs.
///
/// Long values (e.g., multi-line curl commands) are truncated to keep the
/// dialog manageable — the agent should explain the action beforehand.
fn format_args_preview(args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(m) if !m.is_empty() => m,
        _ => return String::new(),
    };

    let mut out = String::new();
    for (k, v) in obj {
        let s = match v.as_str() {
            Some(s) => s,
            None => continue,
        };
        if !out.is_empty() {
            out.push('\n');
        }
        let line = format!("{}: {}", k, s);
        let remaining = PREVIEW_MAX_CHARS.saturating_sub(out.len());
        if remaining == 0 {
            break;
        }
        out.push_str(&truncate_detail(&line, remaining));
    }
    out
}

/// Convert "run_command" → "Run Command", "write_file" → "Write File".
fn prettify_tool_name(name: &str) -> String {
    name.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Truncate a display string, appending "…" if it exceeds the limit.
fn truncate_detail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prettify_tool_name_basic() {
        assert_eq!(prettify_tool_name("run_command"), "Run Command");
        assert_eq!(prettify_tool_name("write_file"), "Write File");
        assert_eq!(prettify_tool_name("read_file"), "Read File");
        assert_eq!(prettify_tool_name("remember"), "Remember");
    }

    #[test]
    fn summarize_single_arg() {
        let args = json!({"command": "ls -la"});
        assert_eq!(summarize_tool_args(&args), "ls -la");
    }

    #[test]
    fn summarize_multi_arg_picks_shortest() {
        let args = json!({"path": "foo.txt", "content": "a very long content string that should not be shown first"});
        let summary = summarize_tool_args(&args);
        assert_eq!(summary, "foo.txt");
    }

    #[test]
    fn approval_text_generic() {
        let call = ToolCall {
            id: "1".into(),
            name: "run_command".into(),
            arguments: json!({"command": "cargo build"}),
        };
        let (title, desc, preview) = build_approval_text(&call);
        assert_eq!(title, "Run Command");
        assert!(desc.contains("run_command"));
        assert!(preview.contains("cargo build"));
    }
}
