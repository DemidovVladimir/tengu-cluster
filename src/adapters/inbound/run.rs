//! `tengu run --sandbox <s>` — the long-running process of a sandbox: one
//! runner per sandbox (lease), every decision loop built once, every
//! `[feeds.*]` scheduled, the webhook routes when built with
//! `--features webhooks` and `[webhooks] enabled`,
//! graceful shutdown on SIGINT / SIGTERM (a second signal exits at once,
//! code 130). Composition `bootstrap/runtime.rs`; operator doc
//! `docs/runtime-2026-09-30.md`. Logs go to `<TENGU_HOME>/logs/tengu.log` +
//! stderr (`cli/mod.rs`).
//!
//! | Piece | Who uses it |
//! |---|---|
//! | [`start_session`] → [`RunSession`] | the runtime started: leases, recording, loops, feeds, heartbeat, the webhook routes — `tengu run` and Studio's Play (`studio/control.rs`) start it the same way, so there is one runtime, never a mini copy |
//! | [`RunSession::stopper`] | requests the graceful stop: the first SIGINT / SIGTERM here, Studio's Stop (`"studio stop"`) — the same `Stopper::stop` |
//! | [`RunSession::wait_and_shutdown`] | waits for the stop, drains loop events and orchestrator turns — the webhook agent endpoints' and escalations', owned by `Runtime::turns` (≤ `[runtime] shutdown_grace_secs`, then aborted), releases the leases, writes `runtime.stopped`, logs the totals |
//! | [`run_runtime`] | `tengu run`: signals + a session + its exit code |
//! | [`run_webhooks`] | `tengu webhooks`: [`run_runtime`] on the sandbox with no feeds and only the loops an endpoint names ([`webhooks_only`]) — the same leases, recording (`kind = "run"`), heartbeat, drain and exit codes |

use std::sync::Arc;

use anyhow::Result;
use tracing::{error, info};

use crate::application::runtime::loops::LoopDispatch;
use crate::application::runtime::{stopped, Stopper};
use crate::bootstrap::runtime::{self, Runtime, ShutdownReport};
use crate::config::Config;
use crate::domain::secrets::SecretRegistry;

pub(crate) async fn run_runtime(config: Config, secrets: Arc<SecretRegistry>) -> Result<()> {
    // Before anything slow: a SIGTERM during boot must stop us cleanly.
    let mut signals = Signals::install()?;
    let session = start_session(&config, secrets).await?;

    let stopper = session.stopper();
    let signal_task = tokio::spawn(async move {
        let first = signals.next().await;
        stopper.stop(first, false);
        let second = signals.next().await;
        error!(
            signal = second,
            "second signal while draining — exiting now (code 130)"
        );
        std::process::exit(130);
    });

    let report = session.wait_and_shutdown().await;
    signal_task.abort();
    if report.stop.failed {
        anyhow::bail!("tengu run stopped: {}", report.stop.reason);
    }
    Ok(())
}

/// `tengu webhooks --sandbox <s>` (module table): the listener's config
/// checks (`[webhooks] enabled`, endpoints, `[orchestrator]` for a planner
/// endpoint), then the runtime with nothing but the webhook routes and
/// their loops.
#[cfg(feature = "webhooks")]
pub(crate) async fn run_webhooks(config: Config, secrets: Arc<SecretRegistry>) -> Result<()> {
    super::webhooks::check_config(&config)?;
    run_runtime(webhooks_only(config), secrets).await
}

/// `config` without its feeds and without the loops no
/// `[webhooks.endpoints.*] loop` names.
#[cfg_attr(not(feature = "webhooks"), allow(dead_code))]
fn webhooks_only(mut config: Config) -> Config {
    config.feeds.clear();
    let named: std::collections::BTreeSet<String> = config
        .webhooks
        .endpoints
        .values()
        .filter_map(|ep| ep.decision_loop.clone())
        .collect();
    config.decision_loops.retain(|name, _| named.contains(name));
    config
}

/// Start the sandbox's runtime in this process (module table): the
/// leases (held elsewhere ⇒ `Err` carrying `bootstrap::runtime::LeaseHeld`),
/// the recording, every loop and feed, the heartbeat, the webhook routes
/// when built with `--features webhooks` and `[webhooks] enabled`.
pub(crate) async fn start_session(
    config: &Config,
    secrets: Arc<SecretRegistry>,
) -> Result<RunSession> {
    // The escalator records into the runtime's run and its turns are the
    // runtime's (drained on shutdown); `start` hands both before any loop runs.
    let escalator: runtime::MakeEscalator<'_> = Box::new(|trace, turns| {
        crate::bootstrap::one_shot::orchestrator_escalator(config, trace, turns)
    });

    let mut rt = runtime::start(config, Arc::clone(&secrets), escalator).await?;
    #[cfg(feature = "webhooks")]
    let mounted = mount_webhooks(config, &mut rt, secrets).await;
    #[cfg(not(feature = "webhooks"))]
    let mounted = webhooks_off(config, &mut rt);
    if let Err(e) = mounted {
        rt.shutdown().await;
        return Err(e);
    }
    info!(
        sandbox = %rt.sandbox(),
        holder = %rt.holder(),
        run_id = rt.trace().run_id().unwrap_or("none"),
        state_dir = %rt.state_dir().display(),
        loops = ?rt.loops().names(),
        feeds = ?config.feeds.keys().collect::<Vec<_>>(),
        "tengu run started"
    );
    Ok(RunSession { rt })
}

/// A started runtime (module table). Dropping it without
/// [`Self::wait_and_shutdown`] leaves its tasks running until the process
/// ends — always wait.
pub(crate) struct RunSession {
    rt: Runtime,
}

#[cfg_attr(not(feature = "studio"), allow(dead_code))]
impl RunSession {
    /// A runtime started some other way (Studio's control tests: a temp
    /// state dir, fake loops).
    #[cfg(test)]
    pub(crate) fn from_runtime(rt: Runtime) -> Self {
        Self { rt }
    }

    /// Requests the graceful stop (first request wins).
    pub(crate) fn stopper(&self) -> Stopper {
        self.rt.stopper()
    }

    /// The lease holder `<host>:<pid>:<uuid>` = the recording's `runtime_id`.
    pub(crate) fn holder(&self) -> &str {
        self.rt.holder()
    }

    /// The runtime recording's `run_id` (`None`: it records nothing).
    pub(crate) fn run_id(&self) -> Option<String> {
        self.rt.trace().run_id().map(str::to_string)
    }

    /// Where loop events go (Studio's send-event).
    pub(crate) fn loops(&self) -> Arc<LoopDispatch> {
        self.rt.loops()
    }

    /// Wait for the stop (a signal, Studio's Stop, a lost lease, a task
    /// that died), drain, release the leases; log the totals.
    pub(crate) async fn wait_and_shutdown(self) -> ShutdownReport {
        let rt = self.rt;
        let cause = stopped(&mut rt.stop_rx()).await;
        info!(reason = %cause.reason, failed = cause.failed, "tengu run stopping");
        let report = rt.shutdown().await;
        for (name, s) in &report.loop_stats {
            info!(
                decision_loop = %name,
                accepted = s.accepted,
                completed = s.completed,
                failed = s.failed,
                dropped = s.dropped,
                "loop totals"
            );
        }
        info!(
            finished = report.loops.finished,
            dropped = report.loops.dropped,
            aborted = report.loops.aborted,
            turns_finished = report.loops.turns_finished,
            turns_aborted = report.loops.turns_aborted,
            aborted_tasks = ?report.aborted_tasks,
            lease_released = report.lease_released,
            "tengu run stopped"
        );
        report
    }
}

/// Serve `/webhooks/:name` in this process on the runtime's loops; stops
/// accepting on the stop signal (`with_graceful_shutdown`).
#[cfg(feature = "webhooks")]
async fn mount_webhooks(
    config: &Config,
    rt: &mut Runtime,
    secrets: Arc<SecretRegistry>,
) -> Result<()> {
    use super::webhooks;
    use anyhow::Context;

    if !config.webhooks.enabled {
        info!("webhooks off ([webhooks] enabled = false)");
        return Ok(());
    }
    // Each request's `trigger.webhook` root and its work go into the
    // runtime's run.
    let state = webhooks::app_state(config.clone(), rt.loops(), secrets, rt.trace())?;
    let addr = webhooks::bind_addr(config)?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind webhook listener to {addr}"))?;
    let app = webhooks::router(state);
    let stopper = rt.stopper();
    rt.spawn("webhooks", move |mut stop| async move {
        let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
            stopped(&mut stop).await;
        });
        if let Err(e) = serve.await {
            stopper.stop(format!("webhook server failed: {e}"), true);
        }
    });
    let endpoints: Vec<&String> = config.webhooks.endpoints.keys().collect();
    info!(%addr, ?endpoints, "webhooks mounted");
    Ok(())
}

#[cfg(not(feature = "webhooks"))]
fn webhooks_off(config: &Config, _rt: &mut Runtime) -> Result<()> {
    if config.webhooks.enabled {
        tracing::warn!(
            "[webhooks] enabled, but this binary was built without --features webhooks — webhooks off"
        );
    } else {
        info!("webhooks off (built without --features webhooks)");
    }
    Ok(())
}

/// SIGINT + SIGTERM, registered up front (unix); Ctrl-C elsewhere.
pub(super) struct Signals {
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
}

impl Signals {
    pub(super) fn install() -> Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            Ok(Self {
                int: signal(SignalKind::interrupt())?,
                term: signal(SignalKind::terminate())?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {})
        }
    }

    pub(super) async fn next(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.int.recv() => "SIGINT",
                _ = self.term.recv() => "SIGTERM",
            }
        }
        #[cfg(not(unix))]
        {
            if tokio::signal::ctrl_c().await.is_err() {
                tracing::warn!("ctrl-c handler unavailable");
                std::future::pending::<()>().await;
            }
            "ctrl-c"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `tengu webhooks` keeps the routes' loops and nothing else that runs
    /// on its own: no feed, no loop an endpoint does not name.
    #[test]
    fn webhooks_only_keeps_the_endpoint_loops_and_no_feeds() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("sandboxes/control-loop-lab/config.toml");
        let mut config = Config::load(&path).unwrap();
        assert!(!config.feeds.is_empty() && config.decision_loops.contains_key("demo"));
        let mut other = config.decision_loops["demo"].clone();
        other.model = "x/other".into();
        config.decision_loops.insert("other".into(), other);
        config.webhooks.endpoints.insert(
            "e".into(),
            toml::from_str("loop = \"demo\"\nsecret = \"k\"").unwrap(),
        );
        let scoped = webhooks_only(config);
        assert!(scoped.feeds.is_empty());
        let loops: Vec<&String> = scoped.decision_loops.keys().collect();
        assert_eq!(loops, ["demo"]);
    }
}
