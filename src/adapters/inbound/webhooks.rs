//! Inbound webhook listener — `tengu webhooks --sandbox <name>`.
//!
//! Each `[webhooks.endpoints.<name>]` block in the sandbox config binds
//! one URL path to one agent + one HMAC shared secret. Incoming POSTs
//! are verified with HMAC-SHA256 against `secret_env`/`secret`, the body
//! is wrapped into a synthesized user message, and the orchestrator runs
//! a one-shot turn with a per-request `session_id` of the form
//! `webhook-<endpoint>-<uuid>`.
//!
//! Response: `202 Accepted` with `{"session_id": "..."}` JSON body.
//! Agents can take minutes — webhook senders typically time out at 10s,
//! so we never block waiting for the agent. Final output goes to Open
//! Brain (Postgres `agentic_memory`, with `postgres_memory`) and the
//! tracing log. Recall later by querying Postgres for the `session_id`.
//!
//! ## Files / responsibilities
//!
//! - `run_webhooks(...)` — entry point called by `Commands::Webhooks` in
//!   `main.rs`. Owns the long-running tokio task that hosts the axum
//!   server.
//! - `app_state(...)` + `router(...)` — the same routes for `tengu run`
//!   (`inbound/run.rs`), which mounts them in its own process on its
//!   `LoopDispatch` (every loop built once) with graceful shutdown.
//! - `WebhookAppState` — per-process shared state (config, memory
//!   manager, loop dispatch). Cloned into each request handler.
//! - `dispatch_webhook(...)` — per-request handler. HMAC verify → mint
//!   session_id → queue the loop event (`LoopDispatch::submit`; 503 while
//!   shutting down) or spawn an orchestrator turn → 202 reply.
//! - `verify_hmac(...)` — constant-time HMAC-SHA256 verify via the
//!   `hmac` crate's `Mac::verify_slice`.
//!
//! ## Trace (`domain/trace.rs`, Studio)
//!
//! `tengu webhooks` records one run per process (`RunKind::Webhooks`,
//! `runtime_id` = the lease holder; ends with `run.closed`); under `tengu
//! run` / Studio Play the routes write into the runtime's run. Per request
//! to a configured endpoint (an unknown one is logged, never recorded — its
//! name is the sender's):
//!
//! | Event | Node | Status | Payload |
//! |---|---|---|---|
//! | `trigger.webhook` (root; component `loop` or `orchestrator`) | `trigger:webhook/<endpoint>` | running | `endpoint`, `kind` (`loop` · `agent`), the loop or agent, `body_bytes` |
//! | its work: `loop.queued` … (loop endpoint) · `plan.*` / `step.*` (agent endpoint, `orchestrator/trace.rs`) | | | parent = the root, session = the request's |
//! | `webhook.responded` (child of the root) | same | `domain::trace::http_status`: 2xx ok · 401 / 429 / 503 refused · 500 failed | `endpoint`, `kind`, `status_code`, `error` (line 1); `duration_ms` |
//!
//! Never recorded: the body (its size only), a header, a secret. A loop
//! escalation's orchestrator turn records under the loop event's cause.
//!
//! ## Doctrine
//!
//! - Behaviour changes via TOML, not code: every endpoint binding is in
//!   `sandboxes/<name>/config.toml`. Adding a new webhook is one TOML
//!   block, no recompile.
//! - Session_id is the recall key: per-request, namespaced as
//!   `webhook-<endpoint>-<uuid>`. Pairs with Fix B (parent / child
//!   share one id) and Fix A (within-session output recall).

#![cfg(feature = "webhooks")]

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use std::sync::OnceLock;
use std::time::Instant;

use crate::adapters::outbound::engines::build_engine;
use crate::adapters::outbound::noop::{NoopActivity, NoopRuntimeToolExecutor};
use crate::application::chat::tool_loop::collect_engine_response;
use crate::application::memory::manager::MemoryManager;
use crate::application::orchestrator::trace::OrchestratorTrace;
use crate::application::runtime::loops::{LoopDispatch, LoopHandler, Refused};
use crate::application::skills::registry::{FileSystemSkillSource, SkillRegistry};
use crate::application::trace_exec::{self, Cause};
use crate::config::{Config, WebhookEndpointConfig};
use crate::domain::message::{Message, Role};
use crate::domain::secrets::SecretRegistry;
use crate::domain::trace::{
    http_status, line1, Component, EventDraft, RunKind, Status, RUN_CLOSED,
};
use crate::domain::workflow::node_id;
use crate::ports::decision::Escalator;
use crate::ports::engine::ToolExecutor;
use crate::ports::engine::{Engine, EngineContext};
use crate::ports::orchestration::ChatServiceFactory;
use crate::ports::tool_activity::ToolActivityPort;
use crate::ports::trace::TraceSink;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
    routing::post,
    Router,
};
use hmac::{Hmac, Mac};
use serde_json::json;
use serde_json::Value;
use sha2::Sha256;
use tracing::{error, info, warn};

type HmacSha256 = Hmac<Sha256>;

/// Header carrying the HMAC-SHA256 signature of the request body.
/// Format: `sha256=<lowercase-hex>`. Mirrors GitHub / Stripe convention
/// (different prefix, identical shape).
const SIG_HEADER: &str = "x-tengu-signature";

/// Long-running entry point: takes the leases `tengu run` takes
/// (`bootstrap::runtime::LeasePlan`: `runtime:<sandbox>`, and `state:<dir>`
/// for an `[xmarket]` state dir — never beside `tengu run` of the sandbox or
/// of another sandbox on its ledger; held ⇒ exit 1 naming the holder), then
/// serves until SIGINT / SIGTERM (graceful: loop events drain ≤ `[runtime]
/// shutdown_grace_secs`, the leases are freed; a second signal exits 130) or
/// a lost lease (exit 1).
pub async fn run_webhooks(config: Config, secret_registry: Arc<SecretRegistry>) -> Result<()> {
    use crate::application::runtime::{LeaseTiming, Supervisor};
    use crate::bootstrap::runtime::{LeasePlan, OwnerLeases};

    check_config(&config)?;
    let addr = bind_addr(&config)?;
    // Before anything slow: a SIGTERM during boot must stop us cleanly.
    let signals = super::run::Signals::install()?;
    let plan = LeasePlan::of(&config);
    let timing = LeaseTiming::default();
    let owner = OwnerLeases::take(&plan, timing.ttl_ms).await?;
    let mut supervisor = Supervisor::new();
    owner.keep(&mut supervisor, timing);
    info!(
        sandbox = %plan.sandbox,
        leases = ?owner.resources(),
        holder = %owner.holder(),
        state_dir = %plan.state_dir.display(),
        "tengu webhooks leases taken"
    );
    let stopper = supervisor.stopper();
    let grace = std::time::Duration::from_secs(config.runtime.shutdown_grace_secs);
    // This process's recording (module docs § Trace), after the leases.
    let trace = crate::bootstrap::trace::open_sink(
        &config,
        Some(owner.holder()),
        RunKind::Webhooks,
        &secret_registry,
    );
    let holder = owner.holder().to_string();
    let (served, drained) = serve_webhooks(
        config,
        secret_registry,
        addr,
        signals,
        stopper.clone(),
        (Arc::clone(&trace), holder),
    )
    .await;
    supervisor
        .shutdown(tokio::time::Instant::now() + grace)
        .await;
    let released = owner.release().await;
    let stop = stopper.cause();
    info!(lease_released = released, stop = ?stop, "tengu webhooks stopped");
    let failed = served.is_err() || stop.as_ref().is_some_and(|s| s.failed);
    trace.emit(
        EventDraft::new(
            Component::Runtime,
            RUN_CLOSED,
            if failed { Status::Failed } else { Status::Ok },
        )
        .payload(json!({
            "reason": stop.as_ref().map(|s| s.reason.clone()),
            "failed": failed,
            "lease_released": released,
            "drained": drained.map(|d| json!({
                "finished": d.finished,
                "dropped": d.dropped,
                "aborted": d.aborted,
            })),
        })),
    );
    served?;
    match stop {
        Some(s) if s.failed => anyhow::bail!("tengu webhooks stopped: {}", s.reason),
        _ => Ok(()),
    }
}

/// The listener under the leases: every endpoint loop built once, served
/// until the stop signal (a signal, a lost lease), then the loop events
/// drained (`None`: it never served). `rec` = the recording + the lease
/// holder its audit lines name.
async fn serve_webhooks(
    config: Config,
    secret_registry: Arc<SecretRegistry>,
    addr: SocketAddr,
    signals: super::run::Signals,
    stopper: crate::application::runtime::Stopper,
    rec: (Arc<dyn TraceSink>, String),
) -> (
    Result<()>,
    Option<crate::application::runtime::loops::DrainReport>,
) {
    match listen(config, secret_registry, addr, signals, stopper, rec).await {
        Ok((served, drained)) => (served, Some(drained)),
        Err(e) => (Err(e), None),
    }
}

async fn listen(
    config: Config,
    secret_registry: Arc<SecretRegistry>,
    addr: SocketAddr,
    mut signals: super::run::Signals,
    stopper: crate::application::runtime::Stopper,
    (trace, holder): (Arc<dyn TraceSink>, String),
) -> Result<(Result<()>, crate::application::runtime::loops::DrainReport)> {
    use crate::application::runtime::stopped;

    let grace = std::time::Duration::from_secs(config.runtime.shutdown_grace_secs);
    // Memory manager is shared across all requests — opening it per
    // request would be wasteful (store open + embedder per webhook).
    let memory_manager = Arc::new(MemoryManager::new());

    // One `DecisionLoop` per loop referenced by an endpoint — built once so
    // its action history survives across events. Escalation reuses the
    // one-shot orchestrator path when `[orchestrator]` is configured.
    let slot = TraceSlot::default();
    let _ = slot.set(Arc::clone(&trace));
    let escalator = orchestrator_escalator(&config, &memory_manager, Arc::clone(&slot));
    let rec = crate::bootstrap::trace::Recording::of(Arc::clone(&trace), Some(&holder));
    let mut handlers: BTreeMap<String, Arc<dyn LoopHandler>> = BTreeMap::new();
    for ep in config.webhooks.endpoints.values() {
        let Some(name) = &ep.decision_loop else {
            continue;
        };
        if handlers.contains_key(name) {
            continue;
        }
        // Step / tool events into this process's recording; audit lines
        // carry its runtime + run id.
        let dl = crate::bootstrap::decision::build_decision_loop(
            &config,
            name,
            escalator.clone(),
            Arc::clone(&secret_registry),
            rec.clone(),
        )?;
        handlers.insert(name.clone(), dl);
    }
    let loops = Arc::new(
        LoopDispatch::new(
            handlers,
            config.runtime.max_decisions_in_flight,
            config.runtime.max_queued_per_loop,
        )
        .with_trace(Arc::clone(&trace)),
    );

    let state = app_state(
        config,
        memory_manager,
        Arc::clone(&loops),
        secret_registry,
        trace,
    )?;
    let endpoints_summary: Vec<&str> = state
        .config
        .webhooks
        .endpoints
        .keys()
        .map(|s| s.as_str())
        .collect();
    info!(
        %addr,
        endpoints = ?endpoints_summary,
        sandbox = ?state.config.sandbox_name,
        "tengu webhooks listening"
    );

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind webhook listener to {}", addr))?;

    let on_signal = stopper.clone();
    let signal_task = tokio::spawn(async move {
        let first = signals.next().await;
        on_signal.stop(first, false);
        let second = signals.next().await;
        error!(
            signal = second,
            "second signal while draining — exiting now (code 130)"
        );
        std::process::exit(130);
    });
    let mut stop = stopper.subscribe();
    let served = axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            stopped(&mut stop).await;
        })
        .await
        .context("webhook server exited unexpectedly");
    // Not stopped yet ⇒ the server died on its own.
    stopper.stop("webhook server exited", true);
    let drained = loops.drain(tokio::time::Instant::now() + grace).await;
    info!(
        finished = drained.finished,
        dropped = drained.dropped,
        aborted = drained.aborted,
        "webhook loop events drained"
    );
    signal_task.abort();
    Ok((served, drained))
}

/// Refuse a config the listener cannot serve: disabled, no endpoints,
/// planner endpoints without `[orchestrator]`, bad endpoint auth or loop refs.
fn check_config(config: &Config) -> Result<()> {
    if !config.webhooks.enabled {
        return Err(anyhow!(
            "[webhooks] not enabled in this sandbox config — set `[webhooks] enabled = true` to start the listener"
        ));
    }
    if config.webhooks.endpoints.is_empty() {
        return Err(anyhow!(
            "[webhooks] no endpoints defined — add at least one `[webhooks.endpoints.<name>]` block"
        ));
    }
    let has_planner_endpoint = config
        .webhooks
        .endpoints
        .values()
        .any(|ep| ep.decision_loop.is_none());
    if has_planner_endpoint && config.orchestrator.is_none() {
        return Err(anyhow!(
            "[webhooks] requires `[orchestrator]` to be configured — endpoints without `loop` dispatch through the orchestrator"
        ));
    }
    validate_endpoints(&config.webhooks.endpoints, &config.decision_loops)
}

/// `[webhooks] bind:port`.
pub(crate) fn bind_addr(config: &Config) -> Result<SocketAddr> {
    let (bind, port) = (&config.webhooks.bind, config.webhooks.port);
    format!("{bind}:{port}")
        .parse()
        .with_context(|| format!("invalid bind address {bind}:{port}"))
}

/// The recording an escalator writes into, set once it is open (`tengu
/// run` opens its recording after building the escalator).
pub(crate) type TraceSlot = Arc<OnceLock<Arc<dyn TraceSink>>>;

/// The escalator loops use when `[orchestrator]` is configured: a
/// low-confidence step becomes a one-shot orchestrator turn, recorded into
/// `trace` (once set) under the loop event's cause.
pub(crate) fn orchestrator_escalator(
    config: &Config,
    memory_manager: &Arc<MemoryManager>,
    trace: TraceSlot,
) -> Option<Arc<dyn Escalator>> {
    config.orchestrator.as_ref().map(|_| {
        Arc::new(OrchestratorEscalator {
            config: config.clone(),
            memory_manager: Arc::clone(memory_manager),
            trace,
        }) as Arc<dyn Escalator>
    })
}

/// Validated shared state for [`router`]. `loops` must hold every loop an
/// endpoint names (`tengu run` passes its dispatch with every loop);
/// `trace` = the process's recording (module docs § Trace).
pub(crate) fn app_state(
    config: Config,
    memory_manager: Arc<MemoryManager>,
    loops: Arc<LoopDispatch>,
    secret_registry: Arc<SecretRegistry>,
    trace: Arc<dyn TraceSink>,
) -> Result<Arc<WebhookAppState>> {
    check_config(&config)?;
    for (name, ep) in &config.webhooks.endpoints {
        if let Some(l) = ep.decision_loop.as_ref().filter(|l| !loops.has(l)) {
            return Err(anyhow!(
                "[webhooks.endpoints.{name}] loop `{l}` is not running in this process"
            ));
        }
    }
    Ok(Arc::new(WebhookAppState {
        config,
        memory_manager,
        loops,
        _secret_registry: secret_registry,
        trace,
    }))
}

/// One route handles all endpoints via the {name} path capture.
/// Per-request lookup against config keeps the route shape declarative —
/// adding a new endpoint is a TOML edit, no router rebuild.
pub(crate) fn router(state: Arc<WebhookAppState>) -> Router {
    Router::new()
        .route("/webhooks/:name", post(dispatch_webhook))
        .with_state(state)
}

/// Per-process shared state. Cloned cheaply into each request handler
/// via the axum `State` extractor wrapping an `Arc`.
pub(crate) struct WebhookAppState {
    config: Config,
    memory_manager: Arc<MemoryManager>,
    /// Loop events for endpoints with `loop = "<name>"` (one dispatch per
    /// process: `tengu webhooks` builds its own, `tengu run` passes its).
    loops: Arc<LoopDispatch>,
    /// Held for redaction parity with telegram (unused in v1 — webhook
    /// responses are 202s with no agent text). Kept for the inevitable
    /// future "sync mode" that mirrors telegram's `secret_registry.redact`.
    _secret_registry: Arc<SecretRegistry>,
    /// Where each request's `trigger.webhook` and its work go (module docs
    /// § Trace; `NoopTrace` = nowhere).
    trace: Arc<dyn TraceSink>,
}

/// One request's root + answer (module docs § Trace).
struct RequestTrace<'a> {
    sink: &'a dyn TraceSink,
    endpoint: &'a str,
    kind: &'static str,
    component: Component,
    session: &'a str,
    root: Option<String>,
    t0: Instant,
}

impl<'a> RequestTrace<'a> {
    /// Write the root `trigger.webhook` (running).
    fn begin(
        sink: &'a dyn TraceSink,
        endpoint: &'a str,
        ep: &WebhookEndpointConfig,
        session: &'a str,
        body_bytes: usize,
    ) -> Self {
        let (kind, component, target) = match &ep.decision_loop {
            Some(l) => ("loop", Component::Loop, ("loop", l.as_str())),
            None => (
                "agent",
                Component::Orchestrator,
                ("agent", ep.agent.as_str()),
            ),
        };
        let mut r = Self {
            sink,
            endpoint,
            kind,
            component,
            session,
            root: None,
            t0: Instant::now(),
        };
        let mut payload = json!({"endpoint": endpoint, "kind": kind, "body_bytes": body_bytes});
        if let Value::Object(o) = &mut payload {
            o.insert(target.0.into(), json!(target.1));
        }
        r.root = sink.emit(r.draft("trigger.webhook", Status::Running).payload(payload));
        r
    }

    fn draft(&self, kind: &str, status: Status) -> EventDraft {
        EventDraft::new(self.component, kind, status)
            .session(self.session)
            .node(node_id::trigger_webhook(self.endpoint))
    }

    /// What the request's work runs caused by.
    fn cause(&self) -> Cause {
        Cause::new(self.root.clone(), self.session)
    }

    /// Write `webhook.responded` for the answer the sender gets.
    fn responded(&self, code: StatusCode, answer: &Value) {
        let mut payload = json!({
            "endpoint": self.endpoint,
            "kind": self.kind,
            "status_code": code.as_u16(),
        });
        if let (Some(e), Value::Object(o)) =
            (answer.get("error").and_then(Value::as_str), &mut payload)
        {
            o.insert("error".into(), json!(line1(e)));
        }
        let mut d = self
            .draft("webhook.responded", http_status(code.as_u16()))
            .duration(self.t0.elapsed().as_millis() as u64)
            .payload(payload);
        if let Some(root) = &self.root {
            d = d.parent(root.clone());
        }
        self.sink.emit(d);
    }
}

/// `POST /webhooks/{name}`. Validates HMAC, mints per-request session_id,
/// fires-and-forgets the orchestrator turn, returns 202 immediately. A
/// configured endpoint's request is recorded (module docs § Trace).
async fn dispatch_webhook(
    Path(name): Path<String>,
    State(state): State<Arc<WebhookAppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // 1. Look up endpoint binding. 404 if unknown — surface clearly so
    //    senders see a typo'd URL rather than a vague auth failure. Not
    //    recorded: the name is the sender's, not a configured node.
    let Some(endpoint) = state.config.webhooks.endpoints.get(&name) else {
        warn!(name = %name, "unknown webhook endpoint");
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("unknown webhook endpoint '{}'", name)})),
        )
            .into_response();
    };

    // Mint the per-request session_id — uuid keeps it unique even when the
    // same endpoint fires twice in the same second; the trace root and every
    // event of the request's work carry it.
    let session_id = format!("webhook-{}-{}", name, uuid::Uuid::new_v4());
    let rt = RequestTrace::begin(&*state.trace, &name, endpoint, &session_id, body.len());
    let (code, answer) = handle_webhook(&state, &name, endpoint, &headers, &body, &rt).await;
    rt.responded(code, &answer);
    (code, Json(answer)).into_response()
}

/// Steps 2–6 of a configured endpoint's request; the answer.
async fn handle_webhook(
    state: &Arc<WebhookAppState>,
    name: &str,
    endpoint: &WebhookEndpointConfig,
    headers: &HeaderMap,
    body: &Bytes,
    rt: &RequestTrace<'_>,
) -> (StatusCode, Value) {
    let session_id = rt.session;
    // 2. Resolve the shared secret. `secret_env` (preferred) reads at
    //    request time so a key rotation doesn't require restart.
    let auth = match resolve_endpoint_auth(endpoint) {
        Ok(s) => s,
        Err(e) => {
            error!(name = %name, error = %e, "secret resolve failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": format!("server misconfiguration: {}", e)}),
            );
        }
    };

    // 3. Verify HMAC (or the static `Authorization` header). 401 on
    //    missing-header / bad-format / mismatch.
    let verified = match &auth {
        EndpointAuth::Hmac(secret) => verify_signature(headers, body, secret.as_bytes()),
        EndpointAuth::Header(expected) => verify_auth_header(headers, expected),
    };
    if let Err(e) = verified {
        warn!(name = %name, error = %e, "webhook auth failed");
        return (StatusCode::UNAUTHORIZED, json!({"error": e.to_string()}));
    }

    // 5a. Decision-loop endpoint: the body is the event (Helius sends a
    //     JSON array of transactions). Non-JSON bodies become a string.
    //     Queued on the loop (one event at a time per loop); failures are
    //     logged by the dispatch and land in the decision audit. Its
    //     `loop.queued` hangs under the request's root.
    if let Some(loop_name) = &endpoint.decision_loop {
        let event: Value = serde_json::from_slice(body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into()));
        let submitted = trace_exec::caused_by_sync(rt.cause(), || {
            state.loops.submit(loop_name, event, session_id.to_string())
        });
        match submitted {
            Ok(()) => {}
            Err(refused @ Refused::ShuttingDown) => {
                warn!(name = %name, decision_loop = %loop_name, session_id = %session_id, "loop event refused: shutting down");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    json!({"error": refused.to_string()}),
                );
            }
            // The dispatch warned (loop, session id, counts).
            Err(refused @ Refused::QueueFull { .. }) => {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    json!({"error": refused.to_string()}),
                );
            }
            Err(Refused::UnknownLoop) => {
                error!(name = %name, decision_loop = %loop_name, "decision loop not built");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": format!("decision loop '{}' not available", loop_name)}),
                );
            }
        }
        info!(
            endpoint = %name,
            decision_loop = %loop_name,
            session_id = %session_id,
            body_bytes = body.len(),
            "webhook accepted; dispatching to decision loop"
        );
        return (
            StatusCode::ACCEPTED,
            json!({
                "status": "accepted",
                "session_id": session_id,
                "endpoint": name,
                "loop": loop_name,
            }),
        );
    }

    // 5. Synthesize the user message: goal_template + body. Body bytes
    //    aren't required to be UTF-8 (binary webhooks exist) — fall
    //    through to lossy stringification rather than rejecting.
    let body_str = String::from_utf8_lossy(body);
    let user_message = format!(
        "{}\n\nPayload (raw body, may be JSON):\n{}",
        endpoint.goal_template, body_str
    );

    // 6. Spawn the orchestrator turn. Fire-and-forget — caller gets
    //    202 immediately; agent runs in the background, its plans and steps
    //    recorded under the request's root.
    let agent_name = endpoint.agent.clone();
    let session_id_for_handle = session_id.to_string();
    let state_for_handle = Arc::clone(state);
    let trace = OrchestratorTrace::of(&state.trace, rt.root.clone());
    tokio::spawn(async move {
        run_one_shot(
            &state_for_handle.config,
            &state_for_handle.memory_manager,
            &session_id_for_handle,
            user_message,
            trace,
        )
        .await;
    });

    info!(
        endpoint = %name,
        agent = %agent_name,
        session_id = %session_id,
        body_bytes = body.len(),
        "webhook accepted; dispatching one-shot orchestrator turn"
    );

    (
        StatusCode::ACCEPTED,
        json!({
            "status": "accepted",
            "session_id": session_id,
            "endpoint": name,
            "agent": agent_name,
        }),
    )
}

/// Run one orchestrator turn for the synthesized webhook user message.
/// Final agent text is written to the tracing log; durable side-effects
/// (`agentic_memory` step summaries, metrics records) flow through the standard
/// pipeline. Errors are logged and discarded — the listener stays up.
/// `trace` = where its plans and steps are recorded (`None`: nowhere).
async fn run_one_shot(
    config: &Config,
    memory_manager: &Arc<MemoryManager>,
    session_id: &str,
    user_message: String,
    trace: Option<OrchestratorTrace>,
) {
    // Per-request orchestrator construction. Uses `WebhookChatServiceFactory`
    // — a per-turn rebuild-from-config pattern modelled on `EvalChatServiceFactory`
    // in `adapters/inbound/eval.rs`. Avoids the `OrchestratorSnapshots` map that
    // chat / telegram use (those surfaces pre-build per-agent state at
    // startup and refresh it before each turn; webhooks are stateless
    // one-shots, so each call just looks up the agent in `Config.agents`
    // and constructs the engine + tools fresh).
    let factory: Arc<dyn ChatServiceFactory> = Arc::new(WebhookChatServiceFactory {
        cfg: Arc::new(config.clone()),
    });

    let Some(orchestrator) = crate::bootstrap::orchestrator::build_orchestrator(
        config,
        factory,
        Arc::clone(memory_manager),
        session_id.to_string(),
        trace,
    ) else {
        warn!(
            session_id = %session_id,
            "build_orchestrator returned None — webhook turn aborted (engine != \"rag\"). This is a sandbox-config issue, not a runtime crisis."
        );
        return;
    };

    let final_text = orchestrator.handle(user_message).await;
    if final_text.trim().is_empty() {
        warn!(
            session_id = %session_id,
            "webhook orchestrator turn returned empty output"
        );
        return;
    }

    let preview: String = final_text.chars().take(400).collect();
    let truncated = final_text.chars().count() > 400;
    info!(
        session_id = %session_id,
        output_chars = final_text.chars().count(),
        preview = %preview,
        truncated = truncated,
        "webhook orchestrator turn completed"
    );

    // Persist the orchestrator's final output to Open Brain (`agentic_memory`,
    // Postgres) with a synthetic step_id `"webhook-handler"`. Without this,
    // webhook turns where the planner emitted `Direct { response }` (no
    // subagent dispatched, so no `compress_and_store` and no backstop) leave
    // nothing recallable. Coexists with subagent step summaries on the same
    // session_id; different step_ids keep them distinct.
    persist_webhook_output(session_id, &final_text).await;
}

/// Fire-and-forget persist of the webhook turn's final text into Open Brain
/// (`agentic_memory`, Postgres). Logs success at info, errors at warn — never
/// propagates. Embedding is best-effort: no `OPENROUTER_API_KEY` (or an embed
/// error) stores a text-only memory.
#[cfg(feature = "postgres_memory")]
async fn persist_webhook_output(session_id: &str, final_text: &str) {
    let embedding = match std::env::var("OPENROUTER_API_KEY") {
        Ok(api_key) => {
            let embedder = crate::adapters::outbound::memory::embedder::Embedder::new(
                api_key,
                // Open Brain is `vector(1536)`: always the pinned model.
                crate::domain::memory::DEFAULT_EMBEDDING_MODEL.to_string(),
            );
            match embedder.embed(final_text).await {
                Ok(v) => Some(v),
                Err(e) => {
                    warn!(
                        session_id = %session_id,
                        error = %e,
                        "webhook persist: embedding failed; writing text-only memory"
                    );
                    None
                }
            }
        }
        Err(_) => None,
    };
    match crate::adapters::outbound::tools::agentic_memory::write_step_summary_with_embedding(
        session_id,
        "webhook-handler",
        final_text,
        embedding.as_deref(),
    )
    .await
    {
        Ok(id) => info!(
            entry_id = %id,
            session_id = %session_id,
            step_id = "webhook-handler",
            "webhook output persisted to agentic_memory"
        ),
        Err(e) => warn!(
            session_id = %session_id,
            error = %e,
            "webhook output persist FAILED — agentic_memory unavailable (is TENGU_MEMORY_DATABASE_URL set?)"
        ),
    }
}

/// Without `postgres_memory` there is no durable memory backend — the webhook
/// turn still completes and its output is in the tracing log, just not
/// recallable.
#[cfg(not(feature = "postgres_memory"))]
async fn persist_webhook_output(session_id: &str, _final_text: &str) {
    warn!(
        session_id = %session_id,
        "webhook output not persisted — built without the `postgres_memory` feature"
    );
}

/// How an endpoint authenticates senders.
enum EndpointAuth {
    /// HMAC-SHA256 of the body in `X-Tengu-Signature`.
    Hmac(String),
    /// Exact `Authorization` header value (Helius `authHeader`).
    Header(String),
}

/// Resolve the endpoint's auth: `auth_header_env` → header compare,
/// otherwise the HMAC secret (`resolve_endpoint_secret`).
fn resolve_endpoint_auth(ep: &WebhookEndpointConfig) -> Result<EndpointAuth> {
    let Some(env_name) = &ep.auth_header_env else {
        return resolve_endpoint_secret(ep).map(EndpointAuth::Hmac);
    };
    if ep.secret_env.is_some() || ep.secret.is_some() {
        return Err(anyhow!(
            "endpoint config has `auth_header_env` and an HMAC secret — use exactly one"
        ));
    }
    match std::env::var(env_name) {
        Ok(v) if !v.is_empty() => Ok(EndpointAuth::Header(v)),
        _ => Err(anyhow!(
            "auth_header_env points at `{}` but that env var is unset or empty in the listener's process",
            env_name
        )),
    }
}

/// Resolve the shared secret from `secret_env` (preferred) or `secret`
/// (literal in TOML). Returns the secret as a `String`. Fails when
/// neither is set, both are set, or `secret_env` points at an unset var.
fn resolve_endpoint_secret(ep: &WebhookEndpointConfig) -> Result<String> {
    match (&ep.secret_env, &ep.secret) {
        (Some(_), Some(_)) => Err(anyhow!(
            "endpoint config has both `secret_env` and `secret` — use exactly one"
        )),
        (None, None) => Err(anyhow!(
            "endpoint config has neither `secret_env` nor `secret` — one is required for HMAC verification"
        )),
        (Some(env_name), None) => std::env::var(env_name).map_err(|_| {
            anyhow!(
                "secret_env points at `{}` but that env var is not set in the listener's process",
                env_name
            )
        }),
        (None, Some(s)) if s.is_empty() => {
            Err(anyhow!("inline `secret` is empty — refuse to verify against empty key"))
        }
        (None, Some(s)) => Ok(s.clone()),
    }
}

/// Validate every endpoint's secret config at startup so misconfiguration
/// fails loudly *before* the listener accepts traffic. Catches the
/// "neither set / both set / empty inline" cases that
/// `resolve_endpoint_secret` would catch per-request.
fn validate_endpoints(
    endpoints: &HashMap<String, WebhookEndpointConfig>,
    loops: &HashMap<String, crate::config::decision_loop::DecisionLoopConfig>,
) -> Result<()> {
    for (name, ep) in endpoints {
        if ep.auth_header_env.is_some() {
            if ep.secret_env.is_some() || ep.secret.is_some() {
                return Err(anyhow!(
                    "[webhooks.endpoints.{}] has `auth_header_env` and an HMAC secret — use exactly one",
                    name
                ));
            }
        } else {
            validate_hmac_secret(name, ep)?;
        }
        match &ep.decision_loop {
            Some(l) if !loops.contains_key(l) => {
                return Err(anyhow!(
                    "[webhooks.endpoints.{}] `loop = \"{}\"` has no [decision_loops.{}] block",
                    name,
                    l,
                    l
                ));
            }
            Some(_) => {}
            None if ep.agent.is_empty() => {
                return Err(anyhow!("[webhooks.endpoints.{}] `agent` is required", name));
            }
            None => {}
        }
    }
    Ok(())
}

fn validate_hmac_secret(name: &str, ep: &WebhookEndpointConfig) -> Result<()> {
    {
        match (&ep.secret_env, &ep.secret) {
            (Some(_), Some(_)) => {
                return Err(anyhow!(
                    "[webhooks.endpoints.{}] has both `secret_env` and `secret` — use exactly one",
                    name
                ));
            }
            (None, None) => {
                return Err(anyhow!(
                    "[webhooks.endpoints.{}] has neither `secret_env` nor `secret` — one is required",
                    name
                ));
            }
            (None, Some(s)) if s.is_empty() => {
                return Err(anyhow!(
                    "[webhooks.endpoints.{}] inline `secret` is empty — refuse to verify against empty key",
                    name
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Verify the `X-Tengu-Signature` header against `body` using
/// HMAC-SHA256 with the shared secret. Returns `Ok` on match,
/// `Err(reason)` on missing/malformed/mismatched.
///
/// Constant-time compare via `Mac::verify_slice`.
fn verify_signature(headers: &HeaderMap, body: &[u8], secret: &[u8]) -> Result<()> {
    let header_value = headers
        .get(SIG_HEADER)
        .ok_or_else(|| anyhow!("missing `X-Tengu-Signature` header"))?
        .to_str()
        .map_err(|_| anyhow!("`X-Tengu-Signature` header is not valid UTF-8"))?;
    verify_hmac(header_value, body, secret)
}

/// Compare the `Authorization` header to the expected value in constant time.
fn verify_auth_header(headers: &HeaderMap, expected: &str) -> Result<()> {
    let got = headers
        .get(axum::http::header::AUTHORIZATION)
        .ok_or_else(|| anyhow!("missing `Authorization` header"))?
        .as_bytes();
    if ct_eq(got, expected.as_bytes()) {
        Ok(())
    } else {
        Err(anyhow!("Authorization header mismatch"))
    }
}

/// Constant-time byte comparison (length leak only).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Pure HMAC verify — extracted so unit tests don't need an `HeaderMap`.
fn verify_hmac(header_value: &str, body: &[u8], secret: &[u8]) -> Result<()> {
    let hex_sig = header_value
        .strip_prefix("sha256=")
        .ok_or_else(|| anyhow!("`X-Tengu-Signature` must start with `sha256=`"))?
        .trim();
    let expected_bytes =
        hex_decode(hex_sig).map_err(|e| anyhow!("`X-Tengu-Signature` hex decode failed: {}", e))?;
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| {
        anyhow!("HMAC key length invalid (this is a bug — Hmac<Sha256> accepts any length)")
    })?;
    mac.update(body);
    mac.verify_slice(&expected_bytes)
        .map_err(|_| anyhow!("HMAC mismatch"))?;
    Ok(())
}

/// Tiny hex decoder — avoids pulling `hex` as a dep just for this. Lower-
/// case and upper-case both accepted; whitespace is rejected.
fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err(format!("odd hex length: {}", s.len()));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let hi = decode_nibble(chunk[0])?;
        let lo = decode_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn decode_nibble(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        other => Err(format!("non-hex byte 0x{:02x}", other)),
    }
}

/// `Escalator` for decision loops: a low-confidence step becomes a one-shot
/// orchestrator turn on the loop's session id (planner → subagents), exactly
/// like a planner-bound webhook. Fire-and-forget.
struct OrchestratorEscalator {
    config: Config,
    memory_manager: Arc<MemoryManager>,
    trace: TraceSlot,
}

#[async_trait]
impl Escalator for OrchestratorEscalator {
    async fn escalate(&self, session_id: String, message: String) {
        let config = self.config.clone();
        let memory_manager = Arc::clone(&self.memory_manager);
        // Recorded under what the loop event runs caused by (its
        // `loop.started`, a decide run's root), read before the spawn.
        let parent = trace_exec::cause().and_then(|c| c.parent);
        let trace = self
            .trace
            .get()
            .and_then(|sink| OrchestratorTrace::of(sink, parent));
        tokio::spawn(async move {
            run_one_shot(&config, &memory_manager, &session_id, message, trace).await;
        });
    }
}

/// Per-turn `ChatServiceFactory` impl that rebuilds the engine + tools +
/// system prompt from `Config.agents` for each `agent_name` it's asked to
/// run. Mirrors `EvalChatServiceFactory` in `adapters/inbound/eval.rs`. Used by the
/// webhook listener because webhook turns are stateless one-shots — there's
/// no pre-built per-agent state to read from (unlike chat/telegram, which
/// keep `OrchestratorSnapshots`).
///
/// The orchestrator agent (`Config.orchestrator.agent`, e.g. `crypto_researcher`)
/// gets NO tools so the planner LLM emits plan JSON, not direct tool calls.
/// Every other dispatched agent gets its full tool surface — the same one
/// it has in chat/telegram.
struct WebhookChatServiceFactory {
    cfg: Arc<Config>,
}

#[async_trait]
impl ChatServiceFactory for WebhookChatServiceFactory {
    async fn run_turn(&self, agent_name: &str, text: &str) -> Result<String> {
        let agent = self
            .cfg
            .agents
            .get(agent_name)
            .ok_or_else(|| anyhow!("unknown agent in webhook turn: {}", agent_name))?;

        let engine_box = build_engine(agent_name, agent, self.cfg.claude_code.as_ref())?;
        let engine: Arc<dyn Engine> = Arc::from(engine_box);

        // The turn's one workspace — executor, skills and engine (a Claude
        // Code CLI's cwd, its bridge): the agent's (`~` expanded — a literal
        // `~` would be joined into runtime paths like
        // `<workspace>/.tengu/cache.db`), else a temp dir for this turn,
        // removed after it (`_turn_dir`), as a `run-agent` step — never the
        // server's cwd.
        let (workspace_path, _turn_dir): (PathBuf, _) =
            crate::bootstrap::tools::workspace_or_temp(agent.workspace.as_deref(), "tengu-turn-")?;

        // The process's vault values (never prompts): tool output reaches the
        // model redacted, as in chat, `run-agent` and the bridge.
        let secret_registry = Arc::new(
            crate::adapters::outbound::secrets::process_secret_registry(None),
        );
        let log_activity: Arc<dyn ToolActivityPort> = Arc::new(NoopActivity);

        // Orchestrator agent gets NO tools — its job is to emit JSON only.
        // Doctrinal: LLM = heart, Open Brain + LLM Wiki = brain, tools =
        // hands. The planner call must not have hands. Mirrors
        // `EvalChatServiceFactory`.
        let is_orchestrator_agent = self
            .cfg
            .orchestrator
            .as_ref()
            .map(|o| o.agent == agent_name)
            .unwrap_or(false);

        let base_tools = if is_orchestrator_agent {
            Vec::new()
        } else {
            // The agent's `tools` list, as on every surface.
            crate::bootstrap::tools::agent_base_tools(
                agent, true, false, // memory tools off for webhook one-shots
            )
        };

        let skill_source = FileSystemSkillSource::new(workspace_path.clone());
        let base_reserved: Vec<String> = base_tools.iter().map(|t| t.name.clone()).collect();
        let mut skill_registry = SkillRegistry::new(base_reserved)
            .with_allowlist(Some(agent.skill_packages.clone()))
            .with_shell_skills(!agent.no_shell_fallback);
        skill_registry.reload(&skill_source);

        let current_tools = if is_orchestrator_agent {
            Vec::new()
        } else {
            crate::bootstrap::tools::rebuild_tools(&base_tools, &skill_registry)
        };
        let system_prompt = crate::bootstrap::tools::rebuild_system_prompt(
            agent,
            true,
            &skill_registry,
            &current_tools,
        );

        let mut tool_defs = current_tools.clone();
        let inner_executor: Arc<dyn ToolExecutor> =
            match crate::bootstrap::tools::build_tool_executor(
                &workspace_path,
                &current_tools,
                &skill_registry,
                &None,
                &secret_registry,
                log_activity,
                None,
                Some(&self.cfg.memory),
                agent,
                &self.cfg.mcp_servers,
            ) {
                Some(executor) => {
                    let extra = executor.additional_tool_defs(&tool_defs);
                    if !extra.is_empty() {
                        tool_defs.extend(extra);
                    }
                    Arc::new(
                        crate::adapters::outbound::secrets::SanitizedToolExecutor::new(
                            Arc::new(executor),
                            Arc::clone(&secret_registry),
                        ),
                    ) as Arc<dyn ToolExecutor>
                }
                None => Arc::new(NoopRuntimeToolExecutor) as Arc<dyn ToolExecutor>,
            };

        let messages = vec![
            Message {
                role: Role::System,
                content: system_prompt.clone(),
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                role: Role::User,
                content: text.to_string(),
                tool_call_id: None,
                tool_calls: None,
            },
        ];

        let (bridge_tools, mcp_servers) = crate::bootstrap::tools::bridge_inputs(
            engine.manages_own_workspace(),
            &tool_defs,
            &self.cfg.mcp_servers,
        );
        let engine_context = EngineContext {
            workspace: Some(workspace_path.clone()),
            system_prompt: Some(system_prompt.clone()),
            bridge_tools,
            max_tool_rounds: Some(agent.limits.max_tool_rounds),
            max_mcp_result_chars: Some(agent.limits.max_mcp_result_chars),
            mcp_servers,
        };

        let response = collect_engine_response(
            &*engine,
            &messages,
            &tool_defs,
            &engine_context,
            Some(&*inner_executor),
            None,
            None,
            None,
            agent.limits.max_tool_rounds,
            agent.limits.max_tool_result_chars,
            agent.limits.stream_event_timeout_secs,
            agent.limits.compact_result_limit,
        )
        .await?;

        Ok(response.text)
    }
}

// `NoopActivity` + `NoopRuntimeToolExecutor` are shared with `inbound::eval`
// via `adapters::noop`, and the Claude Code bridge inputs with it via
// `bootstrap::tools::bridge_inputs`. Webhooks and evals both rebuild
// per-turn services from config and need identical fallback impls.

#[cfg(test)]
mod tests {
    use super::*;

    /// Compute the expected signature for a body+secret. Used by tests
    /// to build inputs for `verify_hmac`.
    fn sign(body: &[u8], secret: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret).expect("hmac");
        mac.update(body);
        let bytes = mac.finalize().into_bytes();
        let mut hex = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            hex.push_str(&format!("{:02x}", b));
        }
        format!("sha256={}", hex)
    }

    #[test]
    fn verify_accepts_correct_signature() {
        let body = b"{\"action\":\"opened\",\"pr\":42}";
        let secret = b"abc123";
        let sig = sign(body, secret);
        assert!(verify_hmac(&sig, body, secret).is_ok());
    }

    #[test]
    fn verify_rejects_tampered_body() {
        let body = b"{\"action\":\"opened\"}";
        let secret = b"abc123";
        let sig = sign(body, secret);
        let tampered = b"{\"action\":\"closed\"}";
        let err = verify_hmac(&sig, tampered, secret).unwrap_err();
        assert!(err.to_string().contains("HMAC mismatch"));
    }

    #[test]
    fn verify_rejects_wrong_secret() {
        let body = b"hello";
        let sig = sign(body, b"correct");
        let err = verify_hmac(&sig, body, b"wrong").unwrap_err();
        assert!(err.to_string().contains("HMAC mismatch"));
    }

    #[test]
    fn verify_rejects_missing_prefix() {
        let body = b"hello";
        let sig_no_prefix = sign(body, b"k").trim_start_matches("sha256=").to_string();
        let err = verify_hmac(&sig_no_prefix, body, b"k").unwrap_err();
        assert!(err.to_string().contains("must start with `sha256=`"));
    }

    #[test]
    fn verify_rejects_malformed_hex() {
        let err = verify_hmac("sha256=zzzz", b"x", b"k").unwrap_err();
        assert!(err.to_string().contains("hex decode failed"));
    }

    #[test]
    fn validate_endpoints_requires_one_secret_source() {
        let mut endpoints = HashMap::new();
        endpoints.insert(
            "test".to_string(),
            WebhookEndpointConfig {
                agent: "main".to_string(),
                secret_env: None,
                secret: None,
                goal_template: "x".to_string(),
                decision_loop: None,
                auth_header_env: None,
            },
        );
        let err = validate_endpoints(&endpoints, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("neither"));
    }

    #[test]
    fn validate_endpoints_rejects_both_secret_sources() {
        let mut endpoints = HashMap::new();
        endpoints.insert(
            "test".to_string(),
            WebhookEndpointConfig {
                agent: "main".to_string(),
                secret_env: Some("X".to_string()),
                secret: Some("y".to_string()),
                goal_template: "x".to_string(),
                decision_loop: None,
                auth_header_env: None,
            },
        );
        let err = validate_endpoints(&endpoints, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("both"));
    }

    #[test]
    fn validate_endpoints_rejects_empty_agent() {
        let mut endpoints = HashMap::new();
        endpoints.insert(
            "test".to_string(),
            WebhookEndpointConfig {
                agent: String::new(),
                secret_env: Some("X".to_string()),
                secret: None,
                goal_template: "x".to_string(),
                decision_loop: None,
                auth_header_env: None,
            },
        );
        let err = validate_endpoints(&endpoints, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("agent"));
    }

    fn loop_endpoint(
        auth_header_env: Option<&str>,
        secret_env: Option<&str>,
    ) -> WebhookEndpointConfig {
        WebhookEndpointConfig {
            agent: String::new(),
            secret_env: secret_env.map(str::to_string),
            secret: None,
            goal_template: "x".to_string(),
            decision_loop: Some("watch".to_string()),
            auth_header_env: auth_header_env.map(str::to_string),
        }
    }

    #[test]
    fn validate_endpoints_loop_endpoint_needs_no_agent_but_a_known_loop() {
        let endpoints = HashMap::from([("h".to_string(), loop_endpoint(Some("H"), None))]);
        let err = validate_endpoints(&endpoints, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("decision_loops.watch"), "{err}");
        let loops = HashMap::from([(
            "watch".to_string(),
            toml::from_str("goal=\"g\"\nagent=\"a\"\n[actions.hold]\ndescription=\"n\"").unwrap(),
        )]);
        validate_endpoints(&endpoints, &loops).unwrap();
    }

    /// A loop-endpoint config: `[webhooks.endpoints.h] loop = "watch"`,
    /// inline HMAC secret `k`.
    fn loop_config() -> Config {
        let mut config = Config::default();
        config.webhooks.enabled = true;
        config.webhooks.endpoints.insert(
            "h".to_string(),
            WebhookEndpointConfig {
                secret: Some("k".to_string()),
                ..loop_endpoint(None, None)
            },
        );
        config.decision_loops.insert(
            "watch".to_string(),
            toml::from_str("goal=\"g\"\nagent=\"main\"\n[actions.hold]\ndescription=\"n\"")
                .unwrap(),
        );
        config
    }

    fn slow_dispatch(
        loops: &[&str],
    ) -> (
        Arc<LoopDispatch>,
        Arc<crate::application::runtime::loops::tests::SlowLoop>,
    ) {
        slow_dispatch_with(loops, 1, 64)
    }

    fn slow_dispatch_with(
        loops: &[&str],
        ms: u64,
        max_queued: usize,
    ) -> (
        Arc<LoopDispatch>,
        Arc<crate::application::runtime::loops::tests::SlowLoop>,
    ) {
        let slow = crate::application::runtime::loops::tests::SlowLoop::new(ms);
        let handlers = loops
            .iter()
            .map(|n| (n.to_string(), Arc::clone(&slow) as Arc<dyn LoopHandler>))
            .collect();
        (Arc::new(LoopDispatch::new(handlers, 4, max_queued)), slow)
    }

    /// W1-gate review: a burst past `[runtime] max_queued_per_loop` gets 429
    /// (the dispatch warns); what was accepted still runs.
    #[tokio::test]
    async fn loop_endpoint_429s_past_the_queue_bound() {
        let (loops, slow) = slow_dispatch_with(&["watch"], 100, 1);
        let state = app_state(
            loop_config(),
            Arc::new(MemoryManager::new()),
            Arc::clone(&loops),
            Arc::new(SecretRegistry::new()),
            Arc::new(crate::adapters::outbound::noop::NoopTrace),
        )
        .unwrap();
        assert_eq!(
            post_signed(&state, b"{\"n\":1}").await,
            StatusCode::ACCEPTED
        );
        for _ in 0..200 {
            if loops.stats()["watch"].in_flight == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert_eq!(
            post_signed(&state, b"{\"n\":2}").await,
            StatusCode::ACCEPTED
        );
        assert_eq!(
            post_signed(&state, b"{\"n\":3}").await,
            StatusCode::TOO_MANY_REQUESTS
        );
        let st = loops.stats()["watch"].clone();
        assert_eq!((st.accepted, st.queued, st.dropped), (2, 1, 1));
        for _ in 0..200 {
            if loops.stats()["watch"].completed == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(slow.seen.lock().unwrap().len(), 2);
    }

    async fn post_signed(state: &Arc<WebhookAppState>, body: &'static [u8]) -> StatusCode {
        let mut headers = HeaderMap::new();
        headers.insert(SIG_HEADER, sign(body, b"k").parse().unwrap());
        dispatch_webhook(
            Path("h".to_string()),
            State(Arc::clone(state)),
            headers,
            Bytes::from_static(body),
        )
        .await
        .into_response()
        .status()
    }

    #[tokio::test]
    async fn loop_endpoint_queues_on_the_dispatch_and_503s_while_draining() {
        let (loops, slow) = slow_dispatch(&["watch"]);
        let state = app_state(
            loop_config(),
            Arc::new(MemoryManager::new()),
            Arc::clone(&loops),
            Arc::new(SecretRegistry::new()),
            Arc::new(crate::adapters::outbound::noop::NoopTrace),
        )
        .unwrap();
        assert_eq!(
            post_signed(&state, b"{\"n\":1}").await,
            StatusCode::ACCEPTED
        );
        for _ in 0..200 {
            if loops.stats()["watch"].completed == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let seen = slow.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].starts_with("webhook-h-"), "{seen:?}");
        loops.drain(tokio::time::Instant::now()).await;
        assert_eq!(
            post_signed(&state, b"{\"n\":2}").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    /// Each request to a configured endpoint: a `trigger.webhook` root, its
    /// loop events under it (one session), a `webhook.responded` with the
    /// code; a refused one too (401, refused); an unknown endpoint nothing.
    /// The body never reaches the trace (its size does).
    #[tokio::test]
    async fn webhook_requests_record_a_root_their_work_and_the_answer() {
        use crate::application::trace_exec::tests::MemTrace;
        let sink = Arc::new(MemTrace::default());
        let slow = crate::application::runtime::loops::tests::SlowLoop::new(1);
        let loops = Arc::new(
            LoopDispatch::new(
                BTreeMap::from([(
                    "watch".to_string(),
                    Arc::clone(&slow) as Arc<dyn LoopHandler>,
                )]),
                4,
                64,
            )
            .with_trace(sink.clone()),
        );
        let state = app_state(
            loop_config(),
            Arc::new(MemoryManager::new()),
            Arc::clone(&loops),
            Arc::new(SecretRegistry::new()),
            sink.clone(),
        )
        .unwrap();
        let body: &'static [u8] = b"{\"marker\":\"body-text-never-recorded\"}";
        assert_eq!(post_signed(&state, body).await, StatusCode::ACCEPTED);
        for _ in 0..200 {
            if loops.stats()["watch"].completed == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let mut bad = HeaderMap::new();
        bad.insert(SIG_HEADER, sign(body, b"wrong").parse().unwrap());
        let refused = dispatch_webhook(
            Path("h".to_string()),
            State(Arc::clone(&state)),
            bad,
            Bytes::from_static(body),
        )
        .await
        .into_response()
        .status();
        assert_eq!(refused, StatusCode::UNAUTHORIZED);
        let unknown = dispatch_webhook(
            Path("nope".to_string()),
            State(Arc::clone(&state)),
            HeaderMap::new(),
            Bytes::from_static(body),
        )
        .await
        .into_response()
        .status();
        assert_eq!(unknown, StatusCode::NOT_FOUND);

        let d = sink.all();
        let session = d[0].session_id.clone().unwrap();
        assert!(session.starts_with("webhook-h-"), "{session}");
        let first: Vec<(&str, Option<&str>)> = d
            .iter()
            .filter(|e| e.session_id.as_deref() == Some(session.as_str()))
            .map(|e| (e.kind.as_str(), e.parent_event_id.as_deref()))
            .collect();
        let root = MemTrace::id(0);
        assert_eq!(first[0], ("trigger.webhook", None), "{first:?}");
        assert!(
            first.contains(&("loop.queued", Some(root.as_str()))),
            "{first:?}"
        );
        assert!(
            first.contains(&("webhook.responded", Some(root.as_str()))),
            "{first:?}"
        );
        for k in ["loop.started", "loop.completed"] {
            assert!(first.iter().any(|(kind, _)| *kind == k), "{k}: {first:?}");
        }
        assert_eq!(d[0].node_id.as_deref(), Some("trigger:webhook/h"));
        assert_eq!(d[0].status, Status::Running);
        assert_eq!(d[0].component, Component::Loop);
        assert_eq!(d[0].payload["kind"], json!("loop"));
        assert_eq!(d[0].payload["loop"], json!("watch"));
        assert_eq!(d[0].payload["body_bytes"], json!(body.len()));
        let answer = d
            .iter()
            .find(|e| e.kind == "webhook.responded" && e.session_id.as_deref() == Some(&session))
            .unwrap();
        assert_eq!(
            (answer.status, &answer.payload["status_code"]),
            (Status::Ok, &json!(202))
        );
        assert!(answer.duration_ms.is_some());

        // The refused request: its own session, root + 401 refused.
        let other: Vec<&EventDraft> = d
            .iter()
            .filter(|e| e.session_id.as_deref() != Some(session.as_str()))
            .collect();
        assert_eq!(
            other
                .iter()
                .map(|e| (e.kind.as_str(), e.status))
                .collect::<Vec<_>>(),
            [
                ("trigger.webhook", Status::Running),
                ("webhook.responded", Status::Refused)
            ]
        );
        assert_eq!(other[1].payload["status_code"], json!(401));
        assert_eq!(other[1].payload["error"], json!("HMAC mismatch"));
        assert!(
            d.iter()
                .all(|e| e.node_id.as_deref() != Some("trigger:webhook/nope")),
            "an unknown endpoint is not recorded"
        );
        let text =
            serde_json::to_string(&d.iter().map(|e| &e.payload).collect::<Vec<_>>()).unwrap();
        assert!(!text.contains("body-text-never-recorded"), "{text}");
        assert!(!text.contains("sha256="), "no header: {text}");
    }

    /// The live-check fixture (`tests/fixtures/webhooks/trace.toml`) loads
    /// and passes the listener's checks: a loop endpoint, a planner one; its
    /// models are loopback only.
    #[test]
    fn trace_fixture_config_is_served() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/webhooks/trace.toml");
        let config = Config::load(&path).unwrap();
        check_config(&config).unwrap();
        let ep = &config.webhooks.endpoints["probe"];
        assert_eq!(ep.decision_loop.as_deref(), Some("probe"));
        assert_eq!(bind_addr(&config).unwrap().to_string(), "127.0.0.1:7391");
        assert_eq!(config.webhooks.endpoints["ask"].agent, "planner");
        assert!(config.orchestrator.is_some());
        for a in ["planner", "reader"] {
            let base = &config.agents[a].local.as_ref().unwrap().base_url;
            assert!(base.starts_with("http://127.0.0.1:"), "{a}: {base}");
        }
    }

    #[test]
    fn app_state_refuses_an_endpoint_whose_loop_is_not_running() {
        let (loops, _) = slow_dispatch(&["other"]);
        let err = app_state(
            loop_config(),
            Arc::new(MemoryManager::new()),
            loops,
            Arc::new(SecretRegistry::new()),
            Arc::new(crate::adapters::outbound::noop::NoopTrace),
        )
        .err()
        .unwrap();
        assert!(
            err.to_string().contains("loop `watch` is not running"),
            "{err}"
        );
    }

    #[test]
    fn validate_endpoints_rejects_header_plus_hmac() {
        let endpoints = HashMap::from([("h".to_string(), loop_endpoint(Some("H"), Some("S")))]);
        let err = validate_endpoints(&endpoints, &HashMap::new()).unwrap_err();
        assert!(err.to_string().contains("exactly one"), "{err}");
    }

    #[test]
    fn auth_header_compare() {
        let mut h = HeaderMap::new();
        assert!(verify_auth_header(&h, "Bearer abc").is_err());
        h.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer abc".parse().unwrap(),
        );
        assert!(verify_auth_header(&h, "Bearer abc").is_ok());
        assert!(verify_auth_header(&h, "Bearer abd").is_err());
        assert!(verify_auth_header(&h, "Bearer abcd").is_err());
    }

    #[test]
    fn hex_decode_round_trip() {
        let bytes = vec![0x00, 0x01, 0xab, 0xcd, 0xff];
        let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
        let decoded = hex_decode(&hex).unwrap();
        assert_eq!(decoded, bytes);
        // Mixed case accepted.
        let mixed = "AbCd";
        assert_eq!(hex_decode(mixed).unwrap(), vec![0xab, 0xcd]);
    }
}
