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
//! | [`RunSession::wait_and_shutdown`] | waits for the stop, drains (≤ `[runtime] shutdown_grace_secs`), releases the leases, logs the totals |
//! | [`run_runtime`] | `tengu run`: signals + a session + its exit code |

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

/// Start the sandbox's runtime in this process (module table): the
/// leases (held elsewhere ⇒ `Err` carrying `bootstrap::runtime::LeaseHeld`),
/// the recording, every loop and feed, the heartbeat, the webhook routes
/// when built with `--features webhooks` and `[webhooks] enabled`.
pub(crate) async fn start_session(
    config: &Config,
    secrets: Arc<SecretRegistry>,
) -> Result<RunSession> {
    #[cfg(feature = "webhooks")]
    let webhook_memory = Arc::new(crate::application::memory::manager::MemoryManager::new());
    // The escalator records into the runtime's run, opened by `start`.
    #[cfg(feature = "webhooks")]
    let escalation_trace = super::webhooks::TraceSlot::default();
    #[cfg(feature = "webhooks")]
    let escalator = super::webhooks::orchestrator_escalator(
        config,
        &webhook_memory,
        Arc::clone(&escalation_trace),
    );
    #[cfg(not(feature = "webhooks"))]
    let escalator = None;

    let mut rt = runtime::start(config, Arc::clone(&secrets), escalator).await?;
    #[cfg(feature = "webhooks")]
    let _ = escalation_trace.set(rt.trace());
    #[cfg(feature = "webhooks")]
    let mounted = mount_webhooks(config, &mut rt, secrets, webhook_memory).await;
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
    memory_manager: Arc<crate::application::memory::manager::MemoryManager>,
) -> Result<()> {
    use super::webhooks;
    use anyhow::Context;

    if !config.webhooks.enabled {
        info!("webhooks off ([webhooks] enabled = false)");
        return Ok(());
    }
    // Each request's `trigger.webhook` root and its work go into the
    // runtime's run.
    let state = webhooks::app_state(
        config.clone(),
        memory_manager,
        rt.loops(),
        secrets,
        rt.trace(),
    )?;
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

/// SIGINT + SIGTERM, registered up front (unix); Ctrl-C elsewhere. Also
/// `tengu webhooks` (`webhooks.rs`).
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
