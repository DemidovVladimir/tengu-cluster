//! `compress_and_store` — the harness-enforced "step is done" signal.
//!
//! ## Two dispatch paths (intentional)
//!
//! 1. **Out-of-band** (OpenRouter subagent path) — `main.rs::run_agent_subprocess`
//!    intercepts `tool_calls` named `compress_and_store` BEFORE the executor
//!    runs them. Sets `compress_called = true` and writes via `write_summary`.
//!    This path lets the runner know "the step is done" so the multi-turn
//!    loop can break cleanly.
//!
//! 2. **Plugin path** (Phase 7.6 Bug B — Claude Code subagent path) — Claude
//!    Code's MCP bridge routes the tool call through `PluginToolExecutor`,
//!    which needs an actual `Tool` handler in the registry. This module's
//!    `CompressAndStoreTool` + `CompressAndStorePlugin` provide that. Same
//!    write semantics as `write_summary`. The runner doesn't see this call
//!    out-of-band (Claude Code does its tool loop internally), so
//!    `compress_called` stays false and the runner takes the graceful-
//!    degradation path (final assistant text becomes the IPC summary).
//!    The data is still durably persisted to `tengu_outputs` either way.

#![cfg(feature = "qdrant")]
#![allow(dead_code)]  // definition() and write_summary are both wired live as of Phase 7.6.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::adapters::rag::{MemoryEntry, MemoryKind, RagStore};
use crate::adapters::tool_plugin::{PluginCtx, Tool, ToolCtx, ToolOutput, ToolPlugin};
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

/// Fix D (2026-05-09) — byte cap before we hand a summary to the embedder.
///
/// `text-embedding-3-small` rejects inputs over 8192 tokens; 24 000 bytes
/// (~6–8K tokens depending on the BPE) leaves a safe margin. Pre Fix-D
/// long pipeline summaries (POI + IP-NFT mint + Molecule project + upload
/// + Beach.science announcement → many KB of text) silently failed to
/// embed: `store_memory` returned `Err`, the fail-soft callers swallowed
/// it, and `tengu_outputs` got no row. Truncating loses tail content but
/// a truncated row is still recallable; an embed-failure row is invisible.
///
/// Set conservatively. If you raise this, double-check the embedder's
/// model-specific token cap.
const MAX_SUMMARY_BYTES: usize = 24_000;

/// Truncate a summary at a UTF-8-safe boundary, appending a marker so the
/// planner-side recall block can tell the LLM "this was cut". Delegates to
/// the shared `token::truncate_at_boundary` primitive.
fn cap_summary_for_embed(summary: &str) -> std::borrow::Cow<'_, str> {
    use crate::adapters::token::truncate_at_boundary;
    match truncate_at_boundary(summary, MAX_SUMMARY_BYTES) {
        None => std::borrow::Cow::Borrowed(summary),
        Some((prefix, end)) => std::borrow::Cow::Owned(format!(
            "{prefix}\n\n[truncated for embed — {} of {} bytes retained; embedder caps at ~32K bytes / 8K tokens]",
            end,
            summary.len()
        )),
    }
}

/// Write a step's completion summary to `tengu_outputs`.
///
/// Returns the store-synthesised entry id. Callers (the Phase 3 stub today,
/// the Phase 4 LLM tool dispatcher later) pass through identical arguments
/// so downstream behaviour is the same.
///
/// Fix D — summary is byte-capped to `MAX_SUMMARY_BYTES` before embed.
/// Caps are logged at info-level so operators see the truncation event.
pub async fn write_summary(
    rag: &RagStore,
    session_id: &str,
    step_id: &str,
    summary: &str,
) -> Result<String> {
    let original_bytes = summary.len();
    let capped = cap_summary_for_embed(summary);
    if matches!(capped, std::borrow::Cow::Owned(_)) {
        tracing::info!(
            session_id = %session_id,
            step_id = %step_id,
            original_bytes,
            capped_bytes = MAX_SUMMARY_BYTES,
            "compress_and_store: summary truncated to fit embedder input cap (Fix D)"
        );
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let entry = MemoryEntry {
        kind: MemoryKind::StepOutput,
        session_id: session_id.to_string(),
        step_id: Some(step_id.to_string()),
        content: capped.into_owned(),
        created_at: now,
    };
    rag.store_memory(entry).await
}

#[cfg(test)]
mod cap_tests {
    use super::*;

    /// Fix D — short input passes through untouched (Borrowed variant,
    /// no allocation, no truncation marker).
    #[test]
    fn cap_summary_short_passes_through() {
        let s = "hello world".repeat(10);
        let capped = cap_summary_for_embed(&s);
        assert!(matches!(capped, std::borrow::Cow::Borrowed(_)));
        assert_eq!(&*capped, &s);
        assert!(!capped.contains("[truncated"));
    }

    /// Fix D — input over `MAX_SUMMARY_BYTES` is truncated and a marker
    /// is appended, returning Cow::Owned. The marker preserves operator-
    /// visible context: original length, retained length.
    #[test]
    fn cap_summary_long_truncates_with_marker() {
        let s = "a".repeat(MAX_SUMMARY_BYTES + 5_000);
        let capped = cap_summary_for_embed(&s);
        assert!(matches!(capped, std::borrow::Cow::Owned(_)));
        assert!(capped.starts_with(&"a".repeat(100)));
        assert!(capped.contains("[truncated for embed"));
        // Total length = retained bytes + marker tail (~120 bytes).
        let n_bytes = capped.len();
        assert!(
            n_bytes > MAX_SUMMARY_BYTES,
            "expected marker to push above the cap: got {}",
            n_bytes
        );
        assert!(
            n_bytes < MAX_SUMMARY_BYTES + 300,
            "expected marker to be small: got {}",
            n_bytes
        );
    }

    /// Fix D — slice boundary is UTF-8-safe even when the cap lands inside
    /// a multibyte sequence. Pre-fix this would have panicked on a
    /// non-char-boundary byte index. Delegates to `truncate_at_boundary`,
    /// which steps back to the nearest valid boundary.
    #[test]
    fn cap_summary_handles_multibyte_at_boundary() {
        let prefix: String = "a".repeat(MAX_SUMMARY_BYTES - 1);
        let suffix: String = "é".repeat(500); // each "é" is 2 bytes
        let s = prefix + &suffix;
        // Should not panic.
        let capped = cap_summary_for_embed(&s);
        assert!(matches!(capped, std::borrow::Cow::Owned(_)));
    }
}

// ---------------------------------------------------------------------------
// Plugin path (Phase 7.6 Bug B) — registry-routed tool handler for Claude
// Code subagents. OpenRouter still uses the out-of-band detection in
// run_agent_subprocess; this struct only fires when the executor is invoked.
// ---------------------------------------------------------------------------

pub(crate) struct CompressAndStoreTool {
    def: ToolDef,
    /// Pre-built RagStore handle. Constructed once at plugin registration so
    /// each tool call doesn't re-handshake Qdrant.
    rag: Arc<RagStore>,
}

#[async_trait]
impl Tool for CompressAndStoreTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // scope: pure-compute
        // No agent-visible side effect to gate. `compress_and_store` is
        // harness-implicit (every subagent gets it appended by the runner;
        // it cannot be turned off per-agent and it cannot be aimed at
        // user-controlled targets). The Qdrant write goes to the harness's
        // own tengu_outputs collection — destination URL is fixed by the
        // sandbox config, not chosen per call. Treat as infrastructure.
        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if summary.trim().is_empty() {
            return Ok(ToolOutput::from(
                "compress_and_store called without `summary` — nothing written. \
                 Provide a non-empty summary string."
                    .to_string(),
            ));
        }
        // session_id is read from the env (TENGU_SESSION_ID) which
        // run_agent_subprocess sets at startup. Plugin construction doesn't
        // know which subagent process it's serving until that env is in
        // place, so this lazy read avoids threading session_id through
        // build_tool_executor's signature. step_id is not currently
        // available in ToolCtx — placeholder for now.
        let session_id = std::env::var("TENGU_SESSION_ID").unwrap_or_else(|_| "subagent".to_string());
        let id = write_summary(&self.rag, &session_id, "<plugin-path>", &summary).await?;
        tracing::info!(
            entry_id = %id,
            session_id = %session_id,
            "compress_and_store: plugin path wrote summary to tengu_outputs"
        );
        Ok(ToolOutput::from(format!("stored (entry id: {})", id)))
    }
}

/// Plugin wrapping the single `compress_and_store` tool. Registers the tool
/// only when a `RagStore` is reachable (i.e. Qdrant is up + `OPENROUTER_API_KEY`
/// set for the embedder). On RagStore construction failure we log a warn and
/// skip registration — the tool def stays in the advertised list, so the LLM
/// may still try to call it; the executor's "tool not available" error is
/// the right user-visible signal in that case (memory is genuinely unavailable).
pub(crate) struct CompressAndStorePlugin {
    pub memory_config: crate::adapters::config::MemoryConfig,
}

#[async_trait]
impl ToolPlugin for CompressAndStorePlugin {
    fn name(&self) -> &'static str {
        "compress_and_store"
    }

    async fn tools(&self, _ctx: &PluginCtx<'_>) -> Result<Vec<Arc<dyn Tool>>> {
        let rag = match RagStore::from_config(self.memory_config.clone()).await {
            Ok(r) => Arc::new(r),
            Err(e) => {
                tracing::warn!(error = %e, "compress_and_store plugin: RagStore unavailable; tool not registered");
                return Ok(Vec::new());
            }
        };
        Ok(vec![Arc::new(CompressAndStoreTool {
            def: definition(),
            rag,
        })])
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
