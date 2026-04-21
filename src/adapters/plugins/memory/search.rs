// src/adapters/plugins/memory/search.rs
//! `memory_search` tool — targeted vector read of the memory store.
//!
//! This is the read-side complement to `memory_ingest`: the LLM issues a
//! natural-language `query` (plus optional metadata filters) and receives a
//! JSON array of hits, each carrying the stored text, a similarity score, and
//! the entry's free-form metadata. Use it when the caller needs to look up
//! specific prior content (documents ingested by other agents, past turn
//! summaries, etc.) instead of relying on the automatic memory prefetch.
//!
//! Interim wiring (harness-orchestration task 2.2): the tool runs through the
//! same `MemoryServiceHandle` that `MemoryIngestTool` uses — i.e.
//! `MemoryService::recall_filtered` against the shared embedding + store
//! ports. When `MemoryService` is retired in favor of `MemoryManager` (a later
//! task in the same plan), both tools flip to `MemoryManager` together.

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
pub(crate) const MEMORY_SEARCH_TOOL_NAME: &str = "memory_search";

/// Default `top_k` when the caller does not specify one.
const DEFAULT_TOP_K: usize = 5;

/// Effective token budget for a single `memory_search` response. Chosen to
/// match the order-of-magnitude of an interactive recall — large enough to
/// return several multi-paragraph chunks but small enough to avoid blowing
/// up the tool-result channel. `MemoryService::recall_filtered` uses this
/// only for trimming the already-ranked result list.
const DEFAULT_MAX_TOKENS: usize = 4096;

pub(crate) struct MemorySearchTool {
    def: ToolDef,
    handle: Arc<MemoryServiceHandle>,
}

impl MemorySearchTool {
    pub(crate) fn new(handle: Arc<MemoryServiceHandle>) -> Self {
        Self {
            def: ToolDef::new(
                MEMORY_SEARCH_TOOL_NAME,
                "Targeted vector search of long-term memory. Returns hits with \
                 text, similarity score, and metadata. Use when you need to \
                 look up specific prior content (documents ingested by other \
                 agents, past turn summaries, etc.). Optional `agent`, \
                 `source`, and `kind` filters restrict matches to entries \
                 whose metadata has the exact given value.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Natural-language search query embedded by the memory backend."
                        },
                        "top_k": {
                            "type": "integer",
                            "description": "Max hits to return (default: 5).",
                            "default": 5
                        },
                        "agent": {
                            "type": "string",
                            "description": "Optional metadata filter: only return hits whose `agent` metadata equals this value."
                        },
                        "source": {
                            "type": "string",
                            "description": "Optional metadata filter: only return hits whose `source` metadata equals this value."
                        },
                        "kind": {
                            "type": "string",
                            "description": "Optional metadata filter: only return hits whose `kind` metadata equals this value."
                        }
                    },
                    "required": ["query"]
                }),
            ),
            handle,
        }
    }
}

#[async_trait]
impl Tool for MemorySearchTool {
    fn definition(&self) -> &ToolDef {
        &self.def
    }

    async fn execute(&self, args: &Value, _ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
        // scope: pure-compute — like `memory_ingest`, this tool only touches
        // the in-process embedding + vector-store ports. The tool surface is
        // entirely in-memory; the embedding HTTP call is an implementation
        // detail baked into the `OpenRouterEmbeddingAdapter`, not a
        // tool-argument-driven network hop.

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

        // Build the metadata filter from the three optional keys. An absent
        // key means "do not filter"; an empty string is treated the same as
        // an absent key so the LLM can't accidentally exclude everything by
        // passing `""`.
        let mut filter: HashMap<String, String> = HashMap::new();
        for key in ["agent", "source", "kind"] {
            if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
                if !v.is_empty() {
                    filter.insert(key.to_string(), v.to_string());
                }
            }
        }

        let service =
            MemoryService::new(self.handle.embedding.as_ref(), self.handle.store.as_ref());

        let results = service
            .recall_filtered(query, top_k, DEFAULT_MAX_TOKENS, &filter)
            .await?;

        let hits: Vec<Value> = results
            .into_iter()
            .map(|r| {
                json!({
                    "text": r.entry.content,
                    "score": r.score,
                    "metadata": r.entry.metadata,
                    "agent_id": r.entry.agent_id,
                    "id": r.entry.id,
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
    use crate::adapters::memory_builder::DiskVectorMemoryStore;
    use crate::adapters::plugins::memory::ingest::MemoryIngestTool;
    use crate::adapters::plugins::workspace::test_support::TestHarness;
    use crate::adapters::ports::{EmbeddingPort, MemoryStorePort};
    use tempfile::TempDir;

    /// Deterministic stub embedder. Produces a fixed-length vector whose
    /// first coordinate is driven by a word-frequency check against a tiny
    /// vocabulary, so two inputs that share keywords score higher via
    /// cosine similarity than unrelated inputs. That's all we need to
    /// assert "ingested chunk is retrieved by a keyword-overlapping query".
    struct KeywordEmbedder;

    #[async_trait]
    impl EmbeddingPort for KeywordEmbedder {
        async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
            // 8-dim basis: [alpha, protocol, signed, 2026, writer, researcher, fact, misc]
            let words = [
                "alpha",
                "protocol",
                "signed",
                "2026",
                "writer",
                "researcher",
                "fact",
                "misc",
            ];
            Ok(texts
                .iter()
                .map(|t| {
                    let lower = t.to_lowercase();
                    let mut v = vec![0.0f32; words.len()];
                    for (i, w) in words.iter().enumerate() {
                        if lower.contains(w) {
                            v[i] = 1.0;
                        }
                    }
                    // Always put a tiny bias in the last coord so zero-overlap
                    // queries still produce a non-degenerate vector (otherwise
                    // cosine_similarity returns 0.0 for everything and the
                    // order is indeterminate).
                    v[words.len() - 1] = 0.01;
                    v
                })
                .collect())
        }
    }

    fn make_handle(workspace: &std::path::Path) -> Arc<MemoryServiceHandle> {
        let store_dir = workspace.join("memory");
        let store = DiskVectorMemoryStore::new(&store_dir).expect("disk store init");
        Arc::new(MemoryServiceHandle {
            embedding: Arc::new(KeywordEmbedder),
            store: Arc::new(store) as Arc<dyn MemoryStorePort>,
        })
    }

    /// End-to-end plugin test: agent "researcher" ingests a fact via
    /// `memory_ingest`; agent "writer" retrieves it via `memory_search`.
    /// Mirrors the Phase 2 contract: "fact ingested as researcher →
    /// searchable as writer".
    #[tokio::test]
    async fn ingest_then_search_returns_ingested_chunk() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let handle = make_handle(tmp.path());

        // Ingest as researcher.
        let ingest = MemoryIngestTool::new(Arc::clone(&handle));
        let ingest_args = json!({
            "content": "The Alpha Protocol was signed on 2026-01-15.",
            "agent_id": "researcher",
            "metadata": { "kind": "fact", "source": "dossier.md" }
        });
        ingest
            .execute(&ingest_args, &harness.ctx())
            .await
            .expect("ingest should succeed");

        // Search as writer — the tool has no agent identity of its own; any
        // agent using the shared handle sees the shared store.
        let search = MemorySearchTool::new(Arc::clone(&handle));
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

    /// Metadata-filter path: ingest two entries with different `kind` values
    /// and assert the search only returns the one matching the filter.
    #[tokio::test]
    async fn metadata_filter_restricts_hits() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let handle = make_handle(tmp.path());

        let ingest = MemoryIngestTool::new(Arc::clone(&handle));
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

        let search = MemorySearchTool::new(Arc::clone(&handle));
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
        let handle = make_handle(tmp.path());

        let search = MemorySearchTool::new(handle);
        let out = search.execute(&json!({}), &harness.ctx()).await;
        assert!(out.is_err(), "expected missing-query error");
    }

    /// Empty query is rejected for the same reason.
    #[tokio::test]
    async fn empty_query_errors() {
        let tmp = TempDir::new().unwrap();
        let harness = TestHarness::new(tmp.path());
        let handle = make_handle(tmp.path());

        let search = MemorySearchTool::new(handle);
        let out = search
            .execute(&json!({ "query": "   " }), &harness.ctx())
            .await;
        assert!(out.is_err(), "expected empty-query error");
    }
}
