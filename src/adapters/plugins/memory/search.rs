// src/adapters/plugins/memory/search.rs
//! `memory_search` tool — targeted vector read of the memory store.
//!
//! Read-side complement to `memory_ingest`: the LLM issues a
//! natural-language `query` (plus optional metadata filters) and
//! receives a JSON array of hits, each carrying the stored text, a
//! similarity score, and the entry's free-form metadata.
//!
//! Talks to `MemoryManager` directly (same backend as `memory_ingest`).
//! No more `MemoryService::recall_filtered` indirection.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::adapters::memory::context_block::ChunkMetadata;
use crate::adapters::memory::manager::MemoryManager;
use crate::adapters::tool_plugin::{Tool, ToolCtx, ToolOutput};
use crate::adapters::types::ToolDef;

/// Tool name (kept constant for cross-module reference).
#[allow(dead_code)]
pub(crate) const MEMORY_SEARCH_TOOL_NAME: &str = "memory_search";

/// Default `top_k` when the caller does not specify one.
const DEFAULT_TOP_K: usize = 5;

pub(crate) struct MemorySearchTool {
    def: ToolDef,
    memory_manager: Arc<MemoryManager>,
}

impl MemorySearchTool {
    pub(crate) fn new(memory_manager: Arc<MemoryManager>) -> Self {
        Self {
            def: super::memory_search_def(),
            memory_manager,
        }
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // scope: pure-compute — same rationale as `memory_ingest`.

        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("memory_search: missing required 'query' string"))?;

        if query.trim().is_empty() {
            anyhow::bail!("memory_search: 'query' must be a non-empty string");
        }

        let top_k = args
            .get("top_k")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_TOP_K);

        // Build the metadata filter from the three optional keys. Empty
        // strings are treated as absent so the LLM can't accidentally
        // exclude everything by passing `""`.
        let mut filter = ChunkMetadata::default();
        let mut has_filter = false;
        if let Some(v) = args.get("agent").and_then(|v| v.as_str()) {
            if !v.is_empty() {
                filter.agent = Some(v.to_string());
                has_filter = true;
            }
        }
        if let Some(v) = args.get("source").and_then(|v| v.as_str()) {
            if !v.is_empty() {
                filter.source = Some(v.to_string());
                has_filter = true;
            }
        }
        if let Some(v) = args.get("kind").and_then(|v| v.as_str()) {
            if !v.is_empty() {
                filter.kind = Some(v.to_string());
                has_filter = true;
            }
        }

        let results = self
            .memory_manager
            .search(query, top_k, if has_filter { Some(&filter) } else { None })
            .await?;

        let hits: Vec<Value> = results
            .into_iter()
            .map(|hit| {
                let mut meta_map = serde_json::Map::new();
                if let Some(ref a) = hit.metadata.agent {
                    meta_map.insert("agent".into(), Value::String(a.clone()));
                }
                if let Some(ref s) = hit.metadata.source {
                    meta_map.insert("source".into(), Value::String(s.clone()));
                }
                if let Some(ref k) = hit.metadata.kind {
                    meta_map.insert("kind".into(), Value::String(k.clone()));
                }
                if let Some(ref ts) = hit.metadata.timestamp_utc {
                    meta_map.insert("timestamp_utc".into(), Value::String(ts.clone()));
                }
                if !hit.metadata.tags.is_empty() {
                    meta_map.insert(
                        "tags".into(),
                        Value::Array(
                            hit.metadata
                                .tags
                                .iter()
                                .map(|t| Value::String(t.clone()))
                                .collect(),
                        ),
                    );
                }
                for (k, v) in &hit.metadata.extra {
                    meta_map.insert(k.clone(), v.clone());
                }
                let agent_id = hit.metadata.agent.clone().unwrap_or_default();
                json!({
                    "text": hit.text,
                    "score": hit.score,
                    "metadata": Value::Object(meta_map),
                    "agent_id": agent_id,
                })
            })
            .collect();

        let body = json!({ "hits": hits });
        Ok(ToolOutput::from(body.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::memory::vector::{DiskVectorStore, Embedder, VectorStore};
    use crate::adapters::plugins::memory::ingest::MemoryIngestTool;
    use crate::adapters::plugins::workspace::test_support::TestHarness;
    use tempfile::TempDir;

    fn make_manager() -> Arc<MemoryManager> {
        let manager = Arc::new(MemoryManager::new());
        let store: Arc<dyn VectorStore> = Arc::new(DiskVectorStore::in_memory());
        let embedder = Arc::new(Embedder::null());
        // Blocking on the runtime bootstrap is fine — tokio test runtime.
        let mgr = Arc::clone(&manager);
        futures::executor::block_on(async move {
            mgr.set_vector_backend(embedder, store).await;
        });
        manager
    }

    /// End-to-end plugin test: ingest → search returns the ingested
    /// content. Uses the null Embedder so every vector is the zero
    /// vector; cosine similarity is then 0.0 for every hit, but the
    /// entry still comes back via the raw search path.
    #[tokio::test]
    async fn ingest_then_search_returns_ingested_chunk() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let manager = make_manager();

        let ingest = MemoryIngestTool::new(Arc::clone(&manager));
        let ingest_args = json!({
            "content": "The Alpha Protocol was signed on 2026-01-15.",
            "agent_id": "researcher",
            "metadata": { "kind": "fact", "source": "dossier.md" }
        });
        ingest
            .execute(&ingest_args, &harness.ctx())
            .await
            .expect("ingest should succeed");

        let search = MemorySearchTool::new(Arc::clone(&manager));
        let search_args = json!({
            "query": "Alpha Protocol signing date",
            "top_k": 5,
        });
        let out = search
            .execute(&search_args, &harness.ctx())
            .await
            .expect("search should succeed");

        let parsed: Value = serde_json::from_str(&out.text).expect("tool output should be JSON");
        let hits = parsed
            .get("hits")
            .and_then(|v| v.as_array())
            .expect("response should contain `hits` array");

        assert!(
            !hits.is_empty(),
            "expected at least one hit, got: {}",
            out.text
        );
        let first = &hits[0];
        let text = first
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert!(
            text.contains("Alpha Protocol"),
            "top hit should contain 'Alpha Protocol', got: {}",
            text
        );
    }

    /// Metadata-filter path: ingest two entries with different `kind`
    /// values and assert the search only returns the one matching the
    /// filter.
    #[tokio::test]
    async fn metadata_filter_restricts_hits() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let manager = make_manager();

        let ingest = MemoryIngestTool::new(Arc::clone(&manager));
        ingest
            .execute(
                &json!({
                    "content": "The Alpha Protocol fact entry.",
                    "agent_id": "researcher",
                    "metadata": { "kind": "fact" }
                }),
                &harness.ctx(),
            )
            .await
            .unwrap();
        ingest
            .execute(
                &json!({
                    "content": "The Alpha Protocol note entry.",
                    "agent_id": "researcher",
                    "metadata": { "kind": "note" }
                }),
                &harness.ctx(),
            )
            .await
            .unwrap();

        let search = MemorySearchTool::new(Arc::clone(&manager));
        let out = search
            .execute(
                &json!({
                    "query": "Alpha Protocol",
                    "top_k": 10,
                    "kind": "fact",
                }),
                &harness.ctx(),
            )
            .await
            .unwrap();

        let parsed: Value = serde_json::from_str(&out.text).unwrap();
        let hits = parsed.get("hits").and_then(|v| v.as_array()).unwrap();
        assert!(!hits.is_empty(), "expected at least one matching hit");
        for hit in hits {
            let kind = hit
                .get("metadata")
                .and_then(|m| m.get("kind"))
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            assert_eq!(
                kind, "fact",
                "all hits must be of kind=fact when filter is applied"
            );
        }
    }

    /// Missing query is a hard error — don't silently embed an empty string.
    #[tokio::test]
    async fn missing_query_errors() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let manager = make_manager();

        let search = MemorySearchTool::new(manager);
        let out = search.execute(&json!({}), &harness.ctx()).await;
        assert!(out.is_err(), "expected missing-query error");
    }

    /// Empty query is rejected for the same reason.
    #[tokio::test]
    async fn empty_query_errors() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let manager = make_manager();

        let search = MemorySearchTool::new(manager);
        let out = search
            .execute(&json!({ "query": "   " }), &harness.ctx())
            .await;
        assert!(out.is_err(), "expected empty-query error");
    }
}
