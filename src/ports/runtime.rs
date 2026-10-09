//! Runtime ports. [`RuntimeStore`] — what `tengu run` keeps in its state dir
//! (the `[xmarket]` state dir, else the `[sources]` one, else
//! `<TENGU_HOME>/state`): the single-runner leases in `runtime.db` and the
//! heartbeat file `run-<sandbox>.json`. Impl:
//! `adapters::outbound::runtime_store`. Later waves add feed cursors, the
//! ingest seen-set and timers (tracker convention 3). [`RuntimeJob`] — one
//! named application job a `kind = "job"` feed runs (`config/feeds.rs`
//! `JOBS`; built by `bootstrap/runtime.rs::job_for`).

use async_trait::async_trait;

use crate::domain::observation::ErrorClass;
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

/// How one run of a job went (the feed's health and backoff read it as a
/// tool call's result: `Done` = an item, `Failed` = an error of `class`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JobOutcome {
    /// Done — or nothing to do (a week already frozen); `note` says which.
    Done { note: String },
    /// Failed: logged and backed off like a failed call of that class.
    Failed { class: ErrorClass, message: String },
}

#[async_trait]
pub(crate) trait RuntimeJob: Send + Sync {
    /// Run the job for the slot `slot_ms` (its schedule's time, not the
    /// clock's); `run_id` = `feed:<name>:<slot ms>`, the same for a retry.
    async fn run(&self, slot_ms: i64, run_id: &str) -> JobOutcome;
}
