//! TTL cleanup for `tengu_messages` / `tengu_outputs`.
//!
//! **Phase 1 scope**: no-op when `ttl_days == 0` (the default). When
//! `ttl_days > 0` we log a warning and return 0, because filter-based
//! delete is not yet exposed through the `VectorStore` trait. Phase 2
//! either extends the trait or adds a direct Qdrant client helper.
//!
//! This is acceptable because the default is permanent storage — no user
//! in Phase 1 will hit a non-zero `ttl_days` setting unless they explicitly
//! opted in, at which point the warning tells them what's missing.

#![cfg(feature = "qdrant")]
#![allow(dead_code)]  // Phase 4 wires ttl_cleanup via the orchestrator startup hook.

use anyhow::Result;

use crate::adapters::rag::RagStore;

pub async fn ttl_cleanup(rag: &RagStore) -> Result<u64> {
    if rag.cfg().ttl_days == 0 {
        tracing::debug!("rag ttl_cleanup: ttl_days = 0, skipping purge");
        return Ok(0);
    }
    tracing::warn!(
        ttl_days = rag.cfg().ttl_days,
        "TTL cleanup requested but filter-based delete is not implemented in \
         Phase 1 — tracked in docs/IMPLEMENTATION_PLAN.md Phase 2. \
         Entries older than {} days are NOT being purged.",
        rag.cfg().ttl_days
    );
    Ok(0)
}
