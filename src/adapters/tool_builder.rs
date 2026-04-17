//! Tool infrastructure: path utilities and UI helpers shared by plugins and
//! the MCP bridge.

use crate::adapters::types::ToolCall;
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

// ── Path utilities ──────────────────────────────────────────────────────

pub fn expand_tilde(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if s.starts_with("~/") {
        if let Some(home) = dirs_next::home_dir() {
            return home.join(&s[2..]);
        }
    }
    path.to_path_buf()
}

pub fn validate_path(workspace: &Path, requested: &str) -> Result<PathBuf> {
    let workspace_canonical = workspace
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("Workspace directory not found: {}", e))?;

    let target = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        workspace.join(requested)
    };

    if target.exists() {
        let canonical = target
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("Cannot resolve path: {}", e))?;
        if !canonical.starts_with(&workspace_canonical) {
            bail!("Path escapes workspace: {}", requested);
        }
        return Ok(canonical);
    }

    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Invalid path: no parent directory"))?;
    if !parent.exists() {
        let mut ancestor = parent.to_path_buf();
        while !ancestor.exists() {
            ancestor = match ancestor.parent() {
                Some(p) => p.to_path_buf(),
                None => bail!("No valid ancestor directory for path: {}", requested),
            };
        }
        let ancestor_canonical = ancestor.canonicalize()?;
        if !ancestor_canonical.starts_with(&workspace_canonical) {
            bail!("Path escapes workspace: {}", requested);
        }
    } else {
        let parent_canonical = parent.canonicalize()?;
        if !parent_canonical.starts_with(&workspace_canonical) {
            bail!("Path escapes workspace: {}", requested);
        }
    }

    Ok(target)
}

// ── UI helpers ──────────────────────────────────────────────────────────

pub(crate) fn build_tool_activity_text(
    call: &ToolCall,
) -> (String, Option<String>) {
    let title = prettify_tool_name(&call.name);
    let detail = summarize_tool_args_for(&call.name, &call.arguments);
    let detail = if detail.is_empty() {
        None
    } else {
        Some(detail)
    };
    (title, detail)
}

/// Build a short human-readable summary of tool arguments for activity lines.
///
/// Tool-aware: uses the tool name to pick the most informative arguments
/// (e.g. URL for http_request, destination for sign_and_send_transaction).
fn summarize_tool_args_for(tool_name: &str, args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(m) if !m.is_empty() => m,
        _ => return String::new(),
    };

    match tool_name {
        "http_request" => {
            let method = obj.get("method").and_then(|v| v.as_str()).unwrap_or("?");
            let url = obj.get("url").and_then(|v| v.as_str()).unwrap_or("?");
            format!("{} {}", method, truncate_detail(url, 100))
        }
        "sign_and_send_transaction" => {
            let to = obj.get("to").and_then(|v| v.as_str()).unwrap_or("?");
            let chain = obj.get("chain_id").and_then(|v| v.as_u64());
            match chain {
                Some(id) => format!("to={} chain={}", to, id),
                None => format!("to={}", to),
            }
        }
        _ => summarize_tool_args(args),
    }
}

/// Generic argument summary fallback for tools without special handling.
fn summarize_tool_args(args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(m) if !m.is_empty() => m,
        _ => return String::new(),
    };

    if obj.len() == 1 {
        if let Some(val) = obj.values().next().and_then(|v| v.as_str()) {
            return truncate_detail(val, 120);
        }
    }

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

    let parts: Vec<String> = obj
        .iter()
        .filter_map(|(k, v)| {
            v.as_str()
                .map(|s| format!("{}={}", k, truncate_detail(s, 60)))
        })
        .collect();
    parts.join(" ")
}

/// Convert "run_command" → "Run Command".
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
