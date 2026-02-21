//! Persistent control-plane assignment audit logging.
//!
//! Potential use case:
//! Keep an append-only JSONL trail for delegated capability assignment approvals
//! and denials so orchestrator actions are auditable across runtime sessions.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

const CONTROL_PLANE_AUDIT_FILENAME: &str = "capability_assignments.jsonl";

/// Persistent append-only JSONL writer for control-plane assignment events.
#[derive(Clone)]
pub struct ControlPlaneAuditStore {
    path: PathBuf,
}

/// One structured delegated-assignment audit event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlPlaneAuditEvent {
    /// UNIX timestamp (seconds).
    pub ts_epoch_s: u64,
    /// Flow key associated with this assignment event.
    pub flow_key: String,
    /// Assignment/handoff identifier.
    pub handoff_id: String,
    /// Orchestrator agent id.
    pub orchestrator_agent_id: String,
    /// Dependent agent id.
    pub dependent_agent_id: String,
    /// Outcome status (`approved`, `denied`, `completed`, `failed`).
    pub status: String,
    /// Optional denial/failure reason text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional requested capabilities snapshot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_capabilities: Vec<String>,
    /// Optional objective/summary string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objective: Option<String>,
}

impl ControlPlaneAuditStore {
    /// Initialize assignment audit store under `<home>/state/audit/capability_assignments.jsonl`.
    pub fn new(home: &Path) -> Result<Self> {
        let audit_root = home.join("state").join("audit");
        std::fs::create_dir_all(&audit_root).with_context(|| {
            format!(
                "failed to create control-plane audit root at {}",
                audit_root.display()
            )
        })?;
        Ok(Self {
            path: audit_root.join(CONTROL_PLANE_AUDIT_FILENAME),
        })
    }

    /// Append one control-plane audit event as JSONL line.
    pub fn append(&self, event: &ControlPlaneAuditEvent) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| {
                format!(
                    "failed to open control-plane audit file {}",
                    self.path.display()
                )
            })?;
        let encoded = serde_json::to_string(event)?;
        file.write_all(encoded.as_bytes())?;
        file.write_all(b"\n")?;
        file.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn temp_home() -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "tengu-control-plane-audit-test-{}-{}",
            std::process::id(),
            Uuid::new_v4()
        ));
        std::fs::create_dir_all(&home).expect("create temp home");
        home
    }

    #[test]
    fn append_writes_jsonl_event() {
        let home = temp_home();
        let store = ControlPlaneAuditStore::new(&home).expect("audit store");
        let event = ControlPlaneAuditEvent {
            ts_epoch_s: 1,
            flow_key: "flow-1".to_string(),
            handoff_id: "handoff-1".to_string(),
            orchestrator_agent_id: "main".to_string(),
            dependent_agent_id: "worker".to_string(),
            status: "approved".to_string(),
            reason: None,
            requested_capabilities: vec!["tool:read_file".to_string()],
            objective: Some("inspect one file".to_string()),
        };
        store.append(&event).expect("append");

        let path = home
            .join("state")
            .join("audit")
            .join(CONTROL_PLANE_AUDIT_FILENAME);
        let content = std::fs::read_to_string(path).expect("read audit file");
        assert!(content.contains("\"status\":\"approved\""));
        assert!(content.contains("\"orchestrator_agent_id\":\"main\""));
    }
}
