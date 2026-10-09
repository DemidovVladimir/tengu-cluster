//! `tengu studio --sandbox <s> [--port <n>] [--bind <ip>] [--allow-control]`
//! — the local Tengu Studio server (`TENGU_STUDIO_PLAN.md` § 4–6, ST-20 /
//! ST-30 / ST-31; `--features studio`). Serves the page in `web/studio/`
//! and a JSON / SSE API over what Rust already computed — the validated
//! graph, the trace, the `tengu doctor --live` verdict, the control rules.
//! The browser draws; it decides nothing. Composition:
//! `bootstrap::studio::StudioContext`; delivery rules:
//! `application::studio::stream`; control: `control.rs` (rules
//! `application::studio::control`).
//!
//! | Route | Serves |
//! |---|---|
//! | GET `/` · `/assets/<file>` | `web/studio/` (`index.html`, `studio.css`, `studio.js`), embedded (`assets.rs`) |
//! | GET `/api/v1/meta` | sandbox, `config_hash`, schema versions, `read_only` / `control_enabled`, limits, evidence paths, `tones` (status → tone → meaning: `Status::tone`, the page's legend) |
//! | GET `/api/v1/graph[?map=<sha256>]` | the `WorkflowGraph`, or narrowed by a map `tengu decide --map` kept (re-applied to this config) |
//! | GET `/api/v1/health` | the heartbeat + the `tengu doctor --live` checks (`bootstrap::runtime::read_live`) |
//! | GET `/api/v1/runs` | every recorded run (`RunSummary` + `config_current` + `state`: live · closed · open, `RunState`), the heartbeat holder, `live_run_id` |
//! | GET `/api/v1/runs/:run_id/events?after=&limit=` | events with `seq > after`, `seq` order, `limit` 1–1000 (default 500); `more`, `next_after`; each event + `view` (tone, facets, highlighted edges — `application::studio::board`), `graph` = the map a `--map` run is drawn on |
//! | GET `/api/v1/runs/:run_id/board[?upto=<seq>]` | the run folded over its graph up to `seq`: every node's tone + why, highlighted edges, grey legal sets, header (runtime state, model, loop counters), nodes not in this graph; the run's `state`, `config_current`, trace file |
//! | GET `/api/v1/nodes/:node_id[?map=<sha256>]` | the inspector: the graph node, the validated config section behind it (`application::studio::inspect`, never TOML text), its edges, its evidence files + store keys; redacted |
//! | GET `/api/v1/runs/:run_id/stream` | SSE: the run after `Last-Event-ID` (a browser reconnect; wins) or `?after=` |
//! | GET `/api/v1/live/stream` | SSE: the lease holder's run across restarts (`event: run`) |
//! | GET `/api/v1/control` | the control state (`idle` · `attached` · `starting` · `running` · `stopping` · `stopped` · `failed`), its tone, each action ok or why not, the runtime this Studio runs, the heartbeat seen, scenarios, loops, the last verdict (`Controller::view`) |
//! | POST `/api/v1/control/play` `{scenario?}` | start the sandbox's runtime here (`tengu run`'s `start_session`): 200 running · 409 attached (another holder) / not now · 403 control off · 422 unknown scenario · 500 start failed |
//! | POST `/api/v1/control/stop` | the graceful drain of the runtime this Studio started: 200 stopped · 202 still draining · 409 attached / nothing to stop · 403 |
//! | POST `/api/v1/control/event` `{scenario, loop?}` | one scenario into the owned runtime's loop: 202 queued (`session_id`) · 409 · 422 · 429 queue full · 403 |
//!
//! | Guard (`guard.rs`) | Rule |
//! |---|---|
//! | bind | loopback only (`127.0.0.1`, `::1`, `localhost`): anything else is refused before binding; port 0 (default) = any free one |
//! | token | 32 random bytes, hex, new each process; printed once in the URL fragment (`#t=`, never sent in a request line by the browser); compared in constant time; a GET carries it as `X-Studio-Token` (or `?token=` for `EventSource`) — else 401 |
//! | Host | `127.0.0.1:<port>` / `localhost:<port>` (`[::1]:<port>`) only — else 421 (DNS rebinding), every method |
//! | Origin · Sec-Fetch-Site | an `Origin` must be this server — else 403; a GET marked `cross-site` / `same-site` — 403 |
//! | change request (any method but GET / HEAD, on any path — never decided by how the router matches a path) | CSRF: `X-Studio-Token` header (never `?token=`), `Origin` = this server, `Sec-Fetch-Site: same-origin` — all three, else 403; JSON body (`Content-Type: application/json`, unknown fields refused) ≤ [`MAX_BODY`] (1 MiB, else 413); no CORS headers, no preflight route |
//! | headers | `Cache-Control: no-store`, CSP `default-src 'self'` (no inline script, `frame-ancestors 'none'`), `nosniff`, `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY` |
//!
//! | SSE | Wire |
//! |---|---|
//! | `event: trace` | one `ExecutionEvent`; `id` = its `event_id` (a reconnect sends it back as `Last-Event-ID`) |
//! | `event: run` | the live stream's run: `run_id`, `runtime_id`, `reason` (`attached` · `restarted` · `waiting`), `detail` |
//! | `event: lagged` | the client's buffer (256 live events) was full: `{run_id, last_seq, last_event_id}`, `id` = `last_event_id`, then the stream ends — the browser reconnects and reads the rest from the file |
//! | keep-alive | a comment every 15 s; more than [`MAX_STREAMS`] open = 503; every stream ends at shutdown |
//!
//! Control (`config::studio::control_policy`): `[studio] control = true`
//! (`control-loop-lab`) or `--allow-control`; never in a `[generation]`-bound
//! or hardened sandbox. With it on, Studio records its own run
//! (`RunKind::Studio`: `studio.*`, `control.rs`).
//!
//! Shutdown: SIGINT / SIGTERM first drain the runtime this Studio started
//! (`Controller::shutdown`: no new Play, the Stop path, `studio.stopped`),
//! then end the streams and stop the server (open connections get
//! [`SHUTDOWN_GRACE`]); a second signal exits 130.

mod api;
mod assets;
pub(crate) mod control;
mod guard;
mod sse;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::{watch, Semaphore};
use tracing::{error, info, warn};

use crate::application::studio::stream::StreamLimits;
use crate::bootstrap::studio::StudioContext;
use crate::config::studio::{control_policy, ControlPolicy};
use crate::config::Config;
use crate::domain::observation::now_ms;
use crate::domain::secrets::SecretRegistry;
use crate::domain::trace::RunKind;
use control::{runtime_starter, Controller};

/// SSE streams open at once (each tails a file).
pub(crate) const MAX_STREAMS: usize = 16;
/// How long open connections get after the stop signal.
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Largest request body (a control request is a few bytes).
pub(crate) const MAX_BODY: usize = 1024 * 1024;

/// `--bind` / `--port` / `--allow-control`.
#[derive(Debug, Clone)]
pub(crate) struct ServeOpts {
    pub bind: String,
    pub port: u16,
    pub allow_control: bool,
}

/// What every handler shares.
pub(crate) struct AppState {
    pub ctx: StudioContext,
    /// The token + the `Host` values of the address served.
    pub policy: guard::Policy,
    pub limits: StreamLimits,
    pub streams: Arc<Semaphore>,
    /// `true` once the server stops: every SSE stream ends.
    pub shutdown: watch::Receiver<bool>,
    pub started_ms: i64,
    /// Play / Stop / event (`control.rs`); read-only unless set.
    pub control: Arc<Controller>,
}

impl AppState {
    /// Read-only (control off) until [`Self::with_control`].
    pub(crate) fn new(
        ctx: StudioContext,
        token: guard::Token,
        addr: SocketAddr,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        let control = Arc::new(Controller::read_only(
            ControlPolicy::off("off: read-only Studio"),
            &ctx,
        ));
        Self {
            ctx,
            policy: guard::Policy::new(token, addr),
            limits: StreamLimits::default(),
            streams: Arc::new(Semaphore::new(MAX_STREAMS)),
            shutdown,
            started_ms: now_ms(),
            control,
        }
    }

    pub(crate) fn with_control(mut self, control: Arc<Controller>) -> Self {
        self.control = control;
        self
    }

    /// The URL to open: the token rides in the fragment, which a browser
    /// never sends to the server.
    pub(crate) fn url(&self) -> String {
        format!(
            "http://{}/#t={}",
            self.policy.hosts[0],
            self.policy.token.as_str()
        )
    }
}

pub(crate) fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(assets::index))
        .route("/assets/:file", get(assets::asset))
        .route("/api/v1/meta", get(api::meta))
        .route("/api/v1/graph", get(api::graph))
        .route("/api/v1/health", get(api::health))
        .route("/api/v1/runs", get(api::runs))
        .route("/api/v1/runs/:run_id/events", get(api::events))
        .route("/api/v1/runs/:run_id/board", get(api::board))
        .route("/api/v1/nodes/:node_id", get(api::node))
        .route("/api/v1/runs/:run_id/stream", get(sse::run_stream))
        .route("/api/v1/live/stream", get(sse::live_stream))
        .route("/api/v1/control", get(api::control))
        .route("/api/v1/control/play", post(api::play))
        .route("/api/v1/control/stop", post(api::stop))
        .route("/api/v1/control/event", post(api::event))
        .fallback(api::not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            guard::guard,
        ))
        .with_state(state)
}

/// Serve until SIGINT / SIGTERM (module table). Refuses a non-loopback
/// `--bind` before anything else.
pub(crate) async fn run_studio(
    config: Config,
    secrets: Arc<SecretRegistry>,
    opts: ServeOpts,
) -> Result<()> {
    let ip = guard::loopback(&opts.bind)?;
    let mut signals = super::run::Signals::install()?;
    let policy = control_policy(&config, opts.allow_control);
    let runtime_config = config.clone();
    let ctx = StudioContext::open(config, Arc::clone(&secrets))?;
    let listener = tokio::net::TcpListener::bind((ip, opts.port))
        .await
        .with_context(|| format!("bind {ip}:{}", opts.port))?;
    let addr = listener.local_addr()?;
    let control = if policy.enabled {
        let trace =
            crate::bootstrap::trace::open_sink(&ctx.config, None, RunKind::Studio, &secrets);
        let starter = runtime_starter(runtime_config, Arc::clone(&secrets));
        Controller::new(policy, &ctx, starter, trace)
    } else {
        Controller::read_only(policy, &ctx)
    };
    let (stop_tx, stop_rx) = watch::channel(false);
    let state = Arc::new(
        AppState::new(ctx, guard::Token::generate()?, addr, stop_rx.clone())
            .with_control(Arc::new(control)),
    );
    state.control.started(&addr.to_string());
    info!(
        sandbox = %state.ctx.sandbox,
        config_hash = state.ctx.graph.config_hash.as_deref().unwrap_or("none"),
        %addr,
        control = %state.control.view()["why"].as_str().unwrap_or_default(),
        trace_dir = %state.ctx.evidence().trace_dir,
        "tengu studio serving"
    );
    // The one place the token is shown (stdout; logs go to stderr).
    println!("Studio: {}", state.url());

    let app = router(Arc::clone(&state));
    let mut wait = stop_rx;
    let mut server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = wait.wait_for(|s| *s).await;
            })
            .await
    });
    let signal = tokio::select! {
        r = &mut server => {
            state.control.shutdown("server stopped").await;
            return r.context("studio server task")?.context("studio server stopped on its own");
        }
        signal = signals.next() => signal,
    };
    info!(signal, "tengu studio stopping");
    // The runtime this Studio started drains first (the server still
    // answers GET /api/v1/control meanwhile); a second signal exits at once.
    tokio::select! {
        () = state.control.shutdown(signal) => {}
        second = signals.next() => {
            error!(signal = second, "second signal while draining — exiting now (code 130)");
            std::process::exit(130);
        }
    }
    let _ = stop_tx.send(true);
    tokio::select! {
        r = tokio::time::timeout(SHUTDOWN_GRACE, &mut server) => match r {
            Ok(joined) => joined.context("studio server task")??,
            Err(_) => warn!(grace_secs = SHUTDOWN_GRACE.as_secs(), "connections still open — closing"),
        },
        signal = signals.next() => {
            error!(signal, "second signal while stopping — exiting now (code 130)");
            std::process::exit(130);
        }
    }
    info!("tengu studio stopped");
    Ok(())
}
