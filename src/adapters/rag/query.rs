//! Thin wrappers over `VectorStore::search` that translate [`MemoryHit`]
//! back into the RAG-shaped [`RagResult`] the planner will consume.

#![cfg(feature = "qdrant")]

use anyhow::Result;

use crate::adapters::memory::context_block::MemoryHit;
use crate::adapters::rag::{RagKind, RagResult, RagStore};

pub async fn search_registry(
    rag: &RagStore,
    query: &str,
    top_k: usize,
) -> Result<Vec<RagResult>> {
    let vec = rag.embedder().embed(query).await?;
    let hits = rag.registry().search(&vec, top_k, None).await?;
    Ok(hits.into_iter().filter_map(hit_to_result).collect())
}

pub async fn search_memory(rag: &RagStore, query: &str, top_k: usize) -> Result<Vec<RagResult>> {
    // Phase 1: searches `tengu_outputs` only. `tengu_messages` is loaded
    // deterministically elsewhere (last-N by session_id, Phase 4).
    let vec = rag.embedder().embed(query).await?;
    let hits = rag.outputs().search(&vec, top_k, None).await?;
    Ok(hits.into_iter().filter_map(hit_to_result).collect())
}

/// Translate a `MemoryHit` back into a `RagResult`. We look first at the
/// `rag_type` / `rag_name` / `rag_source_path` keys in `extra`; falling back
/// to the `kind` / `source` / text fields when those are missing (e.g. for
/// older entries written outside the RAG layer).
fn hit_to_result(hit: MemoryHit) -> Option<RagResult> {
    let kind_str = hit
        .metadata
        .extra
        .get("rag_type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            hit.metadata
                .kind
                .as_deref()
                .and_then(|k| k.strip_prefix("rag.").map(|s| s.to_string()))
        })
        .unwrap_or_default();
    let kind = RagKind::parse(&kind_str).unwrap_or(RagKind::Tool);

    let name = hit
        .metadata
        .extra
        .get("rag_name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            hit.text
                .lines()
                .next()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "<unknown>".to_string())
        });

    let source_path = hit
        .metadata
        .extra
        .get("rag_source_path")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| hit.metadata.source.clone());

    Some(RagResult {
        kind,
        name,
        description: hit.text,
        score: hit.score,
        source_path,
    })
}
