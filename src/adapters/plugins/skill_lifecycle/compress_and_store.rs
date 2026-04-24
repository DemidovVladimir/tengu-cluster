//! `compress_and_store` — the harness-enforced "step is done" signal.
//!
//! Phase 3 of the redesign ships the Qdrant write helper only. The full
//! `impl Tool for CompressAndStoreTool` wrapping lands in Phase 4 once the
//! subprocess LLM mini-loop is wired; at that point the LLM calls this as
//! its FINAL tool to signal completion, and the runner sees the written
//! summary in `tengu_outputs`.
//!
//! The canonical ToolDef is exported so the Phase 4 mini-loop can register
//! it against the LLM. The `write_summary` helper is the path used by the
//! Phase 3 run-agent stub (no LLM) — same write, no tool dispatch.

#![cfg(feature = "qdrant")]
#![allow(dead_code)]  // definition() becomes live in Phase 4 LLM mini-loop; write_summary already used.

use anyhow::Result;

use crate::adapters::rag::{MemoryEntry, MemoryKind, RagStore};
use crate::adapters::types::ToolDef;

/// The canonical tool definition. Shared across the stub (Phase 3) and the
/// real LLM integration (Phase 4+).
pub fn definition() -> ToolDef {
    ToolDef {
        name: "compress_and_store".to_string(),
        description: "Store a compressed summary of your completed work. \
                      Call this as your FINAL action when the task is done. \
                      Do not call it mid-task."
            .to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "Concise summary of what you accomplished, found, or produced."
                }
            },
            "required": ["summary"]
        }),
    }
}

/// Write a step's completion summary to `tengu_outputs`.
///
/// Returns the store-synthesised entry id. Callers (the Phase 3 stub today,
/// the Phase 4 LLM tool dispatcher later) pass through identical arguments
/// so downstream behaviour is the same.
pub async fn write_summary(
    rag: &RagStore,
    session_id: &str,
    step_id: &str,
    summary: &str,
) -> Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let entry = MemoryEntry {
        kind: MemoryKind::StepOutput,
        session_id: session_id.to_string(),
        step_id: Some(step_id.to_string()),
        content: summary.to_string(),
        created_at: now,
    };
    rag.store_memory(entry).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_matches_redesign_spec() {
        let d = definition();
        assert_eq!(d.name, "compress_and_store");
        assert!(d.description.contains("FINAL"));
        let required = d.parameters.get("required").unwrap().as_array().unwrap();
        assert!(required.iter().any(|v| v == "summary"));
    }
}
