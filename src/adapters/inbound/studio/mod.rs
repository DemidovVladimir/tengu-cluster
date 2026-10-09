//! `tengu studio --sandbox <s> [--port <n>] [--bind <ip>]` — the local,
//! read-only Tengu Studio server (`TENGU_STUDIO_PLAN.md` § 4–5, ST-20;
//! `--features studio`). Serves the page in `web/studio/` and a JSON / SSE
//! API over what Rust already computed — the validated graph, the trace, the
//! `tengu doctor --live` verdict. The browser draws; it decides nothing.
//! Composition: `bootstrap::studio::StudioContext`; delivery rules:
//! `application::studio::stream`.
//!
//! | Route (GET only) | Serves |
//! |---|---|
//! | `/` · `/assets/<file>` | `web/studio/` (`index.html`, `studio.css`, `studio.js`), embedded (`assets.rs`) |
//! | `/api/v1/meta` | sandbox, `config_hash`, schema versions, `read_only: true`, `control_enabled: false`, limits, evidence paths, `tones` (status → tone → meaning: `Status::tone`, the page's legend) |
//! | `/api/v1/graph[?map=<sha256>]` | the `WorkflowGraph`, or narrowed by a map `tengu decide --map` kept (re-applied to this config) |
//! | `/api/v1/health` | the heartbeat + the `tengu doctor --live` checks (`bootstrap::runtime::read_live`) |
//! | `/api/v1/runs` | every recorded run (`RunSummary` + `config_current` + `state`: live · closed · open, `RunState`), the heartbeat holder, `live_run_id` |
//! | `/api/v1/runs/:run_id/events?after=&limit=` | events with `seq > after`, `seq` order, `limit` 1–1000 (default 500); `more`, `next_after`; each event + `view` (tone, facets, highlighted edges — `application::studio::board`), `graph` = the map a `--map` run is drawn on |
//! | `/api/v1/runs/:run_id/board[?upto=<seq>]` | the run folded over its graph up to `seq`: every node's tone + why, highlighted edges, grey legal sets, header (runtime state, model, loop counters), nodes not in this graph; the run's `state`, `config_current`, trace file |
//! | `/api/v1/nodes/:node_id[?map=<sha256>]` | the inspector: the graph node, the validated config section behind it (`application::studio::inspect`, never TOML text), its edges, its evidence files + store keys; redacted |
//! | `/api/v1/runs/:run_id/stream` | SSE: the run after `Last-Event-ID` (a browser reconnect; wins) or `?after=` |
//! | `/api/v1/live/stream` | SSE: the lease holder's run across restarts (`event: run`) |
//!
//! | Guard (`guard.rs`) | Rule |
//! |---|---|
//! | bind | loopback only (`127.0.0.1`, `::1`, `localhost`): anything else is refused before binding; port 0 (default) = any free one |
//! | token | 32 random bytes, hex, new each process; printed once in the URL fragment (`#t=`, never sent in a request line by the browser); every `/api/` request carries it (`X-Studio-Token`, or `?token=` for `EventSource`), compared in constant time — else 401 |
//! | Host | `127.0.0.1:<port>` / `localhost:<port>` (`[::1]:<port>`) only — else 421 (DNS rebinding) |
//! | Origin · Sec-Fetch-Site | an `Origin` must be this server — else 403; an `/api/` request marked `cross-site` / `same-site` — 403 |
//! | methods | GET / HEAD only: anything else 405; no CORS headers |
//! | headers | `Cache-Control: no-store`, CSP `default-src 'self'` (no inline script, `frame-ancestors 'none'`), `nosniff`, `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY` |
//!
//! | SSE | Wire |
//! |---|---|
//! | `event: trace` | one `ExecutionEvent`; `id` = its `event_id` (a reconnect sends it back as `Last-Event-ID`) |
//! | `event: run` | the live stream's run: `run_id`, `runtime_id`, `reason` (`attached` · `restarted` · `waiting`), `detail` |
//! | `event: lagged` | the client's buffer (256 live events) was full: `{run_id, last_seq, last_event_id}`, `id` = `last_event_id`, then the stream ends — the browser reconnects and reads the rest from the file |
//! | keep-alive | a comment every 15 s; more than [`MAX_STREAMS`] open = 503; every stream ends at shutdown |
//!
//! Shutdown: SIGINT / SIGTERM end the streams and stop the server (open
//! connections get [`SHUTDOWN_GRACE`]); a second signal exits 130. Studio
//! records no trace of its own yet (`RunKind::Studio` waits for ST-30).

mod api;
mod assets;
mod guard;
mod sse;
#[cfg(test)]
mod tests;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::routing::get;
use axum::Router;
use tokio::sync::{watch, Semaphore};
use tracing::{error, info, warn};

use crate::application::studio::stream::StreamLimits;
use crate::bootstrap::studio::StudioContext;
use crate::config::Config;
use crate::domain::observation::now_ms;
use crate::domain::secrets::SecretRegistry;

/// SSE streams open at once (each tails a file).
pub(crate) const MAX_STREAMS: usize = 16;
/// How long open connections get after the stop signal.
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// `--bind` / `--port`.
#[derive(Debug, Clone)]
pub(crate) struct ServeOpts {
    pub bind: String,
    pub port: u16,
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
}

impl AppState {
    pub(crate) fn new(
        ctx: StudioContext,
        token: guard::Token,
        addr: SocketAddr,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self {
            ctx,
            policy: guard::Policy::new(token, addr),
            limits: StreamLimits::default(),
            streams: Arc::new(Semaphore::new(MAX_STREAMS)),
            shutdown,
            started_ms: now_ms(),
        }
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
        .fallback(api::not_found)
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
    let ctx = StudioContext::open(config, secrets)?;
    let listener = tokio::net::TcpListener::bind((ip, opts.port))
        .await
        .with_context(|| format!("bind {ip}:{}", opts.port))?;
    let addr = listener.local_addr()?;
    let (stop_tx, stop_rx) = watch::channel(false);
    let state = Arc::new(AppState::new(
        ctx,
        guard::Token::generate()?,
        addr,
        stop_rx.clone(),
    ));
    info!(
        sandbox = %state.ctx.sandbox,
        config_hash = state.ctx.graph.config_hash.as_deref().unwrap_or("none"),
        %addr,
        trace_dir = %state.ctx.evidence().trace_dir,
        "tengu studio serving (read-only)"
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
    tokio::select! {
        r = &mut server => {
            return r.context("studio server task")?.context("studio server stopped on its own");
        }
        signal = signals.next() => info!(signal, "tengu studio stopping"),
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
