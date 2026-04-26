//! Thin wrappers over `VectorStore::search` that translate [`MemoryHit`]
//! back into the RAG-shaped [`RagResult`] the planner will consume.

#![cfg(feature = "qdrant")]
#![allow(dead_code)]  // Phase 4 wires search_memory from replan.rs.

use anyhow::Result;

use crate::adapters::memory::context_block::MemoryHit;
use crate::adapters::rag::{RagKind, RagResult, RagStore};

pub async fn search_registry(
    rag: &RagStore,
    query: &str,
    top_k: usize,
) -> Result<Vec<RagResult>> {
    let vec = rag.embedder().embed(query).await?;
    // Pull more raw hits than requested so the dedup pass below has room
    // to merge per-agent duplicates (description + example_queries vectors)
    // without dropping below the caller's requested top_k. 4× is empirical:
    // each agent contributes at most 2 vectors today, so 2× would suffice
    // for agents alone, but tools/skills are single-vector and we want to
    // keep their representation fair after dedup.
    let raw_k = top_k.saturating_mul(4).max(top_k + 8);
    let hits = rag.registry().search(&vec, raw_k, None).await?;
    let mut results: Vec<RagResult> = hits.into_iter().filter_map(hit_to_result).collect();

    // Dedup by (kind, name) — keep the highest-scoring entry per identity.
    // search() returns hits in score order, so the first time we see a
    // given (kind, name) is also its best score. Stringly-typed key avoids
    // having to derive Hash on RagKind.
    let mut seen = std::collections::HashSet::<(String, String)>::new();
    results.retain(|r| seen.insert((r.kind.as_str().to_string(), r.name.clone())));

    results.truncate(top_k);
    Ok(results)
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
