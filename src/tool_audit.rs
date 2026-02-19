//! Persistent tool-call audit logging for runtime governance and forensics.
//!
//! Potential use case:
//! Keep an append-only JSONL trail of tool-call policy decisions and execution
//! outcomes so operators can inspect what happened across sessions.
//!
//! Migration note:
//! Audit appends are currently called by runtime directly and are scheduled to
//! move behind internal event-bus subscribers as part of `E11`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const TOOL_AUDIT_FILENAME: &str = "tool_calls.jsonl";

/// Persistent append-only JSONL writer for tool audit records.
pub struct ToolAuditStore {
    path: PathBuf,
}

/// One structured tool-call audit event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolAuditEvent {
    /// UNIX timestamp (seconds).
    pub ts_epoch_s: u64,
    /// Flow key associated with this tool event.
    pub flow_key: String,
    /// Agent identity that handled the tool call.
    pub agent_id: String,
    /// Tool call id emitted by model stream.
    pub tool_call_id: String,
    /// Tool name as seen in tool-call lifecycle events.
    pub tool_name: String,
    /// Audit phase (policy/protocol/execute).
    pub phase: String,
    /// Outcome status for this phase.
    pub status: String,
    /// Optional reason or error string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional arguments preview (truncated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments_preview: Option<String>,
    /// Optional result preview (truncated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_preview: Option<String>,
}

impl ToolAuditStore {
    /// Initialize tool audit store under `<home>/state/audit/tool_calls.jsonl`.
    pub fn new(home: &Path) -> Result<Self> {
        let audit_root = home.join("state").join("audit");
        std::fs::create_dir_all(&audit_root).with_context(|| {
            format!(
                "failed to create tool audit root at {}",
                audit_root.display()
            )
        })?;
        Ok(Self {
            path: audit_root.join(TOOL_AUDIT_FILENAME),
        })
    }

    /// Append one tool audit event as JSONL line.
    pub fn append(&self, event: &ToolAuditEvent) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open tool audit file {}", self.path.display()))?;
        let encoded = serde_json::to_string(event)?;
        file.write_all(encoded.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }
}

/// Build a stable timestamp for audit events.
pub fn audit_now_epoch_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Truncate string payload for compact audit storage.
pub fn truncate_audit_text(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push_str("…");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_home() -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "tengu-tool-audit-test-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        home
    }

    #[test]
    fn append_writes_jsonl_event() {
        let home = temp_home();
        let store = ToolAuditStore::new(&home).expect("audit store");
        let event = ToolAuditEvent {
            ts_epoch_s: 1,
            flow_key: "agent:sender".to_string(),
            agent_id: "main".to_string(),
            tool_call_id: "tc-1".to_string(),
            tool_name: "read_file".to_string(),
            phase: "execute".to_string(),
            status: "ok".to_string(),
            reason: None,
            arguments_preview: Some("{\"path\":\"a.txt\"}".to_string()),
            result_preview: Some("hello".to_string()),
        };
        store.append(&event).expect("append");

        let path = home.join("state").join("audit").join(TOOL_AUDIT_FILENAME);
        let content = std::fs::read_to_string(path).expect("read audit file");
        assert!(content.contains("\"tool_name\":\"read_file\""));
        assert!(content.contains("\"status\":\"ok\""));
    }
}
