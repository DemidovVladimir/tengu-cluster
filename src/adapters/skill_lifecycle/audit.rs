//! Append-only audit log for skill lifecycle ops (`install`, `remove`, `export`).
//!
//! On-disk: `<workspace>/skills/.audit.jsonl` — one JSON object per line.
//! Append uses `OpenOptions::append(true)`; readers parse line-by-line and
//! tolerate missing files / partial entries.

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AuditEntry {
    pub ts: String, // ISO 8601 UTC
    pub op: String, // "install" | "remove" | "export"
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Helper for callers that don't already have a timestamp string.
pub(crate) fn now_ts() -> String {
    Utc::now().to_rfc3339()
}

fn audit_path(workspace: &Path) -> PathBuf {
    workspace.join("skills").join(".audit.jsonl")
}

/// Append a single entry. Creates the parent directory + file if missing.
/// If `entry.ts` is empty, fills it from `now_ts()`.
pub(crate) fn append(workspace: &Path, mut entry: AuditEntry) -> Result<()> {
    if entry.ts.is_empty() {
        entry.ts = now_ts();
    }
    let path = audit_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create audit dir {:?}", parent))?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("open audit log {:?}", path))?;
    let line = serde_json::to_string(&entry).context("serialize audit entry")?;
    writeln!(f, "{line}").context("write audit line")?;
    Ok(())
}

/// Return up to `limit` most-recent entries, oldest-first within the
/// returned window. Missing log file → `Ok(vec![])`. Malformed lines are
/// skipped with a `tracing::warn!`.
//
// Reserved for a future `tengu skill audit` CLI verb. Tests cover the
// function; not yet wired by any handler.
#[allow(dead_code)]
pub(crate) fn read_recent(workspace: &Path, limit: usize) -> Result<Vec<AuditEntry>> {
    let path = audit_path(workspace);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let f = std::fs::File::open(&path).with_context(|| format!("open audit log {:?}", path))?;
    let mut entries: Vec<AuditEntry> = Vec::new();
    for line in BufReader::new(f).lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "audit: read line failed; skipping");
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<AuditEntry>(&line) {
            Ok(e) => entries.push(e),
            Err(e) => {
                tracing::warn!(error = %e, line = %line, "audit: malformed entry; skipping");
            }
        }
    }
    if entries.len() > limit {
        let drop = entries.len() - limit;
        entries.drain(..drop);
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;
    use tempfile::TempDir;

    fn entry(name: &str, op: &str) -> AuditEntry {
        AuditEntry {
            ts: String::new(),
            op: op.into(),
            name: name.into(),
            verdict: None,
            source: None,
            sha256: None,
        }
    }

    #[test]
    fn roundtrip_append_and_read() {
        let dir = TempDir::new().unwrap();
        append(dir.path(), entry("a", "install")).unwrap();
        append(dir.path(), entry("b", "remove")).unwrap();
        append(dir.path(), entry("c", "export")).unwrap();

        let got = read_recent(dir.path(), 10).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].name, "a");
        assert_eq!(got[1].name, "b");
        assert_eq!(got[2].name, "c");
        assert_eq!(got[0].op, "install");
    }

    #[test]
    fn read_recent_limits_to_n() {
        let dir = TempDir::new().unwrap();
        for n in 0..5 {
            append(dir.path(), entry(&format!("s{n}"), "install")).unwrap();
        }
        let got = read_recent(dir.path(), 2).unwrap();
        assert_eq!(got.len(), 2);
        // oldest-first within the window: entries 4 (index 3) and 5 (index 4)
        // i.e. the last two appended.
        assert_eq!(got[0].name, "s3");
        assert_eq!(got[1].name, "s4");
    }

    #[test]
    fn creates_audit_dir_on_first_append() {
        let dir = TempDir::new().unwrap();
        // No skills/ subdir at all.
        assert!(!dir.path().join("skills").exists());

        append(dir.path(), entry("first", "install")).unwrap();

        let log = dir.path().join("skills").join(".audit.jsonl");
        assert!(log.is_file(), "audit log should exist at {:?}", log);
    }

    #[test]
    fn read_recent_on_missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let got = read_recent(dir.path(), 10).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn now_ts_is_rfc3339() {
        let ts = now_ts();
        let parsed = DateTime::parse_from_rfc3339(&ts);
        assert!(parsed.is_ok(), "now_ts() should parse as RFC3339, got: {ts}");
    }
}
