//! TTL cleanup for `tengu_messages` / `tengu_outputs`.
//!
//! Phase 6.3 — filter-based delete is now implemented via the new
//! `VectorStore::delete_older_than` trait method. Both ephemeral
//! collections store a numeric `rag_created_at` (unix seconds) in their
//! payload extras (set by `memory_entry_metadata` in `rag/mod.rs`),
//! which the Qdrant impl filters with a `Range { lt: cutoff }` condition.
//!
//! `tengu_registry` is deliberately excluded — registry contents are
//! deterministic from the workspace (agents/skills/tools) and re-built
//! on each `reindex_all_workspace`, so time-decaying purge would just
//! delete entries that the next reindex re-adds.

#![cfg(feature = "qdrant")]
#![allow(dead_code)]  // Phase 4 wires ttl_cleanup via the orchestrator startup hook.

use anyhow::Result;

use crate::adapters::rag::RagStore;

/// Phase 6.3 — sweep entries older than `ttl_days` from `tengu_messages`
/// AND `tengu_outputs`. No-op when `ttl_days == 0` (default — permanent
/// storage). Returns the total count deleted across both collections.
pub async fn ttl_cleanup(rag: &RagStore) -> Result<u64> {
    let ttl_days = rag.cfg().ttl_days;
    if ttl_days == 0 {
        tracing::debug!("rag ttl_cleanup: ttl_days = 0, skipping purge");
        return Ok(0);
    }

    // unix seconds cutoff: entries with rag_created_at strictly less than
    // this are older than ttl_days and get purged.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cutoff = now.saturating_sub(ttl_days.saturating_mul(86_400));
    let cutoff_f = cutoff as f64;

    let msg_deleted = rag
        .messages()
        .delete_older_than("extra_rag_created_at", cutoff_f)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, collection = "tengu_messages", "ttl_cleanup: delete failed; continuing");
            0
        });
    let out_deleted = rag
        .outputs()
        .delete_older_than("extra_rag_created_at", cutoff_f)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, collection = "tengu_outputs", "ttl_cleanup: delete failed; continuing");
            0
        });

    let total = msg_deleted + out_deleted;
    tracing::info!(
        ttl_days,
        cutoff_unix = cutoff,
        messages_purged = msg_deleted,
        outputs_purged = out_deleted,
        total = total,
        "rag ttl_cleanup complete"
    );
    Ok(total)
}
