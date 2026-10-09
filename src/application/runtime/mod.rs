//! `tengu run` use cases, composed by `bootstrap/runtime.rs` (operator doc
//! `docs/runtime-2026-09-30.md`):
//!
//! | Piece | Role |
//! |---|---|
//! | [`Supervisor`] | named long-running tasks sharing one stop signal (`tokio::sync::watch`); a task that ends before the stop fails the run; shutdown waits until a deadline, then aborts |
//! | [`HeldLeases`] | the process's leases as its renewal tasks and its writers share them (`ports::runtime::Ownership`): one known expiry per lease, one sticky loss — a use case renews them before every irreversible write (`ensure`) and races its long waits against the loss (`lost`) |
//! | [`keep_lease`] | renews one lease of [`HeldLeases`] (`runtime:<sandbox>`, `state:<dir>`) every `renew_ms`; a lost lease stops the run (failed) |
//! | [`loops::LoopDispatch`] | loop events: one at a time per loop, `[runtime] max_decisions_in_flight` across loops, drain on shutdown |
//! | [`health::HealthBoard`] | heartbeat file + `loop/1` / `feed/1` rows every `[runtime] heartbeat_secs`; `FeedWriter` for the scheduler |
//! | [`feeds::run_feed`] | one `[feeds.<n>]`: fire on its schedule (`Clock`), call the agent's tool or tick a loop, back off on errors, report health |

pub(crate) mod feeds;
pub(crate) mod health;
pub(crate) mod loops;

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tracing::{error, warn};

use crate::domain::observation::now_ms;
use crate::domain::runtime::{RunnerLease, LEASE_RENEW_MS, LEASE_TTL_MS};
use crate::ports::runtime::{LeaseLost, Ownership, RuntimeStore};

/// Why the runtime stops. The first request wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stop {
    pub reason: String,
    /// Abnormal stop (lease lost, a task died): the process exits non-zero.
    pub failed: bool,
}

/// The stop signal as a task sees it: `None` until a stop is requested.
pub(crate) type StopRx = watch::Receiver<Option<Stop>>;

/// Resolve once the stop signal has fired (at once when it already has).
pub(crate) async fn stopped(rx: &mut StopRx) -> Stop {
    let fired = rx
        .wait_for(Option::is_some)
        .await
        .ok()
        .and_then(|s| s.clone());
    fired.unwrap_or_else(|| Stop {
        reason: "stop signal dropped".into(),
        failed: false,
    })
}

/// Clonable handle that requests a stop.
#[derive(Clone)]
pub(crate) struct Stopper(Arc<watch::Sender<Option<Stop>>>);

impl Stopper {
    fn new() -> Self {
        Self(Arc::new(watch::channel(None).0))
    }

    /// Request a stop. `false` when one was already requested (first wins).
    pub(crate) fn stop(&self, reason: impl Into<String>, failed: bool) -> bool {
        let reason = reason.into();
        self.0.send_if_modified(|s| {
            if s.is_some() {
                return false;
            }
            *s = Some(Stop { reason, failed });
            true
        })
    }

    pub(crate) fn subscribe(&self) -> StopRx {
        self.0.subscribe()
    }

    /// The stop requested so far, if any.
    pub(crate) fn cause(&self) -> Option<Stop> {
        self.0.borrow().clone()
    }
}

/// Named long-running tasks sharing one stop signal — the seam where
/// `tengu run` registers the lease keeper, the heartbeat, the webhook server
/// and one task per feed.
pub(crate) struct Supervisor {
    stopper: Stopper,
    tasks: Vec<(String, JoinHandle<()>)>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    pub(crate) fn new() -> Self {
        Self {
            stopper: Stopper::new(),
            tasks: Vec::new(),
        }
    }

    pub(crate) fn stopper(&self) -> Stopper {
        self.stopper.clone()
    }

    /// Spawn a long-running task. `f` gets the stop signal and must return
    /// soon after it fires; returning (or panicking) earlier stops the whole
    /// runtime as failed.
    pub(crate) fn spawn<F, Fut>(&mut self, name: impl Into<String>, f: F)
    where
        F: FnOnce(StopRx) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let name = name.into();
        let task = f(self.stopper.subscribe());
        let stopper = self.stopper.clone();
        let label = name.clone();
        let handle = tokio::spawn(async move {
            let what = match std::panic::AssertUnwindSafe(task).catch_unwind().await {
                Ok(()) => "exited",
                Err(_) => "panicked",
            };
            // `stop` is false when a stop was already requested: expected exit.
            if stopper.stop(format!("runtime task `{label}` {what}"), true) {
                error!(task = %label, "runtime task {what} before shutdown — stopping");
            }
        });
        self.tasks.push((name, handle));
    }

    /// Fire the stop signal (if not yet) and wait for every task until
    /// `deadline`; abort the rest. Returns the aborted task names.
    pub(crate) async fn shutdown(self, deadline: Instant) -> Vec<String> {
        self.stopper.stop("shutdown", false);
        let mut aborted = Vec::new();
        for (name, mut handle) in self.tasks {
            if tokio::time::timeout_at(deadline, &mut handle)
                .await
                .is_err()
            {
                handle.abort();
                warn!(task = %name, "runtime task still running at the shutdown deadline — aborted");
                aborted.push(name);
            }
        }
        aborted
    }
}

/// Lease timing: `domain::runtime` constants (tests shorten them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LeaseTiming {
    pub ttl_ms: i64,
    pub renew_ms: u64,
}

impl Default for LeaseTiming {
    fn default() -> Self {
        Self {
            ttl_ms: LEASE_TTL_MS,
            renew_ms: LEASE_RENEW_MS,
        }
    }
}

/// What a renewal that met a store error needs left on the known expiry to
/// keep the lease ([`HeldLeases`]): room for the write it guards.
pub(crate) const EXPIRY_GUARD_MS: i64 = 1_000;

/// The leases one process holds (`bootstrap::runtime::OwnerLeases`), shared
/// by its renewal tasks ([`keep_lease`]) and its writers ([`Ownership`]):
/// one known expiry per lease, one sticky loss (the first wins; a lost
/// lease is never owned again, even when it is free once more).
///
/// | A renewal of a lease | Means |
/// |---|---|
/// | granted, `acquired_at_ms` as taken | still this process's: its known expiry moves |
/// | granted, another `acquired_at_ms` | it lapsed and another holder had it since (the store keeps `acquired_at_ms` only across the same holder): lost |
/// | refused | another holder has it: lost |
/// | a store error with ≥ [`EXPIRY_GUARD_MS`] left before the known expiry | still this process's until then (no one can take it earlier); retried |
/// | a store error with less left | lost — a write after it could land past the expiry |
pub(crate) struct HeldLeases {
    store: Arc<dyn RuntimeStore>,
    ttl_ms: i64,
    /// As taken: resource, holder, `acquired_at_ms`.
    leases: Vec<RunnerLease>,
    /// The newest granted expiry of each lease.
    expires: Mutex<Vec<i64>>,
    lost: watch::Sender<Option<LeaseLost>>,
}

impl HeldLeases {
    /// `leases` as taken (granted), each renewed for `ttl_ms` from now.
    pub(crate) fn new(store: Arc<dyn RuntimeStore>, leases: Vec<RunnerLease>, ttl_ms: i64) -> Self {
        let expires = leases.iter().map(|l| l.expires_at_ms).collect();
        Self {
            store,
            ttl_ms,
            leases,
            expires: Mutex::new(expires),
            lost: watch::channel(None).0,
        }
    }

    /// The loss so far, if any.
    pub(crate) fn lost_now(&self) -> Option<LeaseLost> {
        self.lost.borrow().clone()
    }

    /// Record a loss — the first wins — and return the one in force.
    fn lose(&self, reason: String) -> LeaseLost {
        let first = LeaseLost { reason };
        let mine = first.clone();
        self.lost.send_if_modified(|l| {
            if l.is_some() {
                return false;
            }
            *l = Some(first);
            true
        });
        self.lost_now().unwrap_or(mine)
    }

    fn known_expiry(&self, i: usize) -> i64 {
        self.expires.lock().unwrap_or_else(|p| p.into_inner())[i]
    }

    /// Renew lease `i` (table above); `Err` = lost, now or before.
    pub(crate) async fn renew(&self, i: usize) -> Result<(), LeaseLost> {
        if let Some(lost) = self.lost_now() {
            return Err(lost);
        }
        let lease = &self.leases[i];
        let now = now_ms();
        let known = self.known_expiry(i);
        match self
            .store
            .acquire_lease(&lease.resource, &lease.holder, self.ttl_ms, now)
            .await
        {
            Ok(l) if l.granted && l.acquired_at_ms == lease.acquired_at_ms => {
                let mut e = self.expires.lock().unwrap_or_else(|p| p.into_inner());
                e[i] = e[i].max(l.expires_at_ms);
                Ok(())
            }
            Ok(l) if l.granted => Err(self.lose(format!(
                "lease `{}` lapsed and another holder had it since this process took it \
                 (acquired at {} ms then, {} ms now)",
                lease.resource, lease.acquired_at_ms, l.acquired_at_ms
            ))),
            Ok(l) => Err(self.lose(format!(
                "lease `{}` lost to `{}` — another runner took over",
                l.resource, l.current_holder
            ))),
            Err(e) if now + EXPIRY_GUARD_MS >= known => Err(self.lose(format!(
                "lease `{}` not renewed before it expired: {e:#}",
                lease.resource
            ))),
            Err(e) => {
                let error = format!("{e:#}");
                warn!(resource = %lease.resource, %error, "lease renewal failed; retrying");
                Ok(())
            }
        }
    }
}

#[async_trait]
impl Ownership for HeldLeases {
    async fn ensure(&self) -> anyhow::Result<()> {
        for i in 0..self.leases.len() {
            self.renew(i).await?;
        }
        Ok(())
    }

    async fn lost(&self) -> LeaseLost {
        let mut rx = self.lost.subscribe();
        let lost = rx
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|l| l.clone());
        match lost {
            Some(l) => l,
            // The sender lives as long as `self`: unreachable.
            None => std::future::pending().await,
        }
    }
}

/// Renew lease `i` of `held` every `timing.renew_ms` until the stop signal
/// ([`HeldLeases::renew`]: a store error retries until the lease would have
/// expired). Lost ⇒ stop (failed), the loss's reason as the stop's.
pub(crate) async fn keep_lease(
    held: Arc<HeldLeases>,
    i: usize,
    timing: LeaseTiming,
    stopper: Stopper,
    mut stop: StopRx,
) {
    let mut tick = tokio::time::interval(Duration::from_millis(timing.renew_ms.max(1)));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tick.tick().await; // the first tick is immediate; the lease is fresh
    loop {
        tokio::select! {
            _ = stopped(&mut stop) => return,
            _ = tick.tick() => {}
        }
        if let Err(lost) = held.renew(i).await {
            stopper.stop(lost.reason, true);
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::runtime::lease_resource;
    use std::sync::Mutex;

    #[tokio::test]
    async fn first_stop_wins_and_every_subscriber_sees_it() {
        let sup = Supervisor::new();
        let s = sup.stopper();
        let mut rx = s.subscribe();
        assert!(s.cause().is_none());
        assert!(s.stop("SIGTERM", false));
        assert!(!s.stop("later", true), "second request is ignored");
        assert_eq!(
            stopped(&mut rx).await,
            Stop {
                reason: "SIGTERM".into(),
                failed: false
            }
        );
        // A late subscriber resolves at once.
        assert_eq!(stopped(&mut s.subscribe()).await.reason, "SIGTERM");
    }

    #[tokio::test]
    async fn shutdown_waits_for_cooperative_tasks_and_aborts_the_rest() {
        let mut sup = Supervisor::new();
        let done = Arc::new(Mutex::new(Vec::new()));
        let d = Arc::clone(&done);
        sup.spawn("polite", move |mut stop| async move {
            stopped(&mut stop).await;
            tokio::time::sleep(Duration::from_millis(20)).await; // flush
            d.lock().unwrap().push("polite");
        });
        sup.spawn("stubborn", |_stop| async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let stopper = sup.stopper();
        let t0 = Instant::now();
        let aborted = sup
            .shutdown(Instant::now() + Duration::from_millis(300))
            .await;
        assert_eq!(aborted, vec!["stubborn".to_string()]);
        assert_eq!(*done.lock().unwrap(), vec!["polite"]);
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert_eq!(stopper.cause().unwrap().reason, "shutdown");
        assert!(!stopper.cause().unwrap().failed);
    }

    #[tokio::test]
    async fn a_task_that_dies_early_stops_the_runtime_as_failed() {
        let mut sup = Supervisor::new();
        let mut rx = sup.stopper().subscribe();
        sup.spawn("webhooks", |_stop| async move {
            panic!("bind failed");
        });
        let stop = stopped(&mut rx).await;
        assert!(stop.failed);
        assert_eq!(stop.reason, "runtime task `webhooks` panicked");
        sup.shutdown(Instant::now()).await;
    }

    /// Scripted lease store: answers `granted` per call from a queue.
    struct FakeLeases(Mutex<Vec<anyhow::Result<bool>>>);

    #[async_trait::async_trait]
    impl RuntimeStore for FakeLeases {
        async fn acquire_lease(
            &self,
            resource: &str,
            holder: &str,
            ttl_ms: i64,
            now_ms: i64,
        ) -> anyhow::Result<RunnerLease> {
            let next = self.0.lock().unwrap().remove(0)?;
            Ok(RunnerLease {
                resource: resource.into(),
                holder: holder.into(),
                granted: next,
                current_holder: if next { holder.into() } else { "other".into() },
                acquired_at_ms: 0,
                expires_at_ms: now_ms + ttl_ms,
            })
        }
        async fn release_lease(&self, _r: &str, _h: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn write_heartbeat(
            &self,
            _hb: &crate::domain::runtime::Heartbeat,
        ) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn lease(expires_at_ms: i64) -> RunnerLease {
        RunnerLease {
            resource: lease_resource("s"),
            holder: "me".into(),
            granted: true,
            current_holder: "me".into(),
            acquired_at_ms: 0,
            expires_at_ms,
        }
    }

    const FAST: LeaseTiming = LeaseTiming {
        ttl_ms: 60_000,
        renew_ms: 10,
    };

    /// `lease` held through `store`, renewed for `FAST.ttl_ms`.
    fn held(store: Arc<dyn RuntimeStore>, lease: RunnerLease) -> Arc<HeldLeases> {
        Arc::new(HeldLeases::new(store, vec![lease], FAST.ttl_ms))
    }

    #[tokio::test]
    async fn lease_lost_to_another_runner_stops_as_failed() {
        let store = Arc::new(FakeLeases(Mutex::new(vec![Ok(true), Ok(false)])));
        let sup = Supervisor::new();
        let stopper = sup.stopper();
        let leases = held(store, lease(now_ms() + 60_000));
        keep_lease(
            Arc::clone(&leases),
            0,
            FAST,
            stopper.clone(),
            stopper.subscribe(),
        )
        .await;
        let stop = stopper.cause().unwrap();
        assert!(stop.failed);
        assert!(stop.reason.contains("lost to `other`"), "{}", stop.reason);
        // The writers see the same loss, for good.
        assert_eq!(leases.lost_now().unwrap().reason, stop.reason);
        let e = format!("{:#}", leases.ensure().await.unwrap_err());
        assert!(
            e.starts_with("lease_lost: lease `runtime:s` lost to `other`"),
            "{e}"
        );
        assert_eq!(leases.lost().await.reason, stop.reason);
    }

    #[tokio::test]
    async fn renewal_errors_are_retried_until_the_lease_expires() {
        let errs = (0..3).map(|_| Err(anyhow::anyhow!("database is locked")));
        let store = Arc::new(FakeLeases(Mutex::new(errs.collect())));
        let stopper = Supervisor::new().stopper();
        // Already expired: the first failed renewal stops the run.
        keep_lease(
            held(store.clone(), lease(now_ms() - 1)),
            0,
            FAST,
            stopper.clone(),
            stopper.subscribe(),
        )
        .await;
        let stop = stopper.cause().unwrap();
        assert!(
            stop.failed && stop.reason.contains("not renewed"),
            "{stop:?}"
        );
        assert_eq!(store.0.lock().unwrap().len(), 2, "stopped after one try");
        // Before the known expiry a store error is retried: still owned.
        let store = Arc::new(FakeLeases(Mutex::new(vec![
            Err(anyhow::anyhow!("database is locked")),
            Ok(true),
        ])));
        let leases = held(store, lease(now_ms() + 60_000));
        leases.ensure().await.unwrap();
        leases.ensure().await.unwrap();
        assert!(leases.lost_now().is_none());
    }

    #[tokio::test]
    async fn keep_lease_returns_on_stop() {
        let store = Arc::new(FakeLeases(Mutex::new(
            (0..1000).map(|_| Ok(true)).collect(),
        )));
        let stopper = Supervisor::new().stopper();
        let rx = stopper.subscribe();
        let s2 = stopper.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            s2.stop("SIGINT", false);
        });
        let leases = held(store, lease(now_ms() + 60_000));
        keep_lease(Arc::clone(&leases), 0, FAST, stopper.clone(), rx).await;
        assert!(!stopper.cause().unwrap().failed);
        assert!(leases.lost_now().is_none(), "a stop is not a loss");
    }

    /// A lease that lapsed while this process slept (another holder took
    /// it, ran, released it) is granted again on renewal — with another
    /// `acquired_at_ms`: lost, and never owned again though it is ours in
    /// the store once more. An `Unleased` caller is always owned.
    #[tokio::test]
    async fn a_lapsed_lease_retaken_since_is_lost_for_good() {
        use crate::adapters::outbound::runtime_store::SqliteRuntimeStore;
        use crate::ports::runtime::Unleased;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteRuntimeStore::open(dir.path()).unwrap());
        let r = lease_resource("s");
        let now = now_ms();
        let mine = store
            .acquire_lease(&r, "me", 1_000, now - 5_000)
            .await
            .unwrap();
        assert!(mine.granted);
        let leases = Arc::new(HeldLeases::new(store.clone(), vec![mine], 60_000));
        // Expired at now − 4 s: another holder ran and released meanwhile.
        let other = store
            .acquire_lease(&r, "other", 1_000, now - 3_000)
            .await
            .unwrap();
        assert!(other.granted);
        store.release_lease(&r, "other").await.unwrap();
        let e = format!("{:#}", leases.ensure().await.unwrap_err());
        assert!(
            e.starts_with("lease_lost: lease `runtime:s` lapsed and another holder had it"),
            "{e}"
        );
        let again = format!("{:#}", leases.ensure().await.unwrap_err());
        assert_eq!(again, e, "sticky");
        assert!(LeaseLost::of(&leases.ensure().await.unwrap_err()).is_some());
        let stopper = Supervisor::new().stopper();
        keep_lease(leases, 0, FAST, stopper.clone(), stopper.subscribe()).await;
        assert!(stopper.cause().unwrap().reason.contains("lapsed"));
        Unleased.ensure().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), Unleased.lost())
                .await
                .is_err(),
            "never lost"
        );
    }
}
