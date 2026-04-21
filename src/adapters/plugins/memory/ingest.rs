// src/adapters/plugins/memory/ingest.rs
//! `memory_ingest` tool — ingest a document or fact into vector memory.
//!
//! Renamed from `remember` during harness-orchestration Phase 2 / task 2.1.
//! The underlying write path still runs through `MemoryService` (ported into
//! `MemoryManager` in a later task); this file only re-skins the LLM-facing
//! surface and extends the input schema with optional `chunks` and free-form
//! `metadata` fields for forward compatibility with chunked ingestion.
//!
//! The earlier `remember` tool accepted only `content` + string-keyed
//! `metadata`. `memory_ingest` additionally accepts:
//!
//! - `chunks: Vec<String>` — pre-chunked content; if provided, each chunk is
//!   stored as a separate memory entry with shared metadata.
//! - `metadata: object` — free-form tags. Non-string values are coerced to
//!   their JSON string representation so the existing `MemoryService` API
//!   (which takes `HashMap<String, String>`) can still consume them.
//!
//! `text` is accepted as an alias for `content` so callers can phrase their
//! ingest request either way. One of `text`/`content`/`chunks` must be
//! present.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

use crate::adapters::memory_builder::{MemoryService, MemoryServiceHandle};
use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDef;

/// Tool name (kept constant for cross-module reference).
#[allow(dead_code)]
pub(crate) const MEMORY_INGEST_TOOL_NAME: &str = "memory_ingest";

pub(crate) struct MemoryIngestTool {
    def: ToolDef,
    handle: Arc<MemoryServiceHandle>,
}

impl MemoryIngestTool {
    pub(crate) fn new(handle: Arc<MemoryServiceHandle>) -> Self {
        Self {
            def: ToolDef::new(
                MEMORY_INGEST_TOOL_NAME,
                "Ingest a document or fact into long-term vector memory. \
                 Accepts either a single `text`/`content` string or a list of \
                 pre-chunked `chunks`, plus optional free-form `metadata` \
                 (e.g. source, topic, kind). Embeddings are computed by the \
                 memory backend.",
                json!({
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "The fact, insight, or document body to ingest. \
                                            Alias of `text`; one of content/text/chunks is required."
                        },
                        "text": {
                            "type": "string",
                            "description": "Alias of `content` — the text to ingest."
                        },
                        "chunks": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Optional pre-chunked content. If supplied, each chunk \
                                            is ingested as a separate memory entry sharing the \
                                            same metadata. Use when the caller has already split \
                                            a long document."
                        },
                        "metadata": {
                            "type": "object",
                            "description": "Optional free-form tags attached to every stored \
                                            entry (e.g. {\"kind\": \"fact\", \"source\": \"url\", \
                                            \"topic\": \"auth\"}). Non-string values are coerced \
                                            to strings.",
                            "additionalProperties": true
                        }
                    }
                }),
            ),
            handle,
        }
    }
}

/// Convert a free-form metadata object into the `HashMap<String, String>` the
/// underlying `MemoryService` expects. Strings pass through; everything else
/// is serialized to its JSON representation so nothing is silently dropped.
fn coerce_metadata(value: Option<&Value>) -> HashMap<String, String> {
    value
        .and_then(|v| v.as_object())
        .map(|obj| {
            obj.iter()
                .map(|(k, v)| {
                    let s = match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    (k.clone(), s)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[async_trait]
impl Tool for MemoryIngestTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // scope: pure-compute — the OpenRouter embedding host is an
        // implementation detail of the memory subsystem (baked into
        // `OpenRouterEmbeddingAdapter`), not a tool-argument-driven HTTP call.
        // The tool surface exposes only in-memory storage semantics.

        let agent_id = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("default");

        let metadata = coerce_metadata(args.get("metadata"));

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

        let service =
            MemoryService::new(self.handle.embedding.as_ref(), self.handle.store.as_ref());

        let mut ids: Vec<String> = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            let redacted = ctx.secret_registry.redact(chunk);
            let id = service
                .remember_with_metadata(redacted.as_str(), agent_id, metadata.clone())
                .await?;
            ids.push(id);
        }

        let msg = if ids.len() == 1 {
            format!("Stored memory with id: {}", ids[0])
        } else {
            format!("Stored {} chunks with ids: {}", ids.len(), ids.join(", "))
        };
        Ok(ToolOutput::from(msg))
    }
}
