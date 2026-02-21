//! Persistent control-plane assignment audit logging.
//!
//! Potential use case:
//! Keep an append-only JSONL trail for delegated capability assignment lifecycle
//! events (approved/denied/revoked/expired), plus bounded replay/pruning support
//! for restart-safe runtime continuity.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
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
    /// Outcome status (`approved`, `revoked`, `expired`, `denied`, `completed`, `failed`).
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

    /// Read recent assignment audit events from JSONL file.
    ///
    /// Invalid rows are skipped to keep replay resilient after partial writes
    /// or manual edits.
    pub fn read_recent(&self, limit: usize) -> Result<Vec<ControlPlaneAuditEvent>> {
        if limit == 0 || !self.path.exists() {
            return Ok(Vec::new());
        }
        let file = OpenOptions::new()
            .read(true)
            .open(&self.path)
            .with_context(|| {
                format!(
                    "failed to open control-plane audit file {}",
                    self.path.display()
                )
            })?;
        let reader = BufReader::new(file);
        let mut events = Vec::<ControlPlaneAuditEvent>::new();
        for line in reader.lines() {
            let Ok(line) = line else {
                continue;
            };
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<ControlPlaneAuditEvent>(&line) {
                events.push(event);
            }
        }
        if events.len() > limit {
            let start = events.len().saturating_sub(limit);
            Ok(events[start..].to_vec())
        } else {
            Ok(events)
        }
    }

    /// Prune audit file to keep only most recent `max_rows` JSONL entries.
    ///
    /// Returns number of dropped rows. Missing files are treated as empty.
    pub fn prune_retain_last(&self, max_rows: usize) -> Result<usize> {
        if max_rows == 0 || !self.path.exists() {
            return Ok(0);
        }
        let file = OpenOptions::new()
            .read(true)
            .open(&self.path)
            .with_context(|| {
                format!(
                    "failed to open control-plane audit file {}",
                    self.path.display()
                )
            })?;
        let reader = BufReader::new(file);
        let rows: Vec<String> = reader
            .lines()
            .map_while(Result::ok)
            .map(|line| line.trim().to_string())
            .filter(|line| !line.is_empty())
            .collect();
        if rows.len() <= max_rows {
            return Ok(0);
        }

        let dropped = rows.len().saturating_sub(max_rows);
        let retained = &rows[dropped..];
        let temp_path = self.path.with_extension("jsonl.tmp");
        let mut temp = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&temp_path)
            .with_context(|| {
                format!(
                    "failed to open temporary control-plane audit file {}",
                    temp_path.display()
                )
            })?;
        for row in retained {
            temp.write_all(row.as_bytes())?;
            temp.write_all(b"\n")?;
        }
        temp.flush()?;
        std::fs::rename(&temp_path, &self.path).with_context(|| {
            format!(
                "failed to replace control-plane audit file {}",
                self.path.display()
            )
        })?;
        Ok(dropped)
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

    #[test]
    fn read_recent_returns_last_n_events_and_skips_invalid_rows() {
        let home = temp_home();
        let store = ControlPlaneAuditStore::new(&home).expect("audit store");

        let first = ControlPlaneAuditEvent {
            ts_epoch_s: 1,
            flow_key: "flow-1".to_string(),
            handoff_id: "handoff-1".to_string(),
            orchestrator_agent_id: "main".to_string(),
            dependent_agent_id: "worker-a".to_string(),
            status: "approved".to_string(),
            reason: None,
            requested_capabilities: vec!["tool:read_file".to_string()],
            objective: Some("A".to_string()),
        };
        let second = ControlPlaneAuditEvent {
            ts_epoch_s: 2,
            flow_key: "flow-2".to_string(),
            handoff_id: "handoff-2".to_string(),
            orchestrator_agent_id: "main".to_string(),
            dependent_agent_id: "worker-b".to_string(),
            status: "approved".to_string(),
            reason: None,
            requested_capabilities: vec!["tool:search_content".to_string()],
            objective: Some("B".to_string()),
        };

        store.append(&first).expect("append first");
        let path = home
            .join("state")
            .join("audit")
            .join(CONTROL_PLANE_AUDIT_FILENAME);
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open for invalid row");
        file.write_all(b"{invalid-json}\n")
            .expect("write invalid row");
        store.append(&second).expect("append second");

        let recent = store.read_recent(1).expect("read recent");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].handoff_id, "handoff-2");
    }

    #[test]
    fn prune_retain_last_keeps_tail_rows() {
        let home = temp_home();
        let store = ControlPlaneAuditStore::new(&home).expect("audit store");

        for idx in 0..4 {
            store
                .append(&ControlPlaneAuditEvent {
                    ts_epoch_s: (idx + 1) as u64,
                    flow_key: format!("flow-{}", idx + 1),
                    handoff_id: format!("handoff-{}", idx + 1),
                    orchestrator_agent_id: "main".to_string(),
                    dependent_agent_id: format!("worker-{}", idx + 1),
                    status: "approved".to_string(),
                    reason: None,
                    requested_capabilities: vec!["tool:read_file".to_string()],
                    objective: Some(format!("obj-{}", idx + 1)),
                })
                .expect("append");
        }

        let dropped = store.prune_retain_last(2).expect("prune");
        assert_eq!(dropped, 2);

        let recent = store.read_recent(10).expect("read");
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].handoff_id, "handoff-3");
        assert_eq!(recent[1].handoff_id, "handoff-4");
    }
}
