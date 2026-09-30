//! Runtime state port — what `tengu run` keeps in its state dir (the
//! `[xmarket]` state dir, else `<TENGU_HOME>/state`): the single-runner lease
//! in `runtime.db` and the heartbeat file `run-<sandbox>.json`. Impl:
//! `adapters::outbound::runtime_store`. Later waves add feed cursors, the
//! ingest seen-set and timers (tracker convention 3).

use async_trait::async_trait;

use crate::domain::runtime::{Heartbeat, RunnerLease};

#[async_trait]
pub(crate) trait RuntimeStore: Send + Sync {
    /// Take `resource` for `holder` until `now_ms + ttl_ms`: granted when
    /// free, expired, or already `holder`'s (renewal keeps `acquired_at_ms`).
    /// Atomic across processes.
    async fn acquire_lease(
        &self,
        resource: &str,
        holder: &str,
        ttl_ms: i64,
        now_ms: i64,
    ) -> anyhow::Result<RunnerLease>;
    /// Drop `holder`'s lease (no-op when someone else holds it).
    async fn release_lease(&self, resource: &str, holder: &str) -> anyhow::Result<()>;
    /// Replace `run-<hb.sandbox>.json` atomically (readers never see a
    /// partial file).
    async fn write_heartbeat(&self, hb: &Heartbeat) -> anyhow::Result<()>;
}
