// src/adapters/plugins/memory/ingest.rs
//! `memory_ingest` tool — ingest a document or fact into vector memory.
//!
//! Talks to `MemoryManager` directly — the harness-owned `Embedder` +
//! `VectorStore` pair registered once per session by
//! `channel_runtime::build_memory_manager`. The pre-migration path went
//! through a `MemoryServiceHandle` (`EmbeddingPort` / `MemoryStorePort`
//! shim); this version bypasses that indirection entirely.
//!
//! Input schema (unchanged from the legacy `remember` tool plus the
//! harness-orchestration Phase 2 extensions):
//!
//! - `text` / `content` — the text to ingest (aliases).
//! - `chunks: Vec<String>` — pre-chunked content; each chunk lands as
//!   its own memory entry with shared metadata.
//! - `metadata: object` — free-form tags. String values pass through;
//!   non-string values are coerced to their JSON string form.
//! - `agent_id` — the logical agent the entry belongs to (defaults to
//!   `"default"`). Populated into `ChunkMetadata::agent` so
//!   `memory_search` filters still work.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

use crate::adapters::memory::context_block::ChunkMetadata;
use crate::adapters::memory::manager::MemoryManager;
use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDef;

/// Tool name (kept constant for cross-module reference).
#[allow(dead_code)]
pub(crate) const MEMORY_INGEST_TOOL_NAME: &str = "memory_ingest";

pub(crate) struct MemoryIngestTool {
    def: ToolDef,
    memory_manager: Arc<MemoryManager>,
}

impl MemoryIngestTool {
    pub(crate) fn new(memory_manager: Arc<MemoryManager>) -> Self {
        Self {
            def: super::memory_ingest_def(),
            memory_manager,
        }
    }
}

/// Build a `ChunkMetadata` from the LLM-supplied free-form metadata map.
/// Known keys (`agent`, `source`, `kind`, `timestamp_utc`, `tags`) land in
/// their typed slots; everything else lands in `extra` as JSON values (so
/// non-string values don't lose their shape).
fn build_chunk_metadata(raw: Option<&Value>) -> ChunkMetadata {
    let mut md = ChunkMetadata::default();
    if let Some(obj) = raw.and_then(|v| v.as_object()) {
        for (k, v) in obj.iter() {
            match k.as_str() {
                "agent" => md.agent = v.as_str().map(|s| s.to_string()),
                "source" => md.source = v.as_str().map(|s| s.to_string()),
                "kind" => md.kind = v.as_str().map(|s| s.to_string()),
                "timestamp_utc" => md.timestamp_utc = v.as_str().map(|s| s.to_string()),
                "tags" => {
                    if let Some(arr) = v.as_array() {
                        md.tags = arr
                            .iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        md.tags = s
                            .split(',')
                            .map(|t| t.trim().to_string())
                            .filter(|t| !t.is_empty())
                            .collect();
                    }
                }
                other => {
                    md.extra.insert(other.to_string(), v.clone());
                }
            }
        }
    }
    md
}

#[async_trait]
impl Tool for MemoryIngestTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // scope: pure-compute — the OpenRouter embedding host is an
        // implementation detail of the memory subsystem; the tool surface
        // exposes only in-memory storage semantics.

        let agent_id = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("default");

        let metadata = build_chunk_metadata(args.get("metadata"));

        // Collect the corpus to ingest: explicit chunks take precedence, else
        // fall back to `text`/`content` as a single entry.
        let chunks: Vec<String> = if let Some(arr) = args.get("chunks").and_then(|v| v.as_array()) {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        } else {
            let raw = args
                .get("text")
                .or_else(|| args.get("content"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "memory_ingest: one of 'content', 'text', or 'chunks' is required"
                    )
                })?;
            vec![raw.to_string()]
        };

        if chunks.is_empty() {
            anyhow::bail!("memory_ingest: 'chunks' was provided but empty");
        }

        // Redact each chunk and own the String so we can hand refs to the
        // batch API.
        let redacted_owned: Vec<String> = chunks
            .iter()
            .map(|c| ctx.secret_registry.redact(c).to_string())
            .collect();

        let count = if redacted_owned.len() == 1 {
            // Single-text path — skips the batch indirection.
            self.memory_manager
                .ingest_one(&redacted_owned[0], agent_id, metadata.clone())
                .await?;
            1
        } else {
            // Multi-chunk path — one embedding HTTP call covers all chunks.
            let text_refs: Vec<&str> = redacted_owned.iter().map(|s| s.as_str()).collect();
            self.memory_manager
                .ingest_batch(&text_refs, agent_id, metadata.clone())
                .await?
        };

        let msg = if count == 1 {
            "Stored 1 memory chunk.".to_string()
        } else {
            format!("Stored {} chunks.", count)
        };
        Ok(ToolOutput::from(msg))
    }
}
