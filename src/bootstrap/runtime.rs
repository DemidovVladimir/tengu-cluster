//! `tengu run` composition (inbound side: `adapters/inbound/run.rs`; operator
//! doc `docs/runtime-2026-09-30.md`):
//!
//! | Step | What |
//! |---|---|
//! | state dir | `[xmarket]` state dir (`SandboxSections::xm_state_dir`), else `<TENGU_HOME>/state`; `runtime.db` lives there |
//! | leases ([`LeasePlan`], [`OwnerLeases`]) | `runtime:<sandbox>` (sandbox = `--sandbox`, else `default`), then — for an `[xmarket]` state dir, the ledger's — `state:<dir name>`: one owner per ledger, whichever sandbox names that `[xmarket] state`; taken all or none, TTL 30 s, renewed every 10 s; held ⇒ this process refuses to start; lost ⇒ it stops (failed). `tengu webhooks` takes the same leases (`inbound/webhooks.rs`) |
//! | loops | every `[decision_loops.*]` built once (`bootstrap::decision::build_decision_loop`) behind one `LoopDispatch` — the process owns loop state |
//! | health | `HealthBoard`: `run-<sandbox>.json` + `loop/1:<name>` rows (loop agent's store) every `[runtime] heartbeat_secs`; `stopping` / `stopped` beats on shutdown; feeds register via [`Runtime::health`] |
//! | feeds | [`start_feeds`]: one task `feed:<name>` per `[feeds.<n>]` (`application::runtime::feeds::run_feed`, `SystemClock`, `jitter01`); a tool feed calls through its agent's executor (`decision::agent_tool_executor`, one per agent; a tool it cannot run fails the start) under `egress::AttributedExecutor` (egress records: the feed's agent, session `feed:<name>`, the call id), a tick feed submits to [`Runtime::loops`]; `feed/1:<name>` rows go to the feed agent's store (tick: the target loop agent's) |
//! | tasks | [`Runtime::spawn`] registers long-running tasks on the stop signal: the webhook router and the feeds |
//! | shutdown | [`Runtime::shutdown`]: stop signal → loops drain + tasks stop ≤ `[runtime] shutdown_grace_secs` → leases released |

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::adapters::outbound::clock::SystemClock;
use crate::adapters::outbound::egress::{AttributedExecutor, CallSession};
use crate::adapters::outbound::observations::open_observation_store;
use crate::adapters::outbound::rate_limit::jitter01;
use crate::adapters::outbound::runtime_store::SqliteRuntimeStore;
use crate::application::runtime::feeds::{run_feed, FeedEnv, FeedJob, FeedSpec, Rand01};
use crate::application::runtime::health::{heartbeat_task, write_beat, HealthBoard};
use crate::application::runtime::loops::{DrainReport, LoopDispatch, LoopHandler, LoopStats};
use crate::application::runtime::{keep_lease, LeaseTiming, Stop, StopRx, Stopper, Supervisor};
use crate::config::feeds::{FeedConfig, FeedKind};
use crate::config::runtime::RuntimeConfig;
use crate::config::Config;
use crate::domain::observation::now_ms;
use crate::domain::runtime::{lease_resource, state_lease_resource, RunState, RunnerLease};
use crate::domain::secrets::SecretRegistry;
use crate::ports::clock::Clock;
use crate::ports::decision::Escalator;
use crate::ports::engine::ToolExecutor;
use crate::ports::observation::ObservationStore;
use crate::ports::runtime::RuntimeStore;

/// Runner name: the `--sandbox` name, else `default`.
pub(crate) fn runner_name(config: &Config) -> String {
    config
        .sandbox_name
        .clone()
        .unwrap_or_else(|| crate::config::sections::DEFAULT_SANDBOX.to_string())
}

/// The `[xmarket]` state dir every agent sees (`AgentConfig::sandbox`) —
/// the ledger's — when the sandbox has one.
fn xm_state_dir(config: &Config) -> Option<PathBuf> {
    config
        .agents
        .values()
        .find_map(|a| a.sandbox.xm_state_dir.clone())
}

/// Where `runtime.db` lives: the `[xmarket]` state dir every agent sees
/// (`AgentConfig::sandbox`), else `<TENGU_HOME>/state`.
pub(crate) fn runtime_state_dir(config: &Config) -> PathBuf {
    crate::config::runtime::state_dir(
        xm_state_dir(config).as_deref(),
        &crate::config::paths::resolve_tengu_home(),
    )
}

/// What a long-running process of a sandbox (`tengu run`, `tengu webhooks`)
/// leases in `<state_dir>/runtime.db` (module table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeasePlan {
    /// Runner name ([`runner_name`]): `runtime:<sandbox>`.
    pub sandbox: String,
    /// [`runtime_state_dir`].
    pub state_dir: PathBuf,
    /// The state dir is an `[xmarket]` one — it holds the ledger: also
    /// `state:<dir name>`, so two sandboxes naming the same `[xmarket]
    /// state` never run at once. `<TENGU_HOME>/state` (no `[xmarket]`) is
    /// install-wide: no state lease there.
    pub ledger: bool,
}

/// Which lease a refusal is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaseKind {
    Runner,
    State,
}

impl LeasePlan {
    pub(crate) fn of(config: &Config) -> Self {
        let xm = xm_state_dir(config);
        Self {
            sandbox: runner_name(config),
            ledger: xm.is_some(),
            state_dir: runtime_state_dir(config),
        }
    }

    /// The leases in the order they are taken.
    fn resources(&self) -> Vec<(String, LeaseKind)> {
        let mut out = vec![(lease_resource(&self.sandbox), LeaseKind::Runner)];
        if self.ledger {
            let name = self.state_dir.file_name().map_or_else(
                || self.state_dir.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            out.push((state_lease_resource(&name), LeaseKind::State));
        }
        out
    }

    /// Why `lease` (refused) stops this process from starting — the holder
    /// in full.
    fn refusal(&self, kind: LeaseKind, db: &str, lease: &RunnerLease, now: i64) -> anyhow::Error {
        let (resource, holder, secs) = (
            &lease.resource,
            &lease.current_holder,
            lease.remaining_secs(now),
        );
        match kind {
            LeaseKind::Runner => anyhow!(
                "sandbox `{}` is already running: lease `{resource}` in {db} is held by \
                 `{holder}` for {secs} s more. Stop that `tengu run` / `tengu webhooks` first \
                 (SIGTERM drains it); if it crashed, retry once the lease expires.",
                self.sandbox
            ),
            LeaseKind::State => anyhow!(
                "state dir {} already has an owner: lease `{resource}` in {db} is held by \
                 `{holder}` for {secs} s more — a `tengu run` or `tengu webhooks` of a sandbox \
                 naming the same [xmarket] state owns its ledger, and a ledger has one owner. \
                 Stop that process first (SIGTERM drains it), or give this sandbox its own \
                 [xmarket] state; if it crashed, retry once the lease expires.",
                self.state_dir.display()
            ),
        }
    }
}

/// The leases a process holds ([`LeasePlan`]): taken all or none, renewed
/// by [`OwnerLeases::keep`] on the process's supervisor, freed by
/// [`OwnerLeases::release`]. `tengu run` ([`Runtime`]) and `tengu webhooks`
/// hold the same set.
pub(crate) struct OwnerLeases {
    store: Arc<dyn RuntimeStore>,
    holder: String,
    leases: Vec<(RunnerLease, LeaseKind)>,
}

impl OwnerLeases {
    /// Open `<state dir>/runtime.db` and take the plan's leases in order —
    /// `runtime:<sandbox>`, then `state:<dir>` — for `ttl_ms`. A refusal
    /// frees what was taken and names the holder in full.
    pub(crate) async fn take(plan: &LeasePlan, ttl_ms: i64) -> Result<Self> {
        let store = SqliteRuntimeStore::open(&plan.state_dir)?;
        let db = store.path().display().to_string();
        let store: Arc<dyn RuntimeStore> = Arc::new(store);
        let mut taken = Self {
            store,
            holder: holder_id(),
            leases: Vec::new(),
        };
        for (resource, kind) in plan.resources() {
            let now = now_ms();
            let lease = taken
                .store
                .acquire_lease(&resource, &taken.holder, ttl_ms, now)
                .await
                .with_context(|| format!("take lease `{resource}` in {db}"));
            let refusal = match lease {
                Ok(l) if l.granted => {
                    taken.leases.push((l, kind));
                    continue;
                }
                Ok(l) => plan.refusal(kind, &db, &l, now),
                Err(e) => e,
            };
            taken.release().await;
            return Err(refusal);
        }
        Ok(taken)
    }

    /// One renewal task per lease on `supervisor` (`keep_lease`): a lost
    /// lease fires its stop signal (failed).
    pub(crate) fn keep(&self, supervisor: &mut Supervisor, timing: LeaseTiming) {
        for (lease, kind) in &self.leases {
            let (store, lease, stopper) =
                (Arc::clone(&self.store), lease.clone(), supervisor.stopper());
            let name = match kind {
                LeaseKind::Runner => "lease",
                LeaseKind::State => "state lease",
            };
            supervisor.spawn(name, move |stop| {
                keep_lease(store, lease, timing, stopper, stop)
            });
        }
    }

    /// `<host>:<pid>:<uuid>` — one id for every lease of this process.
    pub(crate) fn holder(&self) -> &str {
        &self.holder
    }

    /// The resources held, in the order taken.
    pub(crate) fn resources(&self) -> Vec<&str> {
        self.leases
            .iter()
            .map(|(l, _)| l.resource.as_str())
            .collect()
    }

    fn store(&self) -> Arc<dyn RuntimeStore> {
        Arc::clone(&self.store)
    }

    /// Free every lease (holder-scoped: someone else's stays), the last
    /// taken first. `false` when a release failed — it expires by itself.
    pub(crate) async fn release(&self) -> bool {
        let mut all = true;
        for (lease, _) in self.leases.iter().rev() {
            if let Err(e) = self
                .store
                .release_lease(&lease.resource, &lease.holder)
                .await
            {
                let error = format!("{e:#}");
                warn!(resource = %lease.resource, %error, "lease release failed; it expires by itself");
                all = false;
            }
        }
        all
    }
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

/// One running `tengu run`: leases, loops, health, supervised tasks.
pub(crate) struct Runtime {
    sandbox: String,
    state_dir: PathBuf,
    cfg: RuntimeConfig,
    store: Arc<dyn RuntimeStore>,
    owner: OwnerLeases,
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
        LeasePlan::of(config),
        config.runtime.clone(),
        LeaseTiming::default(),
    )
    .await?;
    let started = build_loops(config, Arc::clone(&secrets), escalator).and_then(|(h, s)| {
        rt.launch(h, s);
        start_feeds(config, &mut rt, &secrets, Arc::new(SystemClock))
    });
    match started {
        Ok(()) => Ok(rt),
        Err(e) => {
            rt.shutdown().await;
            Err(e)
        }
    }
}

/// One agent's tool executor + the tool names it runs.
type AgentTools = (Arc<dyn ToolExecutor>, Arc<BTreeSet<String>>);

/// One supervised task per `[feeds.<n>]`, in name order (module table).
/// Call after [`Runtime::launch`]: tick feeds need their target loop.
pub(crate) fn start_feeds(
    config: &Config,
    rt: &mut Runtime,
    secrets: &Arc<SecretRegistry>,
    clock: Arc<dyn Clock>,
) -> Result<()> {
    let rand01: Rand01 = Arc::new(jitter01);
    let mut executors: BTreeMap<String, AgentTools> = BTreeMap::new();
    let mut stores: BTreeMap<String, Option<Arc<dyn ObservationStore>>> = BTreeMap::new();
    for (name, feed) in &config.feeds {
        let (spec, agent) = feed_spec(config, name, feed, rt, secrets, &mut executors)
            .with_context(|| format!("start [feeds.{name}]"))?;
        let store = stores
            .entry(agent.clone())
            .or_insert_with(|| agent_store(config, &agent))
            .clone();
        let health = rt.health().feed(
            name,
            feed.required,
            feed.stale_after_secs(),
            store,
            clock.now_ms(),
        );
        info!(
            feed = %name,
            kind = %feed.kind,
            every_secs = ?feed.every_secs,
            windows = feed.windows.len(),
            at = ?feed.at,
            tz = %spec.schedule.zone.name(),
            required = feed.required,
            "feed started"
        );
        let env = FeedEnv {
            clock: Arc::clone(&clock),
            rand01: Arc::clone(&rand01),
            health,
        };
        rt.spawn(format!("feed:{name}"), move |stop| {
            run_feed(spec, env, stop)
        });
    }
    Ok(())
}

/// `[feeds.<name>]` built, + the agent whose store takes its `feed/1` row.
/// A tool feed whose agent cannot run its tool is refused here.
fn feed_spec(
    config: &Config,
    name: &str,
    feed: &FeedConfig,
    rt: &Runtime,
    secrets: &Arc<SecretRegistry>,
    executors: &mut BTreeMap<String, AgentTools>,
) -> Result<(FeedSpec, String)> {
    let schedule = feed.schedule().map_err(|e| anyhow!("{}", e.join("; ")))?;
    let (job, agent) = match feed.kind().map_err(|e| anyhow!("{e}"))? {
        FeedKind::Tool => {
            let (agent_name, tool) = match (&feed.agent, &feed.tool) {
                (Some(a), Some(t)) => (a.clone(), t.clone()),
                _ => anyhow::bail!("a tool feed needs `agent` and `tool`"),
            };
            let agent = config
                .agents
                .get(&agent_name)
                .ok_or_else(|| anyhow!("agent: no [agents.{agent_name}] block"))?;
            let (executor, runs) = executors
                .entry(agent_name.clone())
                .or_insert_with(|| {
                    let workspace = agent_workspace(config, &agent_name);
                    let (executor, runs) = crate::bootstrap::decision::agent_tool_executor(
                        config, agent, &workspace, secrets,
                    );
                    (executor, Arc::new(runs))
                })
                .clone();
            if !runs.contains(&tool) {
                anyhow::bail!(
                    "tool: [agents.{agent_name}] cannot run `{tool}` (not in its tools or \
                     workspace_tools, not built into this binary, or its plugin failed — see \
                     the log above)"
                );
            }
            // Egress records name the feed's agent + `feed:<name>` + the call id.
            let executor = Arc::new(AttributedExecutor::new(
                executor,
                &agent_name,
                CallSession::Fixed(format!("feed:{name}")),
            ));
            let job = FeedJob::Tool {
                executor,
                tool,
                calls: feed.calls(),
                concurrency: feed.concurrency(),
            };
            (job, agent_name)
        }
        FeedKind::Tick => {
            let target = feed.target.clone().unwrap_or_default();
            let (Some(dl), true) = (config.decision_loops.get(&target), rt.loops().has(&target))
            else {
                anyhow::bail!("target: decision loop `{target}` is not running in this process");
            };
            let job = FeedJob::Tick {
                loops: rt.loops(),
                target,
                event: feed.event.clone(),
            };
            (job, dl.agent.clone())
        }
    };
    let spec = FeedSpec {
        name: name.to_string(),
        schedule,
        jitter_pct: feed.jitter_pct,
        run_on_start: feed.run_on_start,
        job,
    };
    Ok((spec, agent))
}

/// `[agents.<agent>]`'s observation store (fail-soft: no `feed/1` rows).
fn agent_store(config: &Config, agent: &str) -> Option<Arc<dyn ObservationStore>> {
    let sections = config
        .agents
        .get(agent)
        .map(|a| Arc::clone(&a.sandbox))
        .unwrap_or_default();
    match open_observation_store(&agent_workspace(config, agent), &sections) {
        Ok(s) => Some(s),
        Err(e) => {
            let error = format!("{e:#}");
            warn!(%agent, %error, "observation store unavailable; no feed/1 rows");
            None
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
        let agent = &config.decision_loops[name].agent;
        let workspace = agent_workspace(config, agent);
        let sections = config
            .agents
            .get(agent)
            .map(|a| Arc::clone(&a.sandbox))
            .unwrap_or_default();
        // Same constructor as the tools: `loop/1` rows reach the recorder
        // when `[recorder] schemas` lists them.
        match open_observation_store(&workspace, &sections) {
            Ok(s) => {
                stores.insert(name.clone(), s);
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
    /// Open `<state_dir>/runtime.db`, take the plan's leases (refused ⇒
    /// error naming the holder, nothing held) and start renewing them. No
    /// loops yet.
    pub(crate) async fn begin(
        plan: LeasePlan,
        cfg: RuntimeConfig,
        timing: LeaseTiming,
    ) -> Result<Self> {
        let now = now_ms();
        let owner = OwnerLeases::take(&plan, timing.ttl_ms).await?;
        let mut supervisor = Supervisor::new();
        owner.keep(&mut supervisor, timing);
        let LeasePlan {
            sandbox, state_dir, ..
        } = plan;
        info!(
            %sandbox,
            leases = ?owner.resources(),
            holder = %owner.holder(),
            state_dir = %state_dir.display(),
            "runtime leases taken"
        );
        let loops = Arc::new(LoopDispatch::new(
            BTreeMap::new(),
            cfg.max_decisions_in_flight,
        ));
        let health = Arc::new(HealthBoard::new(
            &sandbox,
            owner.holder(),
            now,
            cfg.heartbeat_secs,
            BTreeMap::new(),
        ));
        Ok(Self {
            sandbox,
            state_dir,
            cfg,
            store: owner.store(),
            owner,
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
            self.owner.holder(),
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
        self.owner.holder()
    }

    /// Where loop events go (webhook endpoints, tick feeds).
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
    /// server, one task per feed.
    pub(crate) fn spawn<F, Fut>(&mut self, name: impl Into<String>, f: F)
    where
        F: FnOnce(StopRx) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.supervisor.spawn(name, f);
    }

    /// Stop (if not yet requested), drain loops and tasks until
    /// `shutdown_grace_secs`, then release the leases. The heartbeat says
    /// `stopping` during the drain and `stopped` after it.
    pub(crate) async fn shutdown(self) -> ShutdownReport {
        let Runtime {
            cfg,
            store,
            owner,
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
        let lease_released = owner.release().await;
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

    /// `sandbox` on the `[xmarket]` state dir `dir` (`ledger`) or on an
    /// install-wide one.
    fn plan(sandbox: &str, dir: &Path, ledger: bool) -> LeasePlan {
        LeasePlan {
            sandbox: sandbox.into(),
            state_dir: dir.to_path_buf(),
            ledger,
        }
    }

    async fn begin_as(plan: LeasePlan, timing: LeaseTiming) -> Result<Runtime> {
        Runtime::begin(plan, RuntimeConfig::default(), timing).await
    }

    async fn begin(dir: &Path, timing: LeaseTiming) -> Result<Runtime> {
        begin_as(plan("xmarket-weekend", dir, true), timing).await
    }

    /// `state:<dir name>` of the plan's state dir.
    fn state_resource(dir: &Path) -> String {
        state_lease_resource(&dir.file_name().unwrap().to_string_lossy())
    }

    /// Review #13: two sandboxes naming one `[xmarket] state` share a
    /// ledger — the second process is refused (exit 1), names the holder in
    /// full and leaves no lease behind; the first stops, the second runs.
    #[tokio::test]
    async fn two_sandboxes_on_one_xmarket_state_dir_never_run_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let first = begin_as(plan("xmarket", dir.path(), true), slow_timing())
            .await
            .unwrap();
        let holder = first.holder().to_string();
        let err = format!(
            "{:#}",
            begin_as(plan("xmarket-copy", dir.path(), true), slow_timing())
                .await
                .err()
                .unwrap()
        );
        let resource = state_resource(dir.path());
        assert!(err.contains("already has an owner"), "{err}");
        assert!(err.contains(&format!("`{resource}`")), "{err}");
        assert!(err.contains(&format!("`{holder}`")), "{err}");
        assert!(err.contains("[xmarket] state"), "{err}");
        // The refused process freed its `runtime:` lease.
        let store = SqliteRuntimeStore::open(dir.path()).unwrap();
        let r = lease_resource("xmarket-copy");
        let probe = store
            .acquire_lease(&r, "probe", 60_000, now_ms())
            .await
            .unwrap();
        assert!(probe.granted, "{probe:?}");
        store.release_lease(&r, "probe").await.unwrap();
        let report = first.shutdown().await;
        assert!(report.lease_released, "{report:?}");
        let second = begin_as(plan("xmarket-copy", dir.path(), true), slow_timing())
            .await
            .unwrap();
        second.shutdown().await;
    }

    /// `<TENGU_HOME>/state` (no `[xmarket]`) is install-wide: sandboxes
    /// without a ledger take no state lease and run side by side.
    #[tokio::test]
    async fn sandboxes_without_a_ledger_share_the_install_state_dir() {
        let dir = tempfile::tempdir().unwrap();
        let a = begin_as(plan("lping", dir.path(), false), slow_timing())
            .await
            .unwrap();
        let b = begin_as(plan("jev-exec", dir.path(), false), slow_timing())
            .await
            .unwrap();
        assert_eq!(a.owner.resources(), ["runtime:lping"]);
        a.shutdown().await;
        b.shutdown().await;
    }

    /// `tengu webhooks` holds the same leases: refused beside a running
    /// `tengu run` of the sandbox or of another sandbox on its ledger; once
    /// that stops, taken, kept and freed.
    #[tokio::test]
    async fn webhooks_leases_are_refused_beside_a_runner() {
        let dir = tempfile::tempdir().unwrap();
        let rt = begin(dir.path(), slow_timing()).await.unwrap();
        let same = plan("xmarket-weekend", dir.path(), true);
        let err = format!(
            "{:#}",
            OwnerLeases::take(&same, 60_000).await.err().unwrap()
        );
        assert!(
            err.contains("sandbox `xmarket-weekend` is already running"),
            "{err}"
        );
        let other = plan("xmarket", dir.path(), true);
        let err = format!(
            "{:#}",
            OwnerLeases::take(&other, 60_000).await.err().unwrap()
        );
        assert!(err.contains("already has an owner"), "{err}");
        rt.shutdown().await;
        let owner = OwnerLeases::take(&same, 60_000).await.unwrap();
        assert_eq!(
            owner.resources(),
            [
                "runtime:xmarket-weekend".to_string(),
                state_resource(dir.path())
            ]
        );
        let mut supervisor = Supervisor::new();
        owner.keep(&mut supervisor, slow_timing());
        let aborted = supervisor
            .shutdown(Instant::now() + Duration::from_secs(5))
            .await;
        assert!(aborted.is_empty(), "{aborted:?}");
        assert!(owner.release().await);
        let again = OwnerLeases::take(&other, 60_000).await.unwrap();
        again.release().await;
    }

    #[tokio::test]
    async fn losing_the_state_lease_stops_the_runtime_as_failed() {
        let dir = tempfile::tempdir().unwrap();
        let fast = LeaseTiming {
            ttl_ms: 60_000,
            renew_ms: 20,
        };
        let rt = begin(dir.path(), fast).await.unwrap();
        let thief = SqliteRuntimeStore::open(dir.path()).unwrap();
        let r = state_resource(dir.path());
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
            .expect("state lease keeper noticed");
        assert!(stop.failed);
        assert!(stop.reason.contains(&format!("`{r}`")), "{}", stop.reason);
        rt.shutdown().await;
    }

    #[test]
    fn lease_plan_follows_the_xmarket_state_dir() {
        let ws = tempfile::tempdir().unwrap();
        let config = feed_config(ws.path(), "");
        let p = LeasePlan::of(&config);
        assert_eq!((p.sandbox.as_str(), p.ledger), ("default", false));
        assert!(p.state_dir.ends_with("state"), "{p:?}");
        let mut config = feed_config(ws.path(), "[xmarket]\nstate = \"xm-plan\"\n");
        config.sandbox_name = Some("xmarket".into());
        let p = LeasePlan::of(&config);
        assert_eq!((p.sandbox.as_str(), p.ledger), ("xmarket", true));
        assert_eq!(
            Some(&p.state_dir),
            config.agents["main"].sandbox.xm_state_dir.as_ref()
        );
        assert!(p.state_dir.ends_with("state/xm-plan"), "{p:?}");
        assert_eq!(
            p.resources(),
            [
                ("runtime:xmarket".to_string(), LeaseKind::Runner),
                ("state:xm-plan".to_string(), LeaseKind::State)
            ]
        );
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

    /// Agent `main` in `workspace` + loop `xm_main` + `feeds`, loaded like
    /// `Config::load` does.
    fn feed_config(workspace: &Path, feeds: &str) -> Config {
        let text = format!(
            r#"
            [agents.main]
            default = true
            engine = "openrouter"
            model = "m"
            workspace = "{}"

            [decision_loops.xm_main]
            goal = "g"
            agent = "main"
            [decision_loops.xm_main.actions.hold]
            description = "Nothing to do"

            {feeds}
            "#,
            workspace.display()
        );
        let mut c: Config = toml::from_str(&text).unwrap();
        c.validate().unwrap();
        c.fold_default_scopes();
        c
    }

    #[tokio::test]
    async fn feeds_start_with_the_runtime_and_tick_their_loop() {
        use crate::domain::runtime::{FeedHealth, FeedState};
        let (dir, ws) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut rt = begin(dir.path(), slow_timing()).await.unwrap();
        let (slow, _) = launch_slow(&mut rt, 1);
        let config = feed_config(
            ws.path(),
            r#"
            [feeds.exit_tick]
            kind = "tick"
            target = "xm_main"
            every_secs = 3600
            run_on_start = true
            event = { phase = "exit" }
            "#,
        );
        let secrets = Arc::new(SecretRegistry::new());
        start_feeds(&config, &mut rt, &secrets, Arc::new(SystemClock)).unwrap();
        for _ in 0..400 {
            if !slow.seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let seen = slow.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1, "run_on_start ticked once: {seen:?}");
        assert!(seen[0].starts_with("exit_tick:"), "{seen:?}");
        // The feed/1 row lands in the loop agent's store.
        let store = open_observation_store(ws.path(), &config.agents["main"].sandbox).unwrap();
        let row = store
            .get("feed/1:exit_tick")
            .await
            .unwrap()
            .expect("feed row");
        let h: FeedHealth = row.typed().unwrap();
        assert_eq!((h.state, h.items, h.required), (FeedState::Live, 1, false));
        let report = rt.shutdown().await;
        assert!(
            report.aborted_tasks.is_empty() && !report.stop.failed,
            "{report:?}"
        );
    }

    // Multi-thread: the executor build `block_on`s plugin registration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tool_feed_calls_through_its_agents_executor() {
        use crate::domain::runtime::{FeedHealth, FeedState};
        let (dir, ws) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        std::fs::write(ws.path().join("notes.txt"), "hello").unwrap();
        let mut rt = begin(dir.path(), slow_timing()).await.unwrap();
        launch_slow(&mut rt, 1);
        let mut config = feed_config(
            ws.path(),
            r#"
            [feeds.notes]
            kind = "tool"
            agent = "main"
            tool = "read_file"
            each = { path = ["notes.txt", "missing.txt"] }
            every_secs = 3600
            run_on_start = true
            required = true
            "#,
        );
        config.agents.get_mut("main").unwrap().tools = vec!["read_file".into()];
        let secrets = Arc::new(SecretRegistry::new());
        start_feeds(&config, &mut rt, &secrets, Arc::new(SystemClock)).unwrap();
        let store = open_observation_store(ws.path(), &config.agents["main"].sandbox).unwrap();
        let mut row = None;
        for _ in 0..400 {
            row = store.get("feed/1:notes").await.unwrap();
            if row.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let h: FeedHealth = row.expect("feed row").typed().unwrap();
        // One file read, the missing one failed without a retry.
        assert_eq!((h.state, h.items, h.required), (FeedState::Live, 1, true));
        assert!(h.last_error_class.is_some(), "{h:?}");
        rt.shutdown().await;
    }

    // Multi-thread: the executor build `block_on`s plugin registration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tool_feed_whose_agent_cannot_run_the_tool_refuses_to_start() {
        let (dir, ws) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut rt = begin(dir.path(), slow_timing()).await.unwrap();
        launch_slow(&mut rt, 1);
        // No allow-list and not an opt-in name: validation passes, but no
        // plugin provides it (a typo).
        let config = feed_config(
            ws.path(),
            "[feeds.notes]\nkind = \"tool\"\nagent = \"main\"\ntool = \"read_fil\"\nevery_secs = 60\n",
        );
        let secrets = Arc::new(SecretRegistry::new());
        let err = start_feeds(&config, &mut rt, &secrets, Arc::new(SystemClock)).unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains("start [feeds.notes]") && err.contains("cannot run `read_fil`"),
            "{err}"
        );
        rt.shutdown().await;
    }

    #[tokio::test]
    async fn a_tick_feed_without_its_loop_refuses_to_start() {
        let (dir, ws) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let mut rt = begin(dir.path(), slow_timing()).await.unwrap();
        rt.launch(BTreeMap::new(), BTreeMap::new()); // xm_main not built
        let config = feed_config(
            ws.path(),
            "[feeds.exit_tick]\nkind = \"tick\"\ntarget = \"xm_main\"\nevery_secs = 60\n",
        );
        let secrets = Arc::new(SecretRegistry::new());
        let err = start_feeds(&config, &mut rt, &secrets, Arc::new(SystemClock)).unwrap_err();
        let err = format!("{err:#}");
        assert!(
            err.contains("start [feeds.exit_tick]")
                && err.contains("decision loop `xm_main` is not running"),
            "{err}"
        );
        rt.shutdown().await;
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
