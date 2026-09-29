//! Solana write-coordination port — one row set per wallet, shared by every
//! process of the install (TUI, `run-agent` children, webhook listener,
//! `tengu decide`): the single-writer lease, the in-flight send record and
//! the write fence. Impl: `adapters::outbound::solana::writes_store`
//! (`<TENGU_HOME>/state/solana-writes.db`). Unavailable ⇒ `send` refused.

use async_trait::async_trait;

use crate::domain::solana_write::{Lease, PendingSend};

#[async_trait]
pub(crate) trait SolanaWriteStore: Send + Sync {
    /// Take `resource` for `holder` until `now_ms + ttl_ms`: granted when
    /// free, expired, or already `holder`'s (renewal keeps `acquired_at_ms`).
    /// Atomic across processes.
    async fn acquire(
        &self,
        resource: &str,
        holder: &str,
        ttl_ms: i64,
        now_ms: i64,
    ) -> anyhow::Result<Lease>;
    /// Drop `holder`'s lease (no-op when someone else holds it).
    async fn release(&self, resource: &str, holder: &str) -> anyhow::Result<()>;
    /// Record a signed transaction about to be sent (one per wallet).
    async fn put_pending(&self, p: &PendingSend) -> anyhow::Result<()>;
    async fn pending(&self, wallet: &str) -> anyhow::Result<Option<PendingSend>>;
    /// Clear the record when its outcome is known (only if `signature`
    /// still matches).
    async fn clear_pending(&self, wallet: &str, signature: &str) -> anyhow::Result<()>;
    /// Raise the wallet's fence to `slot` (never lowers it).
    async fn raise_fence(&self, wallet: &str, slot: u64, now_ms: i64) -> anyhow::Result<()>;
    /// Slot of the wallet's last landed write; reads must be at or after it.
    async fn fence(&self, wallet: &str) -> anyhow::Result<Option<u64>>;
}
