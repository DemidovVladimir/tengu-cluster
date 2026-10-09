//! Runtime ports. [`RuntimeStore`] — what `tengu run` keeps in its state dir
//! (the `[xmarket]` state dir, else the `[sources]` one, else
//! `<TENGU_HOME>/state`): the single-runner leases in `runtime.db` and the
//! heartbeat file `run-<sandbox>.json`. Impl:
//! `adapters::outbound::runtime_store`. Later waves add feed cursors, the
//! ingest seen-set and timers (tracker convention 3). [`RuntimeJob`] — one
//! named application job a `kind = "job"` feed runs (`config/feeds.rs`
//! `JOBS`; built by `bootstrap/runtime.rs::job_for`). [`Ownership`] — the
//! leases a use case writes under, asked before every irreversible step
//! (impl: `application::runtime::HeldLeases`, the process's leases;
//! [`Unleased`] for a caller that holds none).

use std::fmt;

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

/// Why a process no longer owns what it writes under: a lease taken by
/// another holder, lapsed and re-taken since, or not renewed before it
/// expired. Carried inside the `anyhow::Error` ([`LeaseLost::of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeaseLost {
    /// Which lease and how (`lease `state:soe` lost to `<holder>` — …`).
    pub reason: String,
}

impl LeaseLost {
    /// The loss inside `e`, if `e` is one (anywhere in its chain).
    pub(crate) fn of(e: &anyhow::Error) -> Option<&LeaseLost> {
        e.chain().find_map(|c| c.downcast_ref::<LeaseLost>())
    }
}

impl fmt::Display for LeaseLost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "lease_lost: {} — this process stopped and wrote nothing more",
            self.reason
        )
    }
}

impl std::error::Error for LeaseLost {}

/// The leases a use case writes under, as it sees them: [`Ownership::ensure`]
/// before every irreversible step (a model stage, a freeze, a state-log
/// append), [`Ownership::lost`] raced by every wait that may outlive the
/// lease (a model stage). Once lost, lost for good.
#[async_trait]
pub(crate) trait Ownership: Send + Sync {
    /// Renew every lease now: `Ok` while each is still this process's;
    /// else the [`LeaseLost`] error — and every later call fails the same.
    async fn ensure(&self) -> anyhow::Result<()>;
    /// Resolves once a lease is known lost (by a renewal: the process's
    /// renewal tasks or an [`Ownership::ensure`]); never for [`Unleased`].
    async fn lost(&self) -> LeaseLost;
}

/// No lease to keep (a replay, a test): always owned, never lost.
pub(crate) struct Unleased;

#[async_trait]
impl Ownership for Unleased {
    async fn ensure(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn lost(&self) -> LeaseLost {
        std::future::pending().await
    }
}
