//! Shared helpers for `Tool::execute`: JSON argument extraction and
//! workspace path validation.
//!
//! Centralises the repetitive `args.get(k).and_then(..).ok_or_else(..)`
//! pattern. Each helper returns `anyhow::Result<T>` with a consistent
//! error message shape: `"<tool>: '<key>' is required (<type>)"`.

use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Extract a required string argument. Returns an error keyed on `tool_name`
/// and `key` when the field is missing or not a string.
pub(crate) fn require_str<'a>(args: &'a Value, tool_name: &str, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("{}: '{}' is required (string)", tool_name, key))
}

/// Extract a required i64 argument. JSON numbers round-trip through `as_i64`.
#[allow(dead_code)]
pub(crate) fn require_i64(args: &Value, tool_name: &str, key: &str) -> Result<i64> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow!("{}: '{}' is required (integer)", tool_name, key))
}

/// Extract a required boolean argument.
#[allow(dead_code)]
pub(crate) fn require_bool(args: &Value, tool_name: &str, key: &str) -> Result<bool> {
    args.get(key)
        .and_then(|v| v.as_bool())
        .ok_or_else(|| anyhow!("{}: '{}' is required (boolean)", tool_name, key))
}

/// Resolve `requested` against `workspace` and refuse anything that escapes it.
/// The result is the path the OS will open (`domain::scope::resolve_path`):
/// symlinks of the existing part followed, `..` applied — so
/// `new/../../x` is caught before a writer creates `new`.
pub fn validate_path(workspace: &Path, requested: &str) -> Result<PathBuf> {
    let workspace_canonical = workspace
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("Workspace directory not found: {}", e))?;

    let target = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        workspace.join(requested)
    };

    let resolved = crate::domain::scope::resolve_path(&target)
        .map_err(|e| anyhow!("Cannot resolve path '{}': {:#}", requested, e))?;
    if !resolved.starts_with(&workspace_canonical) {
        bail!("Path escapes workspace: {}", requested);
    }
    Ok(resolved)
}

/// [`validate_path`] for a tool that writes where the model says: also
/// refuses what `domain::scope::protected_write` names (`.tengu/`,
/// `.claude/`, `.git/`, `CLAUDE.md`, `AGENTS.md`, … at any depth) and skill
/// directories (`skills/` at any depth — an LLM-written skill would load on
/// the next scan). Checked on the resolved path relative to the workspace,
/// so `x/../.tengu/…`, a symlinked directory or `Skills/` on a case-folding
/// filesystem cannot slip past. Every sandbox.
pub fn validate_write_path(workspace: &Path, requested: &str) -> Result<PathBuf> {
    let target = validate_path(workspace, requested)?;
    let root = workspace
        .canonicalize()
        .map_err(|e| anyhow!("Workspace directory not found: {}", e))?;
    let rel = target.strip_prefix(&root).unwrap_or(&target);
    if let Some(why) = crate::domain::scope::protected_write(rel) {
        bail!("Writing '{}' is not allowed: {}", requested, why);
    }
    if rel
        .parent()
        .is_some_and(|dirs| dirs.iter().any(|d| d.eq_ignore_ascii_case("skills")))
    {
        bail!("Writing to skill directories is not allowed");
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn require_str_ok() {
        let v = json!({ "x": "hi" });
        assert_eq!(require_str(&v, "t", "x").unwrap(), "hi");
    }

    #[test]
    fn require_str_missing() {
        let v = json!({});
        let err = require_str(&v, "t", "x").unwrap_err().to_string();
        assert!(err.contains("t: 'x' is required"), "got: {}", err);
    }

    #[test]
    fn require_str_wrong_type() {
        let v = json!({ "x": 5 });
        assert!(require_str(&v, "t", "x").is_err());
    }

    #[test]
    fn require_i64_ok() {
        let v = json!({ "n": 7 });
        assert_eq!(require_i64(&v, "t", "n").unwrap(), 7);
    }

    #[test]
    fn require_bool_ok() {
        let v = json!({ "b": true });
        assert!(require_bool(&v, "t", "b").unwrap());
    }

    /// `..` after a directory that does not exist yet: the OS would land
    /// outside the workspace once a writer creates it.
    #[test]
    fn validate_path_refuses_dotdot_through_a_missing_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        for escape in ["new/../../outside.txt", "a/b/../../../x/y.txt"] {
            let err = validate_path(&ws, escape).unwrap_err().to_string();
            assert!(err.contains("Path escapes workspace"), "{escape}: {err}");
        }
        let ok = validate_path(&ws, "new/../inside.txt").unwrap();
        assert_eq!(ok, ws.canonicalize().unwrap().join("inside.txt"));
    }

    /// Writers refuse tengu's state, the nested CLI's config and
    /// instruction files and skill directories — after resolution, in any
    /// case; ordinary files pass.
    #[test]
    fn validate_write_path_refuses_protected_targets() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join(".tengu")).unwrap();
        for bad in [
            ".tengu/observations.db",
            "notes/../.tengu/cache.db",
            ".claude/settings.json",
            "sub/.claude/settings.local.json",
            "CLAUDE.md",
            "docs/claude.md",
            "AGENTS.md",
            "x/CLAUDE.local.md",
            ".mcp.json",
            ".git/hooks/pre-commit",
            "skills/evil/SKILL.md",
            "Skills/evil/SKILL.md",
            "a/skills/b.md",
        ] {
            assert!(validate_write_path(ws, bad).is_err(), "{bad} allowed");
        }
        let abs = ws.join(".tengu/x").display().to_string();
        assert!(validate_write_path(ws, &abs).is_err());
        for good in ["answer.txt", "out/new.txt", "skills.md", "notes/agents.txt"] {
            validate_write_path(ws, good).unwrap_or_else(|e| panic!("{good}: {e:#}"));
        }
    }

    /// A symlinked directory resolves first: `link/…` into `.tengu` is
    /// `.tengu/…`.
    #[cfg(unix)]
    #[test]
    fn validate_write_path_follows_symlinked_dirs() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join(".tengu")).unwrap();
        std::os::unix::fs::symlink(ws.join(".tengu"), ws.join("state")).unwrap();
        let err = validate_write_path(ws, "state/observations.db")
            .unwrap_err()
            .to_string();
        assert!(err.contains("`.tengu/`"), "{err}");
    }
}
