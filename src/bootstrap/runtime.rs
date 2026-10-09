//! `tengu run` composition (inbound side: `adapters/inbound/run.rs`; operator
//! doc `docs/runtime-2026-09-30.md`):
//!
//! | Step | What |
//! |---|---|
//! | state dir | `[xmarket]` state dir (`SandboxSections::xm_state_dir`), else the `[sources]` one (the SOE state root, critic C9), else `<TENGU_HOME>/state`; `runtime.db` lives there |
//! | leases ([`LeasePlan`], [`OwnerLeases`]) | `runtime:<sandbox>` (sandbox = `--sandbox`, else `default`), then — for an `[xmarket]` state dir, the ledger's — `state:<dir name>`: one owner per ledger, whichever sandbox names that `[xmarket] state`; then — with `[soe]` — `state:<[sources] state dir name>`: one owner per SOE state root (its cycles and logs); taken all or none, TTL 30 s, renewed every 10 s; held ⇒ this process refuses to start ([`LeaseHeld`] in the error: Studio attaches read-only); lost ⇒ it stops (failed). `tengu webhooks` takes the same leases (`inbound/webhooks.rs`) |
//! | trace | [`start`] opens this process's recording (`bootstrap::trace::open_sink`, `RunKind::Run`): `<TENGU_HOME>/logs/trace/<sandbox>/<run_id>.jsonl`, `runtime_id` = the lease holder; every loop's `decisions.jsonl` lines carry both ids ([`Runtime::trace`]) |
//! | loops | every `[decision_loops.*]` built once (`bootstrap::decision::build_decision_loop`) behind one `LoopDispatch` — the process owns loop state |
//! | health | `HealthBoard`: `run-<sandbox>.json` + `loop/1:<name>` rows (loop agent's store) every `[runtime] heartbeat_secs`; `stopping` / `stopped` beats on shutdown; feeds register via [`Runtime::health`] |
//! | live verdict | [`read_live`]: the heartbeat file + the `loop/1` / `feed/1` rows of each agent store that exists → `domain::runtime::live_verdict` — `tengu doctor --live` and Studio's `/api/v1/health` read the same |
//! | feeds | [`start_feeds`]: one task `feed:<name>` per `[feeds.<n>]` (`application::runtime::feeds::run_feed`, `SystemClock`, `jitter01`); a tool feed calls through its agent's executor (`decision::agent_tool_executor`, one per agent; a tool it cannot run fails the start) under `egress::AttributedExecutor` (egress records: the feed's agent, session `feed:<name>`, the call id), a tick feed submits to [`Runtime::loops`]; a job feed runs the named job ([`job_for`]: `soe_cycle` = `bootstrap::soe::soe_cycle_job`); `feed/1:<name>` rows go to the feed agent's store (tick: the target loop agent's; job `soe_cycle`: the `[soe] architect`'s) |
//! | tasks | [`Runtime::spawn`] registers long-running tasks on the stop signal: the webhook router and the feeds |
//! | shutdown | [`Runtime::shutdown`]: stop signal → loops drain + tasks stop ≤ `[runtime] shutdown_grace_secs` → leases released |

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::adapters::outbound::clock::SystemClock;
use crate::adapters::outbound::egress::{AttributedExecutor, CallSession};
use crate::adapters::outbound::noop::NoopTrace;
use crate::adapters::outbound::observations::{open_observation_store, SqliteObservationStore};
use crate::adapters::outbound::rate_limit::jitter01;
use crate::adapters::outbound::runtime_store::{read_heartbeat, SqliteRuntimeStore};
use crate::application::runtime::feeds::{run_feed, FeedEnv, FeedJob, FeedSpec, Rand01};
use crate::application::runtime::health::{heartbeat_task, write_beat, HealthBoard};
use crate::application::runtime::loops::{DrainReport, LoopDispatch, LoopHandler, LoopStats};
use crate::application::runtime::{keep_lease, LeaseTiming, Stop, StopRx, Stopper, Supervisor};
use crate::application::trace_exec::TracedExecutor;
use crate::bootstrap::trace::Recording;
use crate::config::feeds::{FeedConfig, FeedKind, JOBS, JOB_SOE_CYCLE};
use crate::config::runtime::RuntimeConfig;
use crate::config::Config;
use crate::domain::observation::{now_ms, Observation, Observed};
use crate::domain::runtime::{
    heartbeat_file, lease_resource, live_verdict, state_lease_resource, FeedHealth, HeartbeatRead,
    LiveKnobs, LiveReport, LoopHealth, RunState, RunnerLease,
};
use crate::domain::secrets::SecretRegistry;
use crate::domain::trace::{Component, EventDraft, Status};
use crate::domain::tz::Zone;
use crate::domain::workflow::node_id;
use crate::ports::clock::Clock;
use crate::ports::decision::Escalator;
use crate::ports::engine::ToolExecutor;
use crate::ports::observation::ObservationStore;
use crate::ports::runtime::{RuntimeJob, RuntimeStore};
use crate::ports::trace::TraceSink;

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

/// The `[sources]` state dir — the SOE state root (critic C8) — when the
/// sandbox has `[sources]`.
fn sources_state_dir(config: &Config) -> Option<PathBuf> {
    config
        .sources
        .as_ref()
        .map(|s| s.state_dir(&crate::config::paths::resolve_tengu_home()))
}

/// Where `runtime.db` lives: the `[xmarket]` state dir every agent sees
/// (`AgentConfig::sandbox`), else the `[sources]` one, else
/// `<TENGU_HOME>/state`.
pub(crate) fn runtime_state_dir(config: &Config) -> PathBuf {
    crate::config::runtime::state_dir(
        xm_state_dir(config).as_deref(),
        sources_state_dir(config).as_deref(),
        &crate::config::paths::resolve_tengu_home(),
    )
}

/// What `tengu doctor --live` and Studio's `/api/v1/health` judge: the
/// heartbeat file and the `loop/1` / `feed/1` rows, through
/// `domain::runtime::live_verdict` (no rule here).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LiveHealth {
    /// Runner name ([`runner_name`]).
    pub sandbox: String,
    /// Where `run-<sandbox>.json` was read.
    pub state_dir: PathBuf,
    pub heartbeat: HeartbeatRead,
    pub report: LiveReport,
}

/// The live verdict of `config`'s runtime, its heartbeat read from
/// `state_dir` ([`runtime_state_dir`] for this process's `TENGU_HOME`).
pub(crate) async fn read_live(config: &Config, state_dir: &Path) -> LiveHealth {
    let sandbox = runner_name(config);
    let heartbeat = match read_heartbeat(state_dir, &sandbox) {
        Ok(Some(hb)) => HeartbeatRead::Found(hb),
        Ok(None) => HeartbeatRead::Missing,
        Err(e) => HeartbeatRead::Unreadable(format!("{e:#}")),
    };
    let rows = health_rows(config, &heartbeat).await;
    let knobs = LiveKnobs {
        heartbeat_stale_secs: config.runtime.heartbeat_stale_secs,
    };
    let report = live_verdict(&sandbox, &heartbeat, &rows, now_ms(), knobs);
    LiveHealth {
        sandbox,
        state_dir: state_dir.to_path_buf(),
        heartbeat,
        report,
    }
}

/// `loop/1` rows of every configured loop and `feed/1` rows of every feed
/// the heartbeat lists, from each agent workspace whose observation store
/// exists (never created here). Unreadable stores are skipped.
async fn health_rows(config: &Config, heartbeat: &HeartbeatRead) -> Vec<Observation> {
    let mut keys: BTreeSet<String> = config
        .decision_loops
        .keys()
        .map(|n| Observation::key_for(LoopHealth::SCHEMA, n))
        .collect();
    if let HeartbeatRead::Found(hb) = heartbeat {
        keys.extend(
            hb.loops
                .keys()
                .map(|n| Observation::key_for(LoopHealth::SCHEMA, n)),
        );
        keys.extend(
            hb.feeds
                .keys()
                .map(|n| Observation::key_for(FeedHealth::SCHEMA, n)),
        );
    }
    let keys: Vec<String> = keys.into_iter().collect();
    let workspaces: BTreeSet<PathBuf> = config
        .agents
        .keys()
        .map(|a| agent_workspace(config, a))
        .filter(|ws| ws.join(".tengu").join("observations.db").exists())
        .collect();
    let mut rows = Vec::new();
    for ws in workspaces {
        let Ok(store) = SqliteObservationStore::open(&ws) else {
            continue;
        };
        if let Ok(found) = store.get_many(&keys).await {
            rows.extend(found.into_iter().flatten());
        }
    }
    rows
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
    /// With `[soe]`: the SOE state root (the `[sources]` state dir) — also
    /// `state:<dir name>`, so one process owns its cycles and logs.
    pub soe_state: Option<PathBuf>,
}

/// Which lease a refusal is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaseKind {
    Runner,
    State,
    SoeState,
}

/// `state:<dir name>` of `dir`.
fn dir_lease(dir: &Path) -> String {
    let name = dir.file_name().map_or_else(
        || dir.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    );
    state_lease_resource(&name)
}

impl LeasePlan {
    pub(crate) fn of(config: &Config) -> Self {
        let xm = xm_state_dir(config);
        Self {
            sandbox: runner_name(config),
            ledger: xm.is_some(),
            state_dir: runtime_state_dir(config),
            soe_state: config.soe.as_ref().and_then(|_| sources_state_dir(config)),
        }
    }

    /// The leases in the order they are taken (a dir leased once).
    fn resources(&self) -> Vec<(String, LeaseKind)> {
        let mut out = vec![(lease_resource(&self.sandbox), LeaseKind::Runner)];
        if self.ledger {
            out.push((dir_lease(&self.state_dir), LeaseKind::State));
        }
        if let Some(dir) = &self.soe_state {
            let r = dir_lease(dir);
            if out.iter().all(|(x, _)| *x != r) {
                out.push((r, LeaseKind::SoeState));
            }
        }
        out
    }

    /// Why `lease` (refused) stops this process from starting — the holder
    /// in full; a [`LeaseHeld`] inside the `anyhow::Error`.
    fn refusal(&self, kind: LeaseKind, db: &str, lease: &RunnerLease, now: i64) -> anyhow::Error {
        let (resource, holder, secs) = (
            &lease.resource,
            &lease.current_holder,
            lease.remaining_secs(now),
        );
        let message = match kind {
            LeaseKind::Runner => format!(
                "sandbox `{}` is already running: lease `{resource}` in {db} is held by \
                 `{holder}` for {secs} s more. Stop that `tengu run` / `tengu webhooks` first \
                 (SIGTERM drains it); if it crashed, retry once the lease expires.",
                self.sandbox
            ),
            LeaseKind::State => format!(
                "state dir {} already has an owner: lease `{resource}` in {db} is held by \
                 `{holder}` for {secs} s more — a `tengu run` or `tengu webhooks` of a sandbox \
                 naming the same [xmarket] state owns its ledger, and a ledger has one owner. \
                 Stop that process first (SIGTERM drains it), or give this sandbox its own \
                 [xmarket] state; if it crashed, retry once the lease expires.",
                self.state_dir.display()
            ),
            LeaseKind::SoeState => format!(
                "SOE state root {} already has an owner: lease `{resource}` in {db} is held by \
                 `{holder}` for {secs} s more — a `tengu run` or `tengu webhooks` of a sandbox \
                 with [soe] naming the same [sources] state owns its cycles and logs. Stop that \
                 process first (SIGTERM drains it), or give this sandbox its own [sources] \
                 state; if it crashed, retry once the lease expires.",
                self.soe_state
                    .as_deref()
                    .unwrap_or(&self.state_dir)
                    .display()
            ),
        };
        anyhow::Error::new(LeaseHeld {
            resource: resource.clone(),
            holder: holder.clone(),
            remaining_secs: secs,
            message,
        })
    }
}

/// A lease another process holds — why [`OwnerLeases::take`] (and so
/// [`start`]) refused. Carried inside the `anyhow::Error` ([`LeaseHeld::of`]):
/// Studio's Play tells "another process runs this sandbox — attach
/// read-only" from a start that failed. `Display` = the operator message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LeaseHeld {
    pub resource: String,
    /// The holder `<host>:<pid>:<uuid>`, in full.
    pub holder: String,
    /// Whole seconds until it expires (a crashed holder's lease frees then).
    pub remaining_secs: u64,
    message: String,
}

impl LeaseHeld {
    /// The refusal inside `e`, if `e` is one (anywhere in its chain).
    #[cfg_attr(not(feature = "studio"), allow(dead_code))]
    pub(crate) fn of(e: &anyhow::Error) -> Option<&LeaseHeld> {
        e.chain().find_map(|c| c.downcast_ref::<LeaseHeld>())
    }
}

impl std::fmt::Display for LeaseHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LeaseHeld {}

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
                LeaseKind::SoeState => "soe state lease",
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
    /// This process's trace recording (`bootstrap::trace::open_sink`);
    /// `NoopTrace` until [`start`] opens it.
    trace: Arc<dyn TraceSink>,
}

/// Take the lease, open the trace recording (`run_id`; `runtime_id` = the
/// lease holder), build every decision loop (their audit lines carry both
/// ids), start the lease keeper and the heartbeat. `escalator` comes from
/// the inbound side (the webhook listener's orchestrator escalator when
/// built with `--features webhooks`).
pub(crate) async fn start(
    config: &Config,
    secrets: Arc<SecretRegistry>,
    escalator: Option<Arc<dyn Escalator>>,
) -> Result<Runtime> {
    let rt = Runtime::begin(
        LeasePlan::of(config),
        config.runtime.clone(),
        LeaseTiming::default(),
    )
    .await?;
    let trace = crate::bootstrap::trace::open_sink(
        config,
        Some(rt.holder()),
        crate::domain::trace::RunKind::Run,
        &secrets,
    );
    rt.start_recorded(config, secrets, escalator, trace).await
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
            trace: Some(rt.trace()),
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
            // `tool.*` events per call; egress records name the feed's agent +
            // `feed:<name>` + the call id.
            let executor: Arc<dyn ToolExecutor> =
                Arc::new(TracedExecutor::new(executor, rt.trace(), &agent_name, None));
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
        FeedKind::Job => {
            let (job, agent) = job_for(
                config,
                feed.job.as_deref().unwrap_or_default(),
                schedule.zone,
            )?;
            (FeedJob::Job { job }, agent)
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

/// The named job of a `kind = "job"` feed (`config/feeds.rs` `JOBS`, a
/// closed list) + the agent whose store takes its `feed/1` row.
fn job_for(config: &Config, job: &str, zone: Zone) -> Result<(Arc<dyn RuntimeJob>, String)> {
    match job {
        JOB_SOE_CYCLE => {
            let soe = config
                .soe
                .as_ref()
                .ok_or_else(|| anyhow!("job: `{JOB_SOE_CYCLE}` needs a [soe] section"))?;
            let job = crate::bootstrap::soe::soe_cycle_job(config, zone)
                .with_context(|| format!("job `{JOB_SOE_CYCLE}`"))?;
            Ok((job, soe.architect.clone()))
        }
        other => anyhow::bail!("job: `{other}` is not a known job ({})", JOBS.join(", ")),
    }
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
    rec: Recording,
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
            rec.clone(),
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

/// Where `runtime.*` events go: node `runtime:<sandbox>`, artifact = the
/// heartbeat file (`run-<sandbox>.json`).
struct Lifecycle {
    trace: Arc<dyn TraceSink>,
    node: String,
    heartbeat: String,
    /// Parent of the next event (`runtime.stopped` → `runtime.stopping`).
    parent: Option<String>,
}

impl Lifecycle {
    fn emit(
        &self,
        kind: &str,
        status: Status,
        payload: Value,
        duration_ms: Option<u64>,
    ) -> Option<String> {
        let mut d = EventDraft::new(Component::Runtime, kind, status)
            .node(self.node.clone())
            .payload(payload)
            .artifact(self.heartbeat.clone(), None);
        d.duration_ms = duration_ms;
        d.parent_event_id = self.parent.clone();
        self.trace.emit(d)
    }
}

/// Per-loop counters as a trace payload carries them.
fn loop_stats_json(stats: &BTreeMap<String, LoopStats>) -> Value {
    Value::Object(
        stats
            .iter()
            .map(|(name, s)| (name.clone(), s.to_json()))
            .collect(),
    )
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
            cfg.max_queued_per_loop,
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
            trace: Arc::new(NoopTrace),
        })
    }

    /// [`start`] after the leases: record into `trace` (`runtime.starting`),
    /// build the loops, launch them + the heartbeat, start the feeds
    /// (`runtime.running`); a failure is `runtime.start_failed`, then the
    /// shutdown (`runtime.stopping` → `runtime.stopped`) and the error.
    /// Studio's control tests start a runtime on a temp state dir this way.
    pub(crate) async fn start_recorded(
        mut self,
        config: &Config,
        secrets: Arc<SecretRegistry>,
        escalator: Option<Arc<dyn Escalator>>,
        trace: Arc<dyn TraceSink>,
    ) -> Result<Self> {
        self.trace = trace;
        self.lifecycle(
            "runtime.starting",
            Status::Pending,
            json!({
                "holder": self.holder(),
                "leases": self.owner.resources(),
                "state_dir": self.state_dir.display().to_string(),
                "pid": std::process::id(),
            }),
        );
        let rec = Recording::of(self.trace(), Some(self.holder()));
        let started =
            build_loops(config, Arc::clone(&secrets), escalator, rec).and_then(|(h, s)| {
                self.launch(h, s);
                start_feeds(config, &mut self, &secrets, Arc::new(SystemClock))
            });
        match started {
            Ok(()) => {
                self.lifecycle(
                    "runtime.running",
                    Status::Running,
                    json!({
                        "loops": self.loops.names(),
                        "feeds": config.feeds.keys().collect::<Vec<_>>(),
                    }),
                );
                Ok(self)
            }
            Err(e) => {
                self.lifecycle(
                    "runtime.start_failed",
                    Status::Failed,
                    json!({"error": format!("{e:#}")}),
                );
                self.shutdown().await;
                Err(e)
            }
        }
    }

    /// This process's trace recording (`NoopTrace` before [`start`] opens
    /// one, or when the trace dir cannot be written).
    pub(crate) fn trace(&self) -> Arc<dyn TraceSink> {
        Arc::clone(&self.trace)
    }

    fn life(&self) -> Lifecycle {
        Lifecycle {
            trace: Arc::clone(&self.trace),
            node: node_id::runtime(&self.sandbox),
            heartbeat: self
                .state_dir
                .join(heartbeat_file(&self.sandbox))
                .display()
                .to_string(),
            parent: None,
        }
    }

    /// A `runtime.*` event (module table).
    fn lifecycle(&self, kind: &str, status: Status, payload: Value) {
        self.life().emit(kind, status, payload, None);
    }

    /// Install the loop handlers (+ each loop agent's observation store for
    /// its `loop/1` row) and start serving them and the heartbeat — once,
    /// before anything submits events or registers feeds.
    pub(crate) fn launch(
        &mut self,
        handlers: BTreeMap<String, Arc<dyn LoopHandler>>,
        loop_stores: BTreeMap<String, Arc<dyn ObservationStore>>,
    ) {
        self.loops = Arc::new(
            LoopDispatch::new(
                handlers,
                self.cfg.max_decisions_in_flight,
                self.cfg.max_queued_per_loop,
            )
            .with_trace(Arc::clone(&self.trace)),
        );
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
        let life = self.life();
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
        let t0 = Instant::now();
        let stopping = life.emit(
            "runtime.stopping",
            Status::Pending,
            json!({
                "reason": stop.reason,
                "failed": stop.failed,
                "grace_secs": cfg.shutdown_grace_secs,
                "stats": loop_stats_json(&loops.stats()),
            }),
            None,
        );
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
        let status = if stop.failed {
            Status::Failed
        } else {
            Status::Ok
        };
        let stopped = json!({
            "reason": stop.reason,
            "failed": stop.failed,
            "drain": {
                "finished": drained.finished,
                "dropped": drained.dropped,
                "aborted": drained.aborted,
            },
            "aborted_tasks": aborted_tasks,
            "lease_released": lease_released,
            "stats": loop_stats_json(&loops.stats()),
        });
        let elapsed = Some(t0.elapsed().as_millis() as u64);
        let mut life = life;
        life.parent = stopping;
        life.emit("runtime.stopped", status, stopped, elapsed);
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
            soe_state: None,
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

    /// With `[soe]` the `[sources]` state dir — the SOE state root — holds
    /// `runtime.db`, and one process owns it: `state:<dir name>` beside
    /// `runtime:<sandbox>`. A second sandbox naming the same `[sources]
    /// state` is refused, names the holder in full and keeps nothing; once
    /// the first stops it runs.
    #[tokio::test]
    async fn soe_state_dir_holds_runtime_db_and_lease() {
        let mut config: Config = toml::from_str(
            r#"
            [agents.soe_architect]
            engine = "openrouter"
            model = "m"
            description = "d"
            tools = ["soe_propose"]

            [sources]
            state = "soe-plan"

            [soe]
            architect = "soe_architect"
            critic = "soe_architect"
            max_proposals = 1
            forecast_max_weeks = 1
            "#,
        )
        .unwrap();
        config.fold_default_scopes();
        config.sandbox_name = Some("soe".into());
        let p = LeasePlan::of(&config);
        assert!(p.state_dir.ends_with("state/soe-plan"), "{p:?}");
        assert_eq!(
            (p.ledger, p.soe_state.as_ref()),
            (false, Some(&p.state_dir))
        );
        assert_eq!(
            p.resources(),
            [
                ("runtime:soe".to_string(), LeaseKind::Runner),
                ("state:soe-plan".to_string(), LeaseKind::SoeState)
            ]
        );
        // `[sources]` without `[soe]`: runtime.db there, no state lease.
        let mut plain = config.clone();
        plain.soe = None;
        let q = LeasePlan::of(&plain);
        assert_eq!((&q.state_dir, q.soe_state.as_ref()), (&p.state_dir, None));
        assert_eq!(q.resources().len(), 1);

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("soe-plan");
        let on = |sandbox: &str| LeasePlan {
            sandbox: sandbox.into(),
            state_dir: root.clone(),
            ledger: false,
            soe_state: Some(root.clone()),
        };
        let first = begin_as(on("soe"), slow_timing()).await.unwrap();
        assert!(root.join("runtime.db").is_file());
        assert_eq!(first.owner.resources(), ["runtime:soe", "state:soe-plan"]);
        let err = format!(
            "{:#}",
            begin_as(on("soe-copy"), slow_timing()).await.err().unwrap()
        );
        assert!(err.starts_with("SOE state root "), "{err}");
        assert!(err.contains("`state:soe-plan`"), "{err}");
        assert!(err.contains(&format!("`{}`", first.holder())), "{err}");
        let store = SqliteRuntimeStore::open(&root).unwrap();
        let r = lease_resource("soe-copy");
        let probe = store
            .acquire_lease(&r, "probe", 60_000, now_ms())
            .await
            .unwrap();
        assert!(probe.granted, "the refused process freed its runner lease");
        store.release_lease(&r, "probe").await.unwrap();
        assert!(first.shutdown().await.lease_released);
        let second = begin_as(on("soe-copy"), slow_timing()).await.unwrap();
        second.shutdown().await;
    }

    /// A `kind = "job"` feed starts through `start_recorded` — the start
    /// `tengu run` and Studio's Play share (`inbound::run::start_session`):
    /// with no signed profile the `soe_cycle` run fails before it creates
    /// anything, traced `feed.fired` (`kind = "job"`) → its outcome, a child
    /// of it, correlated `feed:<name>:<slot ms>`.
    #[tokio::test]
    async fn job_feed_starts_and_traces_through_the_shared_start() {
        use crate::application::trace_exec::tests::MemTrace;
        let (dir, ws) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let state = format!("soe-runtime-test-{}", uuid::Uuid::new_v4());
        let agent = |name: &str, tool: &str| {
            format!(
                "[agents.{name}]\nengine = \"openrouter\"\nmodel = \"m\"\ndescription = \"d\"\n\
                 tools = [\"{tool}\"]\nworkspace = \"{}\"\n",
                ws.path().display()
            )
        };
        let text = format!(
            "{}{}[sources]\nstate = \"{state}\"\n\
             [soe]\narchitect = \"soe_architect\"\ncritic = \"soe_critic\"\nmax_proposals = 1\nforecast_max_weeks = 1\n\
             [feeds.soe_week]\nkind = \"job\"\njob = \"soe_cycle\"\ntz = \"Europe/Paris\"\nat = [\"Mon 07:00\"]\nrun_on_start = true\n",
            agent("soe_architect", "soe_propose"),
            agent("soe_critic", "soe_challenge"),
        );
        let file = dir.path().join("config.toml");
        std::fs::write(&file, &text).unwrap();
        let mut config: Config = toml::from_str(&text).unwrap();
        config.loaded_from = Some(file);
        config.fold_default_scopes();
        let sink = Arc::new(MemTrace::default());
        let rt = begin_as(plan("soe", &dir.path().join("rt"), false), slow_timing())
            .await
            .unwrap()
            .start_recorded(&config, Arc::new(SecretRegistry::new()), None, sink.clone())
            .await
            .unwrap();
        let feed_events = || {
            sink.all()
                .into_iter()
                .filter(|d| d.kind.starts_with("feed."))
                .collect::<Vec<_>>()
        };
        for _ in 0..400 {
            if feed_events().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        rt.shutdown().await;
        let d = feed_events();
        assert!(d.len() >= 2, "{:?}", sink.kinds());
        assert_eq!(d[0].kind, "feed.fired");
        assert_eq!(d[0].payload["kind"], json!("job"));
        let slot = d[0].payload["slot_ms"].as_i64().unwrap();
        assert_eq!(d[0].correlation_id, Some(format!("feed:soe_week:{slot}")));
        assert!(
            ["feed.failed", "feed.retrying"].contains(&d[1].kind.as_str()),
            "{:?}",
            sink.kinds()
        );
        assert!(
            d[1].payload["error"]
                .as_str()
                .is_some_and(|e| e.starts_with("operator_profile_missing")),
            "{:?}",
            d[1].payload
        );
        let root = crate::config::paths::resolve_tengu_home()
            .join("state")
            .join(&state);
        assert!(!root.exists(), "nothing created in {}", root.display());
    }

    #[tokio::test]
    async fn a_second_instance_is_refused_until_the_first_stops() {
        let dir = tempfile::tempdir().unwrap();
        let first = begin(dir.path(), slow_timing()).await.unwrap();
        let holder = first.holder().to_string();
        assert!(holder.contains(&format!(":{}:", std::process::id())));
        let refused = begin(dir.path(), slow_timing()).await.err().unwrap();
        // Typed: Studio's Play tells a held lease from any other failure.
        let held = LeaseHeld::of(&refused).expect("a LeaseHeld refusal");
        assert_eq!(
            (held.resource.as_str(), held.holder.as_str()),
            ("runtime:xmarket-weekend", holder.as_str())
        );
        assert!(held.remaining_secs > 0 && held.remaining_secs <= 60);
        let err = format!("{refused:#}");
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

    /// Agent `main` in `workspace` + one tool feed of `tool` (no loops: no
    /// Jev key needed).
    fn tool_feed_config(workspace: &Path, tool: &str) -> Config {
        let text = format!(
            "[agents.main]\ndefault = true\nengine = \"openrouter\"\nmodel = \"m\"\n\
             workspace = \"{}\"\n\n[feeds.probe]\nkind = \"tool\"\nagent = \"main\"\n\
             tool = \"{tool}\"\nargs = {{ path = \".\" }}\nevery_secs = 3600\n\
             run_on_start = true\n",
            workspace.display()
        );
        let mut c: Config = toml::from_str(&text).unwrap();
        c.validate().unwrap();
        c.fold_default_scopes();
        c
    }

    /// `runtime.starting` → `runtime.running` (the feed's events in between,
    /// its tool call traced through the agent's executor) → on shutdown
    /// `runtime.stopping` → `runtime.stopped` (its child, timed, drain +
    /// lease); a start that fails is `runtime.start_failed`, then the same
    /// shutdown pair. Node `runtime:<sandbox>`, artifact = the heartbeat.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_events_on_start_and_shutdown() {
        use crate::application::trace_exec::tests::MemTrace;
        let (dir, ws) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let sink = Arc::new(MemTrace::default());
        let rt = begin_as(plan("control-loop-lab", dir.path(), false), slow_timing())
            .await
            .unwrap();
        let config = tool_feed_config(ws.path(), "list_directory");
        let secrets = Arc::new(SecretRegistry::new());
        let rt = rt
            .start_recorded(&config, secrets, None, sink.clone())
            .await
            .unwrap();
        for _ in 0..400 {
            if sink.kinds().iter().any(|k| k == "feed.completed") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let report = rt.shutdown().await;
        assert!(report.lease_released);
        let d = sink.all();
        let kinds = sink.kinds();
        let at = |k: &str| kinds.iter().position(|x| x == k).unwrap();
        assert_eq!(kinds[0], "runtime.starting");
        assert!(at("runtime.running") > 0);
        assert_eq!(
            kinds[kinds.len() - 2..],
            ["runtime.stopping", "runtime.stopped"]
        );
        let (fired, started, done) = (at("feed.fired"), at("tool.started"), at("tool.completed"));
        assert!(fired < started && started < done && done < at("feed.completed"));
        assert_eq!(d[started].parent_event_id, Some(MemTrace::id(fired)));
        assert_eq!(
            d[started].node_id.as_deref(),
            Some("tool:main/list_directory")
        );
        let stopped = d.last().unwrap();
        assert_eq!(stopped.status, Status::Ok);
        assert_eq!(stopped.parent_event_id, Some(MemTrace::id(d.len() - 2)));
        assert!(stopped.duration_ms.is_some());
        assert_eq!(stopped.payload["lease_released"], json!(true));
        assert_eq!(stopped.payload["drain"]["aborted"], json!(0));
        assert_eq!(d[0].node_id.as_deref(), Some("runtime:control-loop-lab"));
        assert_eq!(d[0].payload["leases"], json!(["runtime:control-loop-lab"]));
        let hb = dir.path().join("run-control-loop-lab.json");
        assert_eq!(
            d[0].artifact.as_ref().unwrap().file,
            hb.display().to_string()
        );

        // A start that fails: start_failed, then the shutdown pair.
        let sink = Arc::new(MemTrace::default());
        let rt = begin_as(plan("control-loop-lab", dir.path(), false), slow_timing())
            .await
            .unwrap();
        let config = tool_feed_config(ws.path(), "read_fil");
        let Err(err) = rt
            .start_recorded(&config, Arc::new(SecretRegistry::new()), None, sink.clone())
            .await
        else {
            panic!("a feed whose agent cannot run its tool must refuse to start");
        };
        assert!(format!("{err:#}").contains("cannot run `read_fil`"));
        assert_eq!(
            sink.kinds(),
            [
                "runtime.starting",
                "runtime.start_failed",
                "runtime.stopping",
                "runtime.stopped"
            ]
        );
        let failed = &sink.all()[1];
        assert_eq!(failed.status, Status::Failed);
        assert!(failed.payload["error"]
            .as_str()
            .unwrap()
            .contains("cannot run `read_fil`"));
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
