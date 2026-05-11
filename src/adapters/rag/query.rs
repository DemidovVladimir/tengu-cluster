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
    search_registry_with_vec(rag, &vec, top_k).await
}

/// Fix E (2026-05-09) — vector-input variant. Lets `RagPlanner::plan`
/// embed the user message ONCE per turn and reuse the vector across all
/// retrieval lanes (registry + outputs + messages) plus the persist write.
/// Quota and latency relief.
pub async fn search_registry_with_vec(
    rag: &RagStore,
    vec: &[f32],
    top_k: usize,
) -> Result<Vec<RagResult>> {
    // Pull more raw hits than requested so the dedup pass below has room
    // to merge per-agent duplicates (description + per-example vectors)
    // without dropping below the caller's requested top_k.
    //
    // Worst-case shape after the per-example-vector landing: an agent with
    // N example queries contributes 1 + N raw vectors. Today's max is 10
    // examples → 11 vectors per agent. If the query is highly relevant to
    // one agent, all of its vectors can crowd the head of the result list,
    // and the dedup pass collapses them into ONE row. To leave room for
    // tools/skills/other agents after dedup, we over-fetch generously.
    //
    // 6× is empirical: it covers up to ~16 example queries per agent at
    // top_k=10 without starving the result. Bump if you push example_queries
    // counts higher.
    let raw_k = top_k.saturating_mul(6).max(top_k + 16);
    let hits = rag.registry().search(vec, raw_k, None).await?;
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
    // Phase 1: searches `tengu_outputs` only. `tengu_messages` has its own
    // accessor (`search_messages`, Phase 6.4 full) so callers can choose
    // explicitly which bucket they want — outputs are high-signal step
    // results, messages are conversational and noisier.
    let vec = rag.embedder().embed(query).await?;
    let hits = rag.outputs().search(&vec, top_k, None).await?;
    Ok(hits.into_iter().filter_map(hit_to_result).collect())
}

/// Within-session output recall (added 2026-05-09 alongside Fix A).
///
/// Same retrieval as [`search_memory`] (semantic search over `tengu_outputs`)
/// but post-filters hits to those tagged with `rag_session_id == session_id`.
/// Over-fetches `top_k * 5` raw hits to leave room after the in-process
/// filter — mirrors the over-fetch pattern in [`search_registry`].
///
/// Server-side filtering is not yet wired into [`crate::adapters::memory::vector::VectorStore::search`]
/// (Qdrant backend ignores the `_filter` arg today; see `qdrant.rs:200-210`),
/// so we fetch wide and trim. When the VectorStore filter pushes down,
/// switch this helper to use it and drop the over-fetch margin.
///
/// Used by `RagPlanner::plan` when `MemoryConfig.within_session_output_top_k > 0`
/// to surface what subagents already produced THIS session, fixing the
/// "I have no record of that step" failure on follow-up questions.
pub async fn search_outputs_for_session(
    rag: &RagStore,
    query: &str,
    session_id: &str,
    top_k: usize,
) -> Result<Vec<RagResult>> {
    if top_k == 0 || session_id.is_empty() {
        return Ok(Vec::new());
    }
    let vec = rag.embedder().embed(query).await?;
    search_outputs_for_session_with_vec(rag, &vec, session_id, top_k).await
}

/// Fix E (2026-05-09) — vector-input variant. See `search_registry_with_vec`.
///
/// Fix F (2026-05-09) — server-side `rag_session_id` filtering via Qdrant
/// payload match. Pre Fix-F we over-fetched `5×` raw hits and post-filtered
/// in-process; with the filter pushed down we ask Qdrant for exactly
/// `top_k` matching rows. The post-filter still runs as a defence-in-depth
/// (older entries from before Fix-F may lack `rag_session_id` in payload,
/// and disk-backed test stores ignore the filter argument outright).
pub async fn search_outputs_for_session_with_vec(
    rag: &RagStore,
    vec: &[f32],
    session_id: &str,
    top_k: usize,
) -> Result<Vec<RagResult>> {
    if top_k == 0 || session_id.is_empty() {
        return Ok(Vec::new());
    }

    // Build the server-side filter targeting `extra.rag_session_id`.
    let mut filter = crate::adapters::memory::context_block::ChunkMetadata::default();
    filter.extra.insert(
        "rag_session_id".to_string(),
        serde_json::Value::String(session_id.to_string()),
    );

    // Modest over-fetch (2×, floor +4) as a safety margin in case the
    // Qdrant filter-push is partial or older payloads predate Fix F's
    // `rag_session_id` indexing. Way below the pre-Fix-F 5× margin.
    let raw_k = top_k.saturating_mul(2).max(top_k + 4);
    let hits = rag.outputs().search(vec, raw_k, Some(&filter)).await?;

    let filtered: Vec<RagResult> = hits
        .into_iter()
        .filter(|h| {
            h.metadata
                .extra
                .get("rag_session_id")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s == session_id)
        })
        .filter_map(hit_to_result)
        .take(top_k)
        .collect();
    Ok(filtered)
}

/// Phase 6.4 (full) — semantic search over `tengu_messages`. Forward-compat
/// hook: today no caller injects these into a planner prompt, but the
/// persistence side (`RagPlanner::persist_user_message`) writes to this
/// collection on every turn, so the data is accumulating. A follow-up
/// commit can wire this into the planner prompt as a "Cross-session
/// message recall" block, gated on a config knob.
///
/// Returns hits without any session_id filtering — single-user case is the
/// dominant deployment, and pulling all-sessions lets the planner reason
/// about long-running threads. Multi-tenant deployments will want to add
/// a session_id filter via `ChunkMetadata` once VectorStore exposes it.
pub async fn search_messages(rag: &RagStore, query: &str, top_k: usize) -> Result<Vec<RagResult>> {
    let vec = rag.embedder().embed(query).await?;
    search_messages_with_vec(rag, &vec, top_k).await
}

/// Fix E (2026-05-09) — vector-input variant. See `search_registry_with_vec`.
pub async fn search_messages_with_vec(
    rag: &RagStore,
    vec: &[f32],
    top_k: usize,
) -> Result<Vec<RagResult>> {
    let hits = rag.messages().search(vec, top_k, None).await?;
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
