//! `tengu run --sandbox <s>` — the long-running process of a sandbox: one
//! runner per sandbox (lease), every decision loop built once, the webhook
//! routes when built with `--features webhooks` and `[webhooks] enabled`,
//! graceful shutdown on SIGINT / SIGTERM (a second signal exits at once,
//! code 130). Composition `bootstrap/runtime.rs`; operator doc
//! `docs/runtime-2026-09-30.md`. Logs go to `<TENGU_HOME>/logs/tengu.log` +
//! stderr (`cli/mod.rs`).

use std::sync::Arc;

use anyhow::Result;
use tracing::{error, info};

use crate::application::runtime::stopped;
use crate::bootstrap::runtime::{self, Runtime};
use crate::config::Config;
use crate::domain::secrets::SecretRegistry;

pub(crate) async fn run_runtime(config: Config, secrets: Arc<SecretRegistry>) -> Result<()> {
    // Before anything slow: a SIGTERM during boot must stop us cleanly.
    let mut signals = Signals::install()?;

    #[cfg(feature = "webhooks")]
    let webhook_memory = Arc::new(crate::application::memory::manager::MemoryManager::new());
    #[cfg(feature = "webhooks")]
    let escalator = super::webhooks::orchestrator_escalator(&config, &webhook_memory);
    #[cfg(not(feature = "webhooks"))]
    let escalator = None;

    let mut rt = runtime::start(&config, Arc::clone(&secrets), escalator).await?;
    #[cfg(feature = "webhooks")]
    let mounted = mount_webhooks(&config, &mut rt, secrets, webhook_memory).await;
    #[cfg(not(feature = "webhooks"))]
    let mounted = webhooks_off(&config, &mut rt);
    if let Err(e) = mounted {
        rt.shutdown().await;
        return Err(e);
    }
    info!(
        sandbox = %rt.sandbox(),
        holder = %rt.holder(),
        state_dir = %rt.state_dir().display(),
        loops = ?rt.loops().names(),
        "tengu run started"
    );

    let stopper = rt.stopper();
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

    let cause = stopped(&mut rt.stop_rx()).await;
    info!(reason = %cause.reason, failed = cause.failed, "tengu run stopping");
    let report = rt.shutdown().await;
    signal_task.abort();
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
    if report.stop.failed {
        anyhow::bail!("tengu run stopped: {}", report.stop.reason);
    }
    Ok(())
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
    let state = webhooks::app_state(config.clone(), memory_manager, rt.loops(), secrets)?;
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
struct Signals {
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
}

impl Signals {
    fn install() -> Result<Self> {
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

    async fn next(&mut self) -> &'static str {
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
