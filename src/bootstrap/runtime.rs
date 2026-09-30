//! `tengu run` composition (inbound side: `adapters/inbound/run.rs`; operator
//! doc `docs/runtime-2026-09-30.md`):
//!
//! | Step | What |
//! |---|---|
//! | state dir | `[xmarket]` state dir (`SandboxSections::xm_state_dir`), else `<TENGU_HOME>/state`; `runtime.db` lives there |
//! | lease | `runtime:<sandbox>` (sandbox = `--sandbox`, else `default`), TTL 30 s, renewed every 10 s; held ⇒ this process refuses to start; lost ⇒ it stops (failed) |
//! | loops | every `[decision_loops.*]` built once (`bootstrap::decision::build_decision_loop`) behind one `LoopDispatch` — the process owns loop state |
//! | health | `HealthBoard`: `run-<sandbox>.json` + `loop/1:<name>` rows (loop agent's store) every `[runtime] heartbeat_secs`; `stopping` / `stopped` beats on shutdown; feeds register via [`Runtime::health`] |
//! | tasks | [`Runtime::spawn`] registers long-running tasks on the stop signal: the webhook router today; next wave (`rt-scheduler`) one task per `[feeds.<n>]` that submits via [`Runtime::loops`] |
//! | shutdown | [`Runtime::shutdown`]: stop signal → loops drain + tasks stop ≤ `[runtime] shutdown_grace_secs` → lease released |

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::adapters::outbound::observations::SqliteObservationStore;
use crate::adapters::outbound::runtime_store::SqliteRuntimeStore;
use crate::application::runtime::health::{heartbeat_task, write_beat, HealthBoard};
use crate::application::runtime::loops::{DrainReport, LoopDispatch, LoopHandler, LoopStats};
use crate::application::runtime::{keep_lease, LeaseTiming, Stop, StopRx, Stopper, Supervisor};
use crate::config::runtime::RuntimeConfig;
use crate::config::Config;
use crate::domain::observation::now_ms;
use crate::domain::runtime::{lease_resource, RunState, RunnerLease};
use crate::domain::secrets::SecretRegistry;
use crate::ports::decision::Escalator;
use crate::ports::observation::ObservationStore;
use crate::ports::runtime::RuntimeStore;

/// Runner name: the `--sandbox` name, else `default`.
pub(crate) fn runner_name(config: &Config) -> String {
    config
        .sandbox_name
        .clone()
        .unwrap_or_else(|| "default".to_string())
}

/// Where `runtime.db` lives: the `[xmarket]` state dir every agent sees
/// (`AgentConfig::sandbox`), else `<TENGU_HOME>/state`.
pub(crate) fn runtime_state_dir(config: &Config) -> PathBuf {
    let xm = config
        .agents
        .values()
        .find_map(|a| a.sandbox.xm_state_dir.clone());
    crate::config::runtime::state_dir(xm.as_deref(), &crate::config::paths::resolve_tengu_home())
}

/// Workspace of `[agents.<agent>]` as its loop sees it: `workspace` (`~`
/// expanded), else the process cwd — the `bootstrap/decision.rs` rule, so
/// `loop/1` rows land in the store the loop reads.
pub(crate) fn agent_workspace(config: &Config, agent: &str) -> PathBuf {
    config
        .agents
        .get(agent)
        .and_then(|a| a.workspace.as_ref())
        .map(|p| crate::config::paths::expand_tilde(p))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Loop handlers + each loop agent's observation store (for `loop/1` rows).
type BuiltLoops = (
    BTreeMap<String, Arc<dyn LoopHandler>>,
    BTreeMap<String, Arc<dyn ObservationStore>>,
);

/// What [`Runtime::shutdown`] did.
#[derive(Debug)]
pub(crate) struct ShutdownReport {
    pub stop: Stop,
    pub loops: DrainReport,
    /// Final per-loop counters since start.
    pub loop_stats: BTreeMap<String, LoopStats>,
    pub aborted_tasks: Vec<String>,
    pub lease_released: bool,
}

/// One running `tengu run`: lease, loops, health, supervised tasks.
pub(crate) struct Runtime {
    sandbox: String,
    state_dir: PathBuf,
    cfg: RuntimeConfig,
    store: Arc<dyn RuntimeStore>,
    lease: RunnerLease,
    started_at_ms: i64,
    supervisor: Supervisor,
    loops: Arc<LoopDispatch>,
    health: Arc<HealthBoard>,
}

/// Take the lease, build every decision loop, start the lease keeper and
/// the heartbeat. `escalator` comes from the inbound side (the webhook
/// listener's orchestrator escalator when built with `--features webhooks`).
pub(crate) async fn start(
    config: &Config,
    secrets: Arc<SecretRegistry>,
    escalator: Option<Arc<dyn Escalator>>,
) -> Result<Runtime> {
    let mut rt = Runtime::begin(
        runner_name(config),
        runtime_state_dir(config),
        config.runtime.clone(),
        LeaseTiming::default(),
    )
    .await?;
    match build_loops(config, secrets, escalator) {
        Ok((handlers, stores)) => {
            rt.launch(handlers, stores);
            Ok(rt)
        }
        Err(e) => {
            rt.shutdown().await;
            Err(e)
        }
    }
}

fn build_loops(
    config: &Config,
    secrets: Arc<SecretRegistry>,
    escalator: Option<Arc<dyn Escalator>>,
) -> Result<BuiltLoops> {
    let (mut handlers, mut stores): BuiltLoops = Default::default();
    let mut names: Vec<&String> = config.decision_loops.keys().collect();
    names.sort();
    for name in names {
        let dl = crate::bootstrap::decision::build_decision_loop(
            config,
            name,
            escalator.clone(),
            Arc::clone(&secrets),
        )
        .with_context(|| format!("build [decision_loops.{name}]"))?;
        handlers.insert(name.clone(), dl as Arc<dyn LoopHandler>);
        let workspace = agent_workspace(config, &config.decision_loops[name].agent);
        match SqliteObservationStore::open(&workspace) {
            Ok(s) => {
                stores.insert(name.clone(), Arc::new(s) as Arc<dyn ObservationStore>);
            }
            Err(e) => {
                let error = format!("{e:#}");
                warn!(decision_loop = %name, %error, "observation store unavailable; no loop/1 rows");
            }
        }
    }
    Ok((handlers, stores))
}

fn holder_id() -> String {
    let host = sysinfo::System::host_name().unwrap_or_else(|| "unknown-host".to_string());
    format!("{host}:{}:{}", std::process::id(), uuid::Uuid::new_v4())
}

impl Runtime {
    /// Open `<state_dir>/runtime.db`, take `runtime:<sandbox>` (refused ⇒
    /// error naming the holder) and start renewing it. No loops yet.
    pub(crate) async fn begin(
        sandbox: String,
        state_dir: PathBuf,
        cfg: RuntimeConfig,
        timing: LeaseTiming,
    ) -> Result<Self> {
        let store = SqliteRuntimeStore::open(&state_dir)?;
        let db = store.path().display().to_string();
        let store: Arc<dyn RuntimeStore> = Arc::new(store);
        let (resource, holder) = (lease_resource(&sandbox), holder_id());
        let now = now_ms();
        let lease = store
            .acquire_lease(&resource, &holder, timing.ttl_ms, now)
            .await
            .with_context(|| format!("take lease `{resource}` in {db}"))?;
        if !lease.granted {
            anyhow::bail!(
                "sandbox `{sandbox}` is already running: lease `{resource}` in {db} is held by \
                 `{}` for {} s more. Stop that `tengu run` first (SIGTERM drains it); if it \
                 crashed, retry once the lease expires.",
                lease.current_holder,
                lease.remaining_secs(now)
            );
        }
        let mut supervisor = Supervisor::new();
        let (keeper_store, keeper_lease, stopper) =
            (Arc::clone(&store), lease.clone(), supervisor.stopper());
        supervisor.spawn("lease", move |stop| {
            keep_lease(keeper_store, keeper_lease, timing, stopper, stop)
        });
        info!(%sandbox, %resource, %holder, state_dir = %state_dir.display(), "runtime lease taken");
        let loops = Arc::new(LoopDispatch::new(
            BTreeMap::new(),
            cfg.max_decisions_in_flight,
        ));
        let health = Arc::new(HealthBoard::new(
            &sandbox,
            &holder,
            now,
            cfg.heartbeat_secs,
            BTreeMap::new(),
        ));
        Ok(Self {
            sandbox,
            state_dir,
            cfg,
            store,
            lease,
            started_at_ms: now,
            supervisor,
            loops,
            health,
        })
    }

    /// Install the loop handlers (+ each loop agent's observation store for
    /// its `loop/1` row) and start serving them and the heartbeat — once,
    /// before anything submits events or registers feeds.
    pub(crate) fn launch(
        &mut self,
        handlers: BTreeMap<String, Arc<dyn LoopHandler>>,
        loop_stores: BTreeMap<String, Arc<dyn ObservationStore>>,
    ) {
        self.loops = Arc::new(LoopDispatch::new(
            handlers,
            self.cfg.max_decisions_in_flight,
        ));
        self.health = Arc::new(HealthBoard::new(
            &self.sandbox,
            &self.lease.holder,
            self.started_at_ms,
            self.cfg.heartbeat_secs,
            loop_stores,
        ));
        let (board, loops, store) = (
            Arc::clone(&self.health),
            Arc::clone(&self.loops),
            Arc::clone(&self.store),
        );
        self.supervisor.spawn("heartbeat", move |stop| {
            heartbeat_task(board, loops, store, stop)
        });
    }

    /// Where feeds report (`HealthBoard::feed` → `FeedWriter`).
    #[allow(dead_code)] // rt-scheduler (next wave) registers each [feeds.<n>] here
    pub(crate) fn health(&self) -> Arc<HealthBoard> {
        Arc::clone(&self.health)
    }

    pub(crate) fn sandbox(&self) -> &str {
        &self.sandbox
    }

    pub(crate) fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// Lease holder id: `<host>:<pid>:<uuid>`.
    pub(crate) fn holder(&self) -> &str {
        &self.lease.holder
    }

    /// Where loop events go (webhook endpoints now, feeds next wave).
    pub(crate) fn loops(&self) -> Arc<LoopDispatch> {
        Arc::clone(&self.loops)
    }

    pub(crate) fn stopper(&self) -> Stopper {
        self.supervisor.stopper()
    }

    pub(crate) fn stop_rx(&self) -> StopRx {
        self.supervisor.stopper().subscribe()
    }

    /// Register a long-running task (see `Supervisor::spawn`): the webhook
    /// server today, one task per feed next wave.
    #[cfg_attr(not(feature = "webhooks"), allow(dead_code))]
    pub(crate) fn spawn<F, Fut>(&mut self, name: impl Into<String>, f: F)
    where
        F: FnOnce(StopRx) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.supervisor.spawn(name, f);
    }

    /// Stop (if not yet requested), drain loops and tasks until
    /// `shutdown_grace_secs`, then release the lease. The heartbeat says
    /// `stopping` during the drain and `stopped` after it.
    pub(crate) async fn shutdown(self) -> ShutdownReport {
        let Runtime {
            cfg,
            store,
            lease,
            supervisor,
            loops,
            health,
            ..
        } = self;
        let stopper = supervisor.stopper();
        stopper.stop("shutdown", false);
        let stop = stopper.cause().unwrap_or(Stop {
            reason: "shutdown".into(),
            failed: false,
        });
        let beat = |state| {
            let (health, loops, store, reason) = (&health, &loops, &store, &stop.reason);
            async move {
                let hb = health
                    .beat(&loops.stats(), state, Some(reason), now_ms())
                    .await;
                write_beat(&**store, &hb).await;
            }
        };
        beat(RunState::Stopping).await;
        let deadline = Instant::now() + Duration::from_secs(cfg.shutdown_grace_secs);
        let (drained, aborted_tasks) =
            tokio::join!(loops.drain(deadline), supervisor.shutdown(deadline));
        beat(RunState::Stopped).await;
        let lease_released = match store.release_lease(&lease.resource, &lease.holder).await {
            Ok(()) => true,
            Err(e) => {
                let error = format!("{e:#}");
                warn!(resource = %lease.resource, %error, "lease release failed; it expires by itself");
                false
            }
        };
        ShutdownReport {
            stop,
            loops: drained,
            loop_stats: loops.stats(),
            aborted_tasks,
            lease_released,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::outbound::runtime_store::read_heartbeat;
    use crate::application::observe::tests::MemStore;
    use crate::application::runtime::loops::tests::SlowLoop;
    use crate::application::runtime::stopped;
    use serde_json::Value;

    fn slow_timing() -> LeaseTiming {
        LeaseTiming {
            ttl_ms: 60_000,
            renew_ms: 60_000,
        }
    }

    async fn begin(dir: &Path, timing: LeaseTiming) -> Result<Runtime> {
        Runtime::begin(
            "xmarket-weekend".into(),
            dir.to_path_buf(),
            RuntimeConfig::default(),
            timing,
        )
        .await
    }

    #[tokio::test]
    async fn a_second_instance_is_refused_until_the_first_stops() {
        let dir = tempfile::tempdir().unwrap();
        let first = begin(dir.path(), slow_timing()).await.unwrap();
        let holder = first.holder().to_string();
        assert!(holder.contains(&format!(":{}:", std::process::id())));
        let err = format!(
            "{:#}",
            begin(dir.path(), slow_timing()).await.err().unwrap()
        );
        assert!(
            err.contains("sandbox `xmarket-weekend` is already running"),
            "{err}"
        );
        assert!(err.contains("`runtime:xmarket-weekend`"), "{err}");
        assert!(err.contains(&holder), "names the holder in full: {err}");
        let report = first.shutdown().await;
        assert!(report.lease_released && !report.stop.failed);
        assert!(report.aborted_tasks.is_empty(), "{report:?}");
        let again = begin(dir.path(), slow_timing()).await.unwrap();
        again.shutdown().await;
    }

    #[tokio::test]
    async fn a_crashed_runners_lease_is_taken_over_after_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let ghost = SqliteRuntimeStore::open(dir.path()).unwrap();
        let r = lease_resource("xmarket-weekend");
        assert!(
            ghost
                .acquire_lease(&r, "ghost:1:dead", 150, now_ms())
                .await
                .unwrap()
                .granted
        );
        let err = format!(
            "{:#}",
            begin(dir.path(), slow_timing()).await.err().unwrap()
        );
        assert!(err.contains("`ghost:1:dead`"), "{err}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let rt = begin(dir.path(), slow_timing()).await.unwrap();
        rt.shutdown().await;
    }

    fn launch_slow(rt: &mut Runtime, ms: u64) -> (Arc<SlowLoop>, Arc<MemStore>) {
        let slow = SlowLoop::new(ms);
        let store = Arc::new(MemStore::default());
        rt.launch(
            BTreeMap::from([(
                "xm_main".to_string(),
                Arc::clone(&slow) as Arc<dyn LoopHandler>,
            )]),
            BTreeMap::from([(
                "xm_main".to_string(),
                Arc::clone(&store) as Arc<dyn ObservationStore>,
            )]),
        );
        (slow, store)
    }

    #[tokio::test]
    async fn shutdown_drains_a_running_loop_event() {
        let dir = tempfile::tempdir().unwrap();
        let mut rt = begin(dir.path(), slow_timing()).await.unwrap();
        let (slow, _) = launch_slow(&mut rt, 200);
        rt.loops()
            .submit("xm_main", Value::Null, "s-1".into())
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        let loops = rt.loops();
        rt.stopper().stop("SIGTERM", false);
        let t0 = std::time::Instant::now();
        let report = rt.shutdown().await;
        assert!(t0.elapsed() < Duration::from_secs(5));
        assert_eq!(report.stop.reason, "SIGTERM");
        assert_eq!(
            report.loops,
            DrainReport {
                finished: 1,
                dropped: 0,
                aborted: 0
            }
        );
        assert!(report.aborted_tasks.is_empty(), "{report:?}");
        assert_eq!(loops.stats()["xm_main"].completed, 1);
        assert_eq!(*slow.seen.lock().unwrap(), ["s-1"]);
    }

    #[tokio::test]
    async fn heartbeat_file_and_loop_rows_track_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut rt = begin(dir.path(), slow_timing()).await.unwrap();
        let (_, store) = launch_slow(&mut rt, 1);
        let read = || read_heartbeat(dir.path(), "xmarket-weekend").unwrap();
        for _ in 0..400 {
            if read().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let hb = read().expect("first beat at launch");
        assert_eq!((hb.state, hb.pid), (RunState::Running, std::process::id()));
        assert_eq!(hb.holder, rt.holder());
        assert!(hb.loops.contains_key("xm_main"));
        let row = store.get("loop/1:xm_main").await.unwrap();
        assert_eq!(
            row.expect("loop row").features["in_flight"],
            serde_json::json!(0)
        );

        rt.stopper().stop("SIGTERM", false);
        rt.shutdown().await;
        let hb = read().unwrap();
        assert_eq!(hb.state, RunState::Stopped);
        assert_eq!(hb.stop_reason.as_deref(), Some("SIGTERM"));
        let live = crate::domain::runtime::live_verdict(
            "xmarket-weekend",
            &crate::domain::runtime::HeartbeatRead::Found(hb),
            &[],
            now_ms(),
            crate::domain::runtime::LiveKnobs {
                heartbeat_stale_secs: 30,
            },
        );
        assert!(!live.ok(), "a stopped run is not live: {live:?}");
    }

    #[tokio::test]
    async fn losing_the_lease_stops_the_runtime_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let fast = LeaseTiming {
            ttl_ms: 60_000,
            renew_ms: 20,
        };
        let rt = begin(dir.path(), fast).await.unwrap();
        let thief = SqliteRuntimeStore::open(dir.path()).unwrap();
        let r = lease_resource("xmarket-weekend");
        thief.release_lease(&r, rt.holder()).await.unwrap();
        assert!(
            thief
                .acquire_lease(&r, "thief:2:x", 60_000, now_ms())
                .await
                .unwrap()
                .granted
        );
        let stop = tokio::time::timeout(Duration::from_secs(5), stopped(&mut rt.stop_rx()))
            .await
            .expect("lease keeper noticed");
        assert!(stop.failed);
        assert!(stop.reason.contains("`thief:2:x`"), "{}", stop.reason);
        let report = rt.shutdown().await;
        assert!(report.stop.failed);
        // The release is holder-scoped: the thief keeps its lease.
        assert!(
            !thief
                .acquire_lease(&r, "third", 60_000, now_ms())
                .await
                .unwrap()
                .granted
        );
    }
}
