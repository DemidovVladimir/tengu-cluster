//! Shared helpers for `Tool::execute`: JSON argument extraction and
//! workspace path validation.
//!
//! | Helpers | For | Rule |
//! |---|---|---|
//! | `require_str` / `require_i64` / `require_bool` | any tool | the key on `args`; `"<tool>: '<key>' is required (<type>)"` |
//! | `opt_u64_arg` | the Hyperliquid and Solana families | absent / `null` ⇒ `None`, else a non-negative integer |
//! | `object_args` · `field` · `opt_str` · `req_str` · `opt_bool` · `whole` · `opt_time` | the strict families (xm, xlab, soe, sources) | an unknown key is an error; `null` counts as absent; strings are trimmed and never blank |
//! | `validate_path` / `validate_write_path` | workspace readers / writers | resolved, inside the workspace, protected names refused |

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

use crate::domain::time::parse_time;

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

/// An optional non-negative integer (absent / `null` ⇒ `None`).
pub(crate) fn opt_u64_arg(args: &Value, tool: &str, key: &str) -> Result<Option<u64>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| anyhow!("{tool}: '{key}' must be a non-negative integer, got {v}")),
    }
}

// ── Strict object arguments (module table) ─────────────────────────────

/// `args` as an object whose every key is in `allowed`.
pub(crate) fn object_args<'a>(
    tool: &str,
    args: &'a Value,
    allowed: &[&str],
) -> Result<&'a Map<String, Value>> {
    let o = args
        .as_object()
        .ok_or_else(|| anyhow!("{tool}: arguments must be a JSON object"))?;
    let unknown: Vec<&str> = o
        .keys()
        .map(String::as_str)
        .filter(|k| !allowed.contains(k))
        .collect();
    if !unknown.is_empty() {
        bail!(
            "{tool}: unknown argument(s) {unknown:?} (allowed: {})",
            allowed.join(", ")
        );
    }
    Ok(o)
}

/// The argument `key`, absent when missing or `null`.
pub(crate) fn field<'a>(o: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    o.get(key).filter(|v| !v.is_null())
}

/// An optional non-empty string argument, trimmed.
pub(crate) fn opt_str<'a>(
    tool: &str,
    o: &'a Map<String, Value>,
    key: &str,
) -> Result<Option<&'a str>> {
    match field(o, key) {
        None => Ok(None),
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.trim())),
        Some(v) => bail!("{tool}: '{key}' must be a non-empty string, got {v}"),
    }
}

/// [`opt_str`], required.
pub(crate) fn req_str<'a>(tool: &str, o: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    opt_str(tool, o, key)?.ok_or_else(|| anyhow!("{tool}: '{key}' is required (string)"))
}

/// An optional boolean argument.
pub(crate) fn opt_bool(tool: &str, o: &Map<String, Value>, key: &str) -> Result<Option<bool>> {
    match field(o, key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(v) => bail!("{tool}: '{key}' must be a boolean, got {v}"),
    }
}

/// A whole number from a JSON integer or an integral float.
pub(crate) fn whole(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| {
        v.as_f64()
            .filter(|x| x.is_finite() && x.fract() == 0.0 && x.abs() < 9.0e15)
            .map(|x| x as i64)
    })
}

/// A time argument (`from` / `to`): a string `parse_time` takes (RFC 3339,
/// a UTC date, epoch ms) or an epoch-ms number.
pub(crate) fn opt_time(tool: &str, o: &Map<String, Value>, key: &str) -> Result<Option<i64>> {
    match field(o, key) {
        None => Ok(None),
        Some(Value::String(s)) => parse_time(s)
            .map(Some)
            .map_err(|e| anyhow!("{tool}: '{key}': {e}")),
        Some(v) => whole(v).map(Some).ok_or_else(|| {
            anyhow!("{tool}: '{key}' must be epoch ms, RFC 3339 or a date (2026-09-25), got {v}")
        }),
    }
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
/// filesystem cannot slip past. Every sandbox; a `hardened` one (the
/// caller's `AgentConfig::hardened`) also refuses the system-prompt files
/// (`domain::scope::protected_write_in`: `MEMORY.md`, `USER.md`,
/// `IDENTITY.md`, `PROFILE.md`, `CONTEXT.md`).
pub fn validate_write_path(workspace: &Path, requested: &str, hardened: bool) -> Result<PathBuf> {
    let target = validate_path(workspace, requested)?;
    let root = workspace
        .canonicalize()
        .map_err(|e| anyhow!("Workspace directory not found: {}", e))?;
    let rel = target.strip_prefix(&root).unwrap_or(&target);
    if let Some(why) = crate::domain::scope::protected_write_in(rel, hardened) {
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

    /// The strict helpers: unknown keys refused, `null` absent, strings
    /// trimmed and never blank, whole numbers from integral floats.
    #[test]
    fn strict_object_helpers() {
        let v = json!({ "a": "  x ", "b": null, "c": true, "n": 3.0, "t": "2026-09-25" });
        let err = object_args("t", &v, &["a"]).unwrap_err().to_string();
        assert!(err.contains("unknown argument(s)"), "{err}");
        assert!(object_args("t", &json!([]), &[]).is_err());
        let o = object_args("t", &v, &["a", "b", "c", "n", "t"]).unwrap();
        assert_eq!(opt_str("t", o, "a").unwrap(), Some("x"));
        assert_eq!(opt_str("t", o, "b").unwrap(), None);
        assert!(req_str("t", o, "b")
            .unwrap_err()
            .to_string()
            .contains("is required"));
        assert!(opt_str("t", o, "c").is_err());
        let blank = json!({ "a": "   " });
        assert!(opt_str("t", blank.as_object().unwrap(), "a").is_err());
        assert_eq!(opt_bool("t", o, "c").unwrap(), Some(true));
        assert!(opt_bool("t", o, "a").is_err());
        assert_eq!(whole(field(o, "n").unwrap()), Some(3));
        assert_eq!(whole(&json!(3.5)), None);
        assert_eq!(opt_time("t", o, "t").unwrap(), Some(1_790_294_400_000));
        assert_eq!(opt_time("t", o, "n").unwrap(), Some(3));
        assert!(opt_time("t", o, "c").is_err());
    }

    #[test]
    fn opt_u64_arg_rules() {
        let v = json!({ "n": 7, "z": null, "neg": -1 });
        assert_eq!(opt_u64_arg(&v, "t", "n").unwrap(), Some(7));
        assert_eq!(opt_u64_arg(&v, "t", "z").unwrap(), None);
        assert_eq!(opt_u64_arg(&v, "t", "missing").unwrap(), None);
        assert!(opt_u64_arg(&v, "t", "neg").is_err());
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
            for hardened in [false, true] {
                assert!(
                    validate_write_path(ws, bad, hardened).is_err(),
                    "{bad} allowed"
                );
            }
        }
        let abs = ws.join(".tengu/x").display().to_string();
        assert!(validate_write_path(ws, &abs, false).is_err());
        for good in ["answer.txt", "out/new.txt", "skills.md", "notes/agents.txt"] {
            for hardened in [false, true] {
                validate_write_path(ws, good, hardened).unwrap_or_else(|e| panic!("{good}: {e:#}"));
            }
        }
    }

    /// The system-prompt files: refused in a hardened sandbox — after
    /// resolution, any case, any depth — and free elsewhere.
    #[test]
    fn validate_write_path_keeps_prompt_files_from_hardened_writers() {
        let tmp = tempfile::TempDir::new().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("notes")).unwrap();
        for f in [
            "MEMORY.md",
            "USER.md",
            "IDENTITY.md",
            "PROFILE.md",
            "CONTEXT.md",
            "identity.md",
            "notes/../Profile.md",
            "project/CONTEXT.md",
        ] {
            let err = validate_write_path(ws, f, true).unwrap_err().to_string();
            assert!(err.contains("hardened sandbox"), "{f}: {err}");
            validate_write_path(ws, f, false).unwrap_or_else(|e| panic!("{f}: {e:#}"));
        }
        let abs = ws.join("MEMORY.md").display().to_string();
        assert!(validate_write_path(ws, &abs, true).is_err());
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
        let err = validate_write_path(ws, "state/observations.db", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`.tengu/`"), "{err}");
        // A file symlink named innocently resolves to the prompt file.
        std::fs::write(ws.join("MEMORY.md"), "").unwrap();
        std::os::unix::fs::symlink(ws.join("MEMORY.md"), ws.join("notes.txt")).unwrap();
        let err = validate_write_path(ws, "notes.txt", true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("hardened sandbox"), "{err}");
    }
}
