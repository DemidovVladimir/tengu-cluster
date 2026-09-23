//! `compress_and_store` — the harness-enforced "step is done" signal.
//!
//! The model's "I'm done" tool. `main.rs::run_agent_subprocess` intercepts
//! `tool_calls` named `compress_and_store` out-of-band (OpenRouter path),
//! sets `compress_called = true`, and captures the summary; Claude Code
//! subagents often just stop instead, in which case the runner's graceful-
//! degradation path treats the final assistant text as the summary.
//!
//! Durable capture (when the `postgres_memory` feature is enabled) happens in
//! `run-agent` via `try_persist_agentic_step_summary` → Open Brain Postgres.
//!
//! This module now owns only the canonical tool *definition*. The legacy
//! Qdrant `write_summary` + `CompressAndStoreTool` / `CompressAndStorePlugin`
//! handler were removed in Phase 6 — Open Brain Postgres is the durable
//! backend, and the runner's out-of-band intercept + backstop cover capture.

#![allow(dead_code)]

use crate::domain::message::ToolDef;

/// The canonical tool definition. The runner appends it implicitly to every
/// subagent (`build_subprocess_tool_executor`) — never list it in
/// `[agents.<name>].tools`.
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
