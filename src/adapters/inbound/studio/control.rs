//! Studio control — Play / Stop / send-event on the sandbox's runtime
//! (`TENGU_STUDIO_PLAN.md` § 6, ST-30 / ST-31). The rules (which action now,
//! or why not) are `application::studio::control`; this file does the IO
//! with the runtime `tengu run` starts — never a copy of it.
//!
//! | Action | Does |
//! |---|---|
//! | Play | `inbound::run::start_session` in this process: the same leases (`runtime:<sandbox>` in `<state dir>/runtime.db`), recording (`RunKind::Run`), loops, feeds, heartbeat and webhook routes as `tengu run`. Another process holds the lease (`bootstrap::runtime::LeaseHeld`) ⇒ refused, Studio attaches read-only |
//! | Stop | `Stopper::stop("studio stop")` — the path SIGINT takes in `tengu run`: the bounded drain (`[runtime] shutdown_grace_secs`), `stopping` → `stopped` beats, leases released. Only a runtime this Studio started |
//! | event | one scenario (`<sandbox dir>/scenarios/<name>.json`, by name — the page never sends an event body) into the owned runtime's `LoopDispatch`, session `studio-<scenario>-<uuid>` |
//! | Studio stops (SIGINT / SIGTERM) | [`Controller::shutdown`]: no new Play, the owned runtime drained first, then `studio.stopped` |
//!
//! Studio's own recording (`RunKind::Studio`, `<TENGU_HOME>/logs/trace/<sandbox>/<run_id>.jsonl`,
//! only with control on; node `runtime:<sandbox>`):
//!
//! | Kind | Status | When |
//! |---|---|---|
//! | `studio.started` | `running` | serving with control on: the policy, scenarios, loops (never the token) |
//! | `studio.control` | `pending` | a request: `action`, `scenario`, `loop` |
//! | `studio.control` | `ok` · `refused` · `failed` | its verdict (child of the request): Play running (holder, `run_id`) / start failed (the error) / lease held (holder, seconds left: attached); Stop requested; event queued (`session_id`) or refused (`queue_full`, …) |
//! | `studio.runtime` | `ok` · `failed` | the runtime this Studio started ended, any cause (Stop, a lost lease, a task that died): reason, drain, `lease_released` (child of the Play verdict) |
//! | `studio.stopped` | `ok` | Studio's last line (closes the run) |
//!
//! No orphan by construction: the runtime runs in this process — a crashed
//! Studio takes it along, and its lease frees after the TTL (30 s).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::Result;
use axum::http::StatusCode;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio::time::Instant;
use tracing::{info, warn};

use super::super::run::{start_session, RunSession};
use crate::adapters::outbound::noop::NoopTrace;
use crate::application::runtime::loops::{LoopDispatch, Refused as LoopRefused};
use crate::application::runtime::Stopper;
use crate::application::studio::control::{
    self as rules, pick_loop, Action, ControlView, Phase, Refusal, Scenario, Seen, SeenFn,
};
use crate::bootstrap::runtime::{LeaseHeld, ShutdownReport};
use crate::bootstrap::studio::StudioContext;
use crate::config::studio::ControlPolicy;
use crate::config::Config;
use crate::domain::observation::now_ms;
use crate::domain::secrets::SecretRegistry;
use crate::domain::trace::{Component, EventDraft, Status, Tone};
use crate::domain::workflow::node_id;
use crate::ports::trace::TraceSink;

/// Starts the runtime: `run::start_session` in production, a temp state
/// dir in tests.
pub(crate) type StartFuture = Pin<Box<dyn Future<Output = Result<RunSession>> + Send>>;
pub(crate) type Starter = Arc<dyn Fn() -> StartFuture + Send + Sync>;

/// Stop waits this long past `[runtime] shutdown_grace_secs` for the drain
/// before it answers `stopping` (202).
pub(crate) const STOP_MARGIN: Duration = Duration::from_secs(5);
/// How long Studio's shutdown waits for a Play still starting.
const START_WAIT: Duration = Duration::from_secs(60);

/// `tengu run`'s start, for `config` (the one Studio validated and serves).
pub(crate) fn runtime_starter(config: Config, secrets: Arc<SecretRegistry>) -> Starter {
    let config = Arc::new(config);
    Arc::new(move || {
        let (config, secrets) = (Arc::clone(&config), Arc::clone(&secrets));
        Box::pin(async move { start_session(&config, secrets).await })
    })
}

/// The runtime this Studio started.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RunRef {
    /// The lease holder `<host>:<pid>:<uuid>` = its recording's `runtime_id`.
    pub holder: String,
    pub run_id: Option<String>,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    /// How it ended (`studio.runtime` payload).
    pub end: Option<Value>,
}

/// A control request's verdict (the last one is served).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Outcome {
    pub action: Action,
    /// `ok` · `refused` · `failed`.
    pub status: Status,
    /// `status`'s tone (`Status::tone`): the page draws it, never maps
    /// a status itself.
    pub tone: Tone,
    pub detail: String,
    pub at_ms: i64,
    /// The verdict's event in Studio's recording.
    pub event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scenario: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// What a POST answers: the HTTP status, the verdict, extra facts.
pub(crate) struct Verdict {
    pub code: StatusCode,
    pub outcome: Outcome,
    pub extra: Value,
}

/// One control request in flight: its pending event, timed from receipt.
struct Req {
    id: Option<String>,
    action: Action,
    scenario: Option<String>,
    t0: Instant,
}

struct Owned {
    run: RunRef,
    stopper: Stopper,
    loops: Arc<LoopDispatch>,
    /// `true` once the waiter saw the runtime end.
    ended: watch::Receiver<bool>,
}

#[derive(Default)]
struct Inner {
    phase: Phase,
    own: Option<Owned>,
    /// The latest runtime this Studio started (kept after it ended).
    last_run: Option<RunRef>,
    last: Option<Outcome>,
    /// Studio is stopping: no new Play.
    closing: bool,
}

/// Studio's control (module table): one per `tengu studio` process.
pub(crate) struct Controller {
    policy: ControlPolicy,
    node: String,
    starter: Option<Starter>,
    seen: SeenFn,
    trace: Arc<dyn TraceSink>,
    scenarios: Vec<Scenario>,
    loops: Vec<String>,
    stop_wait: Duration,
    inner: Mutex<Inner>,
}

impl Controller {
    /// Control on: `starter` starts the runtime, `trace` = Studio's own
    /// recording.
    pub(crate) fn new(
        policy: ControlPolicy,
        ctx: &StudioContext,
        starter: Starter,
        trace: Arc<dyn TraceSink>,
    ) -> Self {
        let mut c = Self::read_only(policy, ctx);
        c.starter = Some(starter);
        c.trace = trace;
        c
    }

    /// Control off (`policy.enabled` false): every action refused with the
    /// policy's reason; nothing recorded.
    pub(crate) fn read_only(policy: ControlPolicy, ctx: &StudioContext) -> Self {
        Self {
            policy,
            node: node_id::runtime(&ctx.sandbox),
            starter: None,
            seen: ctx.seen_fn(),
            trace: Arc::new(NoopTrace),
            scenarios: ctx.scenarios.clone(),
            loops: ctx.config.decision_loops.keys().cloned().collect(),
            stop_wait: Duration::from_secs(ctx.config.runtime.shutdown_grace_secs) + STOP_MARGIN,
            inner: Mutex::new(Inner::default()),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.policy.enabled && self.starter.is_some()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One event in Studio's recording (node `runtime:<sandbox>`).
    fn emit(
        &self,
        kind: &str,
        status: Status,
        payload: Value,
        parent: Option<&str>,
        session: Option<&str>,
        duration_ms: Option<u64>,
    ) -> Option<String> {
        let mut d = EventDraft::new(Component::Studio, kind, status)
            .node(self.node.clone())
            .payload(payload);
        d.parent_event_id = parent.map(str::to_string);
        d.session_id = session.map(str::to_string);
        d.duration_ms = duration_ms;
        self.trace.emit(d)
    }

    /// `studio.started`: control on, serving. Never the token.
    pub(crate) fn started(&self, addr: &str) {
        if !self.enabled() {
            return;
        }
        let scenarios: Vec<&str> = self.scenarios.iter().map(|s| s.name.as_str()).collect();
        self.emit(
            "studio.started",
            Status::Running,
            json!({
                "pid": std::process::id(),
                "addr": addr,
                "control": self.policy.why,
                "scenarios": scenarios,
                "loops": self.loops,
            }),
            None,
            None,
            None,
        );
    }

    /// The heartbeat now (`run-<sandbox>.json`).
    fn seen(&self) -> Option<Seen> {
        (self.seen)()
    }

    fn rule_view(&self, inner: &Inner, seen: Option<&Seen>) -> ControlView {
        let own = inner.own.as_ref().map(|o| o.run.holder.as_str());
        rules::view(&self.policy, inner.phase, own, seen)
    }

    /// `GET /api/v1/control`: the rule view (state, tone, each action ok or
    /// why not) + the runtime this Studio runs, the heartbeat seen, the
    /// scenarios, the loops, the last verdict, Studio's run.
    pub(crate) fn view(&self) -> Value {
        let seen = self.seen();
        let inner = self.lock();
        let v = self.rule_view(&inner, seen.as_ref());
        json!({
            "enabled": v.enabled,
            "why": v.why,
            "state": v.state,
            "tone": v.tone,
            "actions": v.actions,
            "closing": inner.closing,
            "own": inner.own.as_ref().map(|o| &o.run),
            "last_run": inner.last_run,
            "seen": seen,
            "scenarios": self.scenarios,
            "loops": self.loops,
            "last": inner.last,
            "studio_run_id": self.trace.run_id(),
        })
    }

    /// A request: its `studio.control` pending event (`action`, `scenario`
    /// and `payload`'s fields), timed from now.
    fn request(&self, action: Action, scenario: Option<&str>, payload: Value) -> Req {
        let mut p = json!({"action": action.as_str(), "scenario": scenario});
        if let (Value::Object(o), Value::Object(extra)) = (&mut p, payload) {
            o.extend(extra);
        }
        Req {
            id: self.emit("studio.control", Status::Pending, p, None, None, None),
            action,
            scenario: scenario.map(str::to_string),
            t0: Instant::now(),
        }
    }

    /// The verdict of `req`: recorded (its child), kept as `last`, answered.
    fn verdict(
        &self,
        req: &Req,
        status: Status,
        code: StatusCode,
        detail: String,
        extra: Value,
        session: Option<&str>,
    ) -> Verdict {
        let mut payload = json!({
            "action": req.action.as_str(),
            "scenario": req.scenario,
            "detail": detail,
        });
        if let (Value::Object(o), Value::Object(x)) = (&mut payload, extra.clone()) {
            o.extend(x);
        }
        let event_id = self.emit(
            "studio.control",
            status,
            payload,
            req.id.as_deref(),
            session,
            Some(req.t0.elapsed().as_millis() as u64),
        );
        let outcome = Outcome {
            action: req.action,
            status,
            tone: status.tone(),
            detail,
            at_ms: now_ms(),
            event_id,
            scenario: req.scenario.clone(),
            session_id: session.map(str::to_string),
        };
        let action = req.action.as_str();
        match status {
            Status::Ok => info!(action, detail = %outcome.detail, "studio control"),
            _ => warn!(action, ?status, detail = %outcome.detail, "studio control"),
        }
        self.lock().last = Some(outcome.clone());
        Verdict {
            code,
            outcome,
            extra,
        }
    }

    /// A rule refusal: 403 control off, 409 not in this state.
    fn refused(&self, req: &Req, r: Refusal) -> Verdict {
        let code = match r {
            Refusal::Off(_) => StatusCode::FORBIDDEN,
            Refusal::Conflict(_) => StatusCode::CONFLICT,
        };
        let why = r.why().to_string();
        self.verdict(req, Status::Refused, code, why, json!({}), None)
    }

    /// 422: a scenario or loop this sandbox does not have.
    fn unprocessable(&self, req: &Req, why: String) -> Verdict {
        let code = StatusCode::UNPROCESSABLE_ENTITY;
        self.verdict(req, Status::Refused, code, why, json!({}), None)
    }

    fn scenario(&self, name: &str) -> Result<&Scenario, String> {
        self.scenarios.iter().find(|s| s.name == name).ok_or_else(|| {
            let names: Vec<&str> = self.scenarios.iter().map(|s| s.name.as_str()).collect();
            let names = if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            };
            format!("no scenario `{name}` (scenarios/<name>.json beside the sandbox config: {names})")
        })
    }

    /// Play (module table). `scenario`: also send that event once running.
    /// Runs on its own task: a client that disconnects mid-start (the HTTP
    /// handler dropped) never leaves a half-started runtime — the start
    /// finishes and the runtime is Studio's, to Stop.
    pub(crate) async fn play(self: &Arc<Self>, scenario: Option<String>) -> Verdict {
        let me = Arc::clone(self);
        match tokio::spawn(async move { me.play_now(scenario).await }).await {
            Ok(v) => v,
            Err(e) => {
                let mut inner = self.lock();
                if inner.phase == Phase::Starting {
                    inner.phase = Phase::Failed;
                }
                drop(inner);
                let req = self.request(Action::Play, None, json!({}));
                let code = StatusCode::INTERNAL_SERVER_ERROR;
                let detail = format!("play task failed: {e}");
                self.verdict(&req, Status::Failed, code, detail, json!({}), None)
            }
        }
    }

    async fn play_now(self: Arc<Self>, scenario: Option<String>) -> Verdict {
        let req = self.request(Action::Play, scenario.as_deref(), json!({}));
        if let Some(name) = scenario.as_deref() {
            if let Err(why) = self.scenario(name) {
                return self.unprocessable(&req, why);
            }
        }
        let seen = self.seen();
        let before = {
            let mut inner = self.lock();
            let st = self.rule_view(&inner, seen.as_ref()).state;
            let verdict = if inner.closing {
                Err(Refusal::Conflict(
                    "Studio is stopping: no new runtime".to_string(),
                ))
            } else {
                rules::check(Action::Play, &self.policy, st, seen.as_ref())
            };
            if let Err(r) = verdict {
                drop(inner);
                return self.refused(&req, r);
            }
            std::mem::replace(&mut inner.phase, Phase::Starting)
        };
        let Some(starter) = self.starter.clone() else {
            self.lock().phase = before;
            let r = Refusal::Off(format!("control {}", self.policy.why));
            return self.refused(&req, r);
        };
        let session = match starter().await {
            Ok(session) => session,
            Err(e) => return self.start_failed(&req, before, &e),
        };
        let mut v = self.running(session, &req);
        if let Some(name) = scenario {
            let sent = self.event(&name, None).await;
            v.extra["event"] = json!({"code": sent.code.as_u16(), "outcome": sent.outcome});
        }
        v
    }

    /// Play's start failed: another process holds the lease (409, attach
    /// read-only, the phase as it was) or anything else (500, `failed`).
    fn start_failed(&self, req: &Req, before: Phase, e: &anyhow::Error) -> Verdict {
        if let Some(held) = LeaseHeld::of(e) {
            self.lock().phase = before;
            let extra = json!({
                "attached": true,
                "holder": held.holder,
                "resource": held.resource,
                "remaining_secs": held.remaining_secs,
            });
            let detail = format!("attached read-only: {held}");
            return self.verdict(
                req,
                Status::Refused,
                StatusCode::CONFLICT,
                detail,
                extra,
                None,
            );
        }
        self.lock().phase = Phase::Failed;
        let code = StatusCode::INTERNAL_SERVER_ERROR;
        let detail = format!("start failed: {e:#}");
        self.verdict(req, Status::Failed, code, detail, json!({}), None)
    }

    /// Our runtime started: phase `running`, the waiter that sees it end.
    fn running(self: &Arc<Self>, session: RunSession, req: &Req) -> Verdict {
        let run = RunRef {
            holder: session.holder().to_string(),
            run_id: session.run_id(),
            started_ms: now_ms(),
            ended_ms: None,
            end: None,
        };
        let (done_tx, done_rx) = watch::channel(false);
        {
            let mut inner = self.lock();
            inner.phase = Phase::Running;
            inner.last_run = Some(run.clone());
            inner.own = Some(Owned {
                run: run.clone(),
                stopper: session.stopper(),
                loops: session.loops(),
                ended: done_rx,
            });
        }
        let extra = json!({"holder": run.holder, "run_id": run.run_id});
        let detail = format!(
            "running: holder `{}`, run {}",
            run.holder,
            run.run_id.as_deref().unwrap_or("(not recorded)")
        );
        let v = self.verdict(req, Status::Ok, StatusCode::OK, detail, extra, None);
        let ctl = Arc::clone(self);
        let parent = v.outcome.event_id.clone();
        tokio::spawn(async move {
            let report = session.wait_and_shutdown().await;
            ctl.ended(&report, parent.as_deref());
            let _ = done_tx.send(true);
        });
        v
    }

    /// The owned runtime ended (any cause): phase, `last_run`, `studio.runtime`.
    fn ended(&self, report: &ShutdownReport, parent: Option<&str>) {
        let end = json!({
            "reason": report.stop.reason,
            "failed": report.stop.failed,
            "drain": {
                "finished": report.loops.finished,
                "dropped": report.loops.dropped,
                "aborted": report.loops.aborted,
            },
            "aborted_tasks": report.aborted_tasks,
            "lease_released": report.lease_released,
        });
        let run = {
            let mut inner = self.lock();
            inner.phase = if report.stop.failed {
                Phase::Failed
            } else {
                Phase::Stopped
            };
            let mut run = inner.own.take().map(|o| o.run);
            if let Some(r) = run.as_mut() {
                r.ended_ms = Some(now_ms());
                r.end = Some(end.clone());
                inner.last_run = Some(r.clone());
            }
            run
        };
        let mut payload = end;
        if let (Value::Object(o), Some(r)) = (&mut payload, &run) {
            o.insert("holder".into(), json!(r.holder));
            o.insert("run_id".into(), json!(r.run_id));
            if let Some(ms) = r.ended_ms {
                o.insert("ran_ms".into(), json!(ms - r.started_ms));
            }
        }
        let status = if report.stop.failed {
            Status::Failed
        } else {
            Status::Ok
        };
        self.emit("studio.runtime", status, payload, parent, None, None);
    }

    /// Stop (module table): the graceful drain of the runtime this Studio
    /// started; answers once it ended (200), or `stopping` (202) after the
    /// grace + [`STOP_MARGIN`].
    pub(crate) async fn stop(&self) -> Verdict {
        self.stop_as("studio stop").await
    }

    async fn stop_as(&self, reason: &str) -> Verdict {
        let req = self.request(Action::Stop, None, json!({"reason": reason}));
        let seen = self.seen();
        let (stopper, mut ended) = {
            let mut inner = self.lock();
            let st = self.rule_view(&inner, seen.as_ref()).state;
            if let Err(r) = rules::check(Action::Stop, &self.policy, st, seen.as_ref()) {
                drop(inner);
                return self.refused(&req, r);
            }
            inner.phase = Phase::Stopping;
            let own = inner.own.as_ref().expect("running ⇒ owned");
            (own.stopper.clone(), own.ended.clone())
        };
        stopper.stop(reason, false);
        let detail = format!("stop requested ({reason}): draining ≤ the shutdown grace");
        let v = self.verdict(&req, Status::Ok, StatusCode::OK, detail, json!({}), None);
        let done = tokio::time::timeout(self.stop_wait, ended.wait_for(|e| *e)).await;
        let end = self.lock().last_run.as_ref().and_then(|r| r.end.clone());
        Verdict {
            code: if done.is_ok() {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            },
            extra: json!({"end": end}),
            ..v
        }
    }

    /// Send scenario `name` to loop `loop_name` (else the only loop) of the
    /// runtime this Studio runs.
    pub(crate) async fn event(&self, name: &str, loop_name: Option<&str>) -> Verdict {
        let req = self.request(Action::Event, Some(name), json!({"loop": loop_name}));
        let scenario = match self.scenario(name) {
            Ok(s) => s,
            Err(why) => return self.unprocessable(&req, why),
        };
        let target = match pick_loop(loop_name, &self.loops) {
            Ok(l) => l.to_string(),
            Err(why) => return self.unprocessable(&req, why),
        };
        let seen = self.seen();
        let loops = {
            let inner = self.lock();
            let st = self.rule_view(&inner, seen.as_ref()).state;
            if let Err(r) = rules::check(Action::Event, &self.policy, st, seen.as_ref()) {
                drop(inner);
                return self.refused(&req, r);
            }
            Arc::clone(&inner.own.as_ref().expect("running ⇒ owned").loops)
        };
        let session = format!("studio-{name}-{}", uuid::Uuid::new_v4());
        let extra = json!({"loop": target, "session_id": session});
        match loops.submit(&target, scenario.raw.clone(), session.clone()) {
            Ok(()) => {
                let detail = format!("queued on loop `{target}` as session `{session}`");
                let code = StatusCode::ACCEPTED;
                self.verdict(&req, Status::Ok, code, detail, extra, Some(&session))
            }
            Err(r) => {
                let code = match r {
                    LoopRefused::QueueFull { .. } => StatusCode::TOO_MANY_REQUESTS,
                    LoopRefused::ShuttingDown => StatusCode::CONFLICT,
                    LoopRefused::UnknownLoop => StatusCode::UNPROCESSABLE_ENTITY,
                };
                let detail = format!("loop `{target}` refused the event: {}", r.as_str());
                self.verdict(&req, Status::Refused, code, detail, extra, Some(&session))
            }
        }
    }

    /// Studio is stopping (SIGINT / SIGTERM, `reason`): no new Play; a Play
    /// still starting is waited for (≤ 60 s); the runtime this Studio runs
    /// is drained (the Stop path) and waited for; then `studio.stopped`.
    pub(crate) async fn shutdown(&self, reason: &str) {
        self.lock().closing = true;
        let deadline = Instant::now() + START_WAIT;
        while self.lock().phase == Phase::Starting && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let running = self.lock().phase;
        let mut drained = None;
        match running {
            Phase::Running => {
                let v = self.stop_as(&format!("studio {reason}")).await;
                drained = Some(v.code == StatusCode::OK);
            }
            Phase::Stopping => {
                let ended = self.lock().own.as_ref().map(|o| o.ended.clone());
                if let Some(mut ended) = ended {
                    let done = tokio::time::timeout(self.stop_wait, ended.wait_for(|e| *e)).await;
                    drained = Some(done.is_ok());
                }
            }
            _ => {}
        }
        if self.enabled() {
            let last_run = self.lock().last_run.clone();
            self.emit(
                "studio.stopped",
                Status::Ok,
                json!({"reason": reason, "drained": drained, "last_run": last_run}),
                None,
                None,
                None,
            );
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! The control use case on a real runtime (`Runtime::begin` +
    //! `start_recorded` / `launch`) in a temp state dir: the lease, the
    //! drain, attach, the trace. HTTP + guard: `studio/tests.rs`.
    use super::*;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use crate::adapters::outbound::runtime_store::{read_heartbeat, SqliteRuntimeStore};
    use crate::application::runtime::loops::tests::SlowLoop;
    use crate::application::runtime::loops::LoopHandler;
    use crate::application::runtime::LeaseTiming;
    use crate::application::trace_exec::tests::MemTrace;
    use crate::bootstrap::runtime::{LeasePlan, Runtime};
    use crate::config::runtime::RuntimeConfig;
    use crate::config::studio::control_policy;
    use crate::domain::runtime::{lease_resource, RunState};
    use crate::ports::runtime::RuntimeStore;

    const SANDBOX: &str = "control-loop-lab";
    const WAIT: Duration = Duration::from_secs(10);

    fn timing() -> LeaseTiming {
        LeaseTiming {
            ttl_ms: 60_000,
            renew_ms: 60_000,
        }
    }

    fn plan(dir: &Path) -> LeasePlan {
        LeasePlan {
            sandbox: SANDBOX.into(),
            state_dir: dir.to_path_buf(),
            ledger: false,
            soe_state: None,
        }
    }

    /// `[agents.lab]` in `ws`, loop `demo`, `extra` (feeds) — the lab's shape
    /// without a Jev key; `[studio] control = true`; a `scenarios/` dir.
    fn lab_like(dir: &Path, ws: &Path, extra: &str) -> Config {
        let sandbox_dir = dir.join("sandboxes").join(SANDBOX);
        std::fs::create_dir_all(sandbox_dir.join("scenarios")).unwrap();
        for (name, body) in [
            ("act.json", r#"{"scenario":"act"}"#),
            ("normal.json", r#"{"scenario":"normal"}"#),
            ("uncertain.map.json", r#"{"loop":"demo","event":{}}"#),
        ] {
            std::fs::write(sandbox_dir.join("scenarios").join(name), body).unwrap();
        }
        let text = format!(
            "[runtime]\nheartbeat_secs = 1\nshutdown_grace_secs = 5\n\n\
             [agents.lab]\nengine = \"openrouter\"\nmodel = \"m\"\nworkspace = \"{}\"\n\
             tools = [\"list_directory\", \"read_file\"]\n\n\
             [decision_loops.demo]\ngoal = \"g\"\nagent = \"lab\"\n\
             [decision_loops.demo.actions.hold]\ndescription = \"stop\"\n\n\
             [studio]\ncontrol = true\n\n{extra}\n",
            ws.display()
        );
        let path = sandbox_dir.join("config.toml");
        std::fs::write(&path, &text).unwrap();
        let mut c: Config = toml::from_str(&text).unwrap();
        c.validate().unwrap();
        c.fold_default_scopes();
        c.loaded_from = Some(path);
        c.sandbox_name = Some(SANDBOX.into());
        c
    }

    /// The runtime `tengu run` would start, its lease + heartbeat in
    /// `state` and a `SlowLoop` as `demo` (Jev needs a key).
    pub(crate) fn slow_starter(state: PathBuf, slow: Arc<SlowLoop>) -> Starter {
        Arc::new(move || {
            let (state, slow) = (state.clone(), Arc::clone(&slow));
            Box::pin(async move {
                let cfg = RuntimeConfig {
                    heartbeat_secs: 1,
                    shutdown_grace_secs: 5,
                    ..RuntimeConfig::default()
                };
                let mut rt = Runtime::begin(plan(&state), cfg, timing()).await?;
                rt.launch(
                    BTreeMap::from([("demo".to_string(), slow as Arc<dyn LoopHandler>)]),
                    BTreeMap::new(),
                );
                Ok(RunSession::from_runtime(rt))
            })
        })
    }

    /// The full `start_recorded` (feeds, lifecycle events) of `config`.
    fn recorded_starter(state: PathBuf, config: Config, rt_trace: Arc<MemTrace>) -> Starter {
        let config = Arc::new(config);
        Arc::new(move || {
            let (state, config, rt_trace) =
                (state.clone(), Arc::clone(&config), Arc::clone(&rt_trace));
            Box::pin(async move {
                let rt = Runtime::begin(plan(&state), config.runtime.clone(), timing()).await?;
                let rt = rt
                    .start_recorded(
                        &config,
                        Arc::new(SecretRegistry::new()),
                        None,
                        rt_trace as Arc<dyn TraceSink>,
                    )
                    .await?;
                Ok(RunSession::from_runtime(rt))
            })
        })
    }

    struct Lab {
        dir: tempfile::TempDir,
        _ws: tempfile::TempDir,
        ctl: Arc<Controller>,
        trace: Arc<MemTrace>,
    }

    impl Lab {
        fn state(&self) -> PathBuf {
            self.dir.path().join("state")
        }
    }

    fn lab(starter: impl FnOnce(PathBuf, &Config) -> Starter) -> Lab {
        let (dir, ws) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let config = lab_like(dir.path(), ws.path(), "");
        let s = starter(state.clone(), &config);
        let policy = control_policy(&config, false);
        assert!(policy.enabled, "{policy:?}");
        let ctx =
            StudioContext::at(config, Arc::new(SecretRegistry::new()), dir.path(), &state).unwrap();
        let trace = Arc::new(MemTrace::default());
        let ctl = Arc::new(Controller::new(
            policy,
            &ctx,
            s,
            Arc::clone(&trace) as Arc<dyn TraceSink>,
        ));
        Lab {
            dir,
            _ws: ws,
            ctl,
            trace,
        }
    }

    fn slow_lab(ms: u64) -> (Lab, Arc<SlowLoop>) {
        let slow = SlowLoop::new(ms);
        let s2 = Arc::clone(&slow);
        (lab(move |state, _| slow_starter(state, s2)), slow)
    }

    fn state_of(ctl: &Controller) -> String {
        ctl.view()["state"].as_str().unwrap().to_string()
    }

    async fn wait_until(mut f: impl FnMut() -> bool) {
        let t0 = std::time::Instant::now();
        while !f() {
            assert!(t0.elapsed() < WAIT, "timed out");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A process that is not this Studio holding the lease (a CLI `tengu run`).
    async fn cli(state: &Path) -> Runtime {
        let mut rt = Runtime::begin(plan(state), RuntimeConfig::default(), timing())
            .await
            .unwrap();
        rt.launch(BTreeMap::new(), BTreeMap::new());
        let holder = rt.holder().to_string();
        wait_until(|| {
            read_heartbeat(state, SANDBOX)
                .ok()
                .flatten()
                .is_some_and(|hb| hb.holder == holder)
        })
        .await;
        rt
    }

    /// Play takes `runtime:<sandbox>` in `<state>/runtime.db` — the `tengu
    /// run` lease: a second start (a CLI `tengu run`) is refused naming
    /// Studio's holder; heartbeat + loops run under that holder.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn play_uses_runtime_lease() {
        let (lab, _slow) = slow_lab(1);
        assert_eq!(state_of(&lab.ctl), "idle");
        let v = lab.ctl.play(None).await;
        assert_eq!(v.code, StatusCode::OK, "{}", v.outcome.detail);
        let holder = v.extra["holder"].as_str().unwrap().to_string();
        assert!(holder.contains(&format!(":{}:", std::process::id())));
        assert_eq!(state_of(&lab.ctl), "running");
        let refused = Runtime::begin(plan(&lab.state()), RuntimeConfig::default(), timing())
            .await
            .err()
            .expect("the lease is Studio's");
        let held = LeaseHeld::of(&refused).expect("a lease refusal");
        assert_eq!(held.resource, lease_resource(SANDBOX));
        assert_eq!(held.holder, holder);
        wait_until(|| {
            read_heartbeat(&lab.state(), SANDBOX)
                .ok()
                .flatten()
                .is_some_and(|hb| hb.holder == holder && hb.state == RunState::Running)
        })
        .await;
        assert_eq!(lab.ctl.stop().await.code, StatusCode::OK);
    }

    /// Play while running (ours) is 409; two Plays at once start one runtime.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn second_play_refused() {
        let (lab, _slow) = slow_lab(1);
        let (a, b) = tokio::join!(lab.ctl.play(None), lab.ctl.play(None));
        let mut codes = [a.code, b.code];
        codes.sort();
        assert_eq!(codes, [StatusCode::OK, StatusCode::CONFLICT]);
        let again = lab.ctl.play(None).await;
        assert_eq!(again.code, StatusCode::CONFLICT);
        assert!(
            again.outcome.detail.contains("already running"),
            "{}",
            again.outcome.detail
        );
        assert_eq!(again.outcome.status, Status::Refused);
        lab.ctl.stop().await;
    }

    /// A CLI `tengu run` holds the lease: Studio shows `attached`, every
    /// action is refused naming that holder; once it stops, Play works. A
    /// Play that races the heartbeat is refused by the lease itself.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn play_attaches_when_cli_holds_lease() {
        let (lab, _slow) = slow_lab(1);
        let other = cli(&lab.state()).await;
        let holder = other.holder().to_string();
        let view = lab.ctl.view();
        assert_eq!(view["state"], json!("attached"));
        assert_eq!(view["seen"]["holder"], json!(holder));
        assert!(view["actions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["ok"] == json!(false)));
        let v = lab.ctl.play(None).await;
        assert_eq!(v.code, StatusCode::CONFLICT);
        assert!(v.outcome.detail.contains(&holder), "{}", v.outcome.detail);

        // A lease without a live heartbeat (a holder that crashed or has not
        // beaten yet): the state says idle, the lease itself refuses, typed.
        other.shutdown().await;
        let ghost = SqliteRuntimeStore::open(&lab.state()).unwrap();
        assert!(
            ghost
                .acquire_lease(&lease_resource(SANDBOX), "ghost:7:x", 60_000, now_ms())
                .await
                .unwrap()
                .granted
        );
        let v = lab.ctl.play(None).await;
        assert_eq!(v.code, StatusCode::CONFLICT);
        assert_eq!(v.extra["attached"], json!(true));
        assert_eq!(v.extra["holder"], json!("ghost:7:x"));
        assert_eq!(state_of(&lab.ctl), "idle", "a refused Play changes nothing");
        ghost
            .release_lease(&lease_resource(SANDBOX), "ghost:7:x")
            .await
            .unwrap();
        assert_eq!(lab.ctl.play(None).await.code, StatusCode::OK);
        lab.ctl.stop().await;
    }

    /// Stop = the graceful drain: the running event finishes, the heartbeat
    /// says `stopped` with reason `studio stop`, the lease is free, the
    /// state is `stopped`; Studio's trace has the request, verdict and
    /// `studio.runtime`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_drains_and_releases_lease() {
        let (lab, slow) = slow_lab(300);
        assert_eq!(lab.ctl.play(None).await.code, StatusCode::OK);
        let sent = lab.ctl.event("act", None).await;
        assert_eq!(sent.code, StatusCode::ACCEPTED, "{}", sent.outcome.detail);
        let session = sent.extra["session_id"].as_str().unwrap().to_string();
        assert!(session.starts_with("studio-act-"), "{session}");
        wait_until(|| !slow.seen.lock().unwrap().is_empty()).await;
        let v = lab.ctl.stop().await;
        assert_eq!(v.code, StatusCode::OK, "{}", v.outcome.detail);
        assert_eq!(v.extra["end"]["drain"]["finished"], json!(1));
        assert_eq!(v.extra["end"]["lease_released"], json!(true));
        assert_eq!(
            slow.seen.lock().unwrap().as_slice(),
            std::slice::from_ref(&session)
        );
        let hb = read_heartbeat(&lab.state(), SANDBOX).unwrap().unwrap();
        assert_eq!(hb.state, RunState::Stopped);
        assert_eq!(hb.stop_reason.as_deref(), Some("studio stop"));
        assert_eq!(state_of(&lab.ctl), "stopped");
        // Free: a CLI start works now.
        let next = Runtime::begin(plan(&lab.state()), RuntimeConfig::default(), timing())
            .await
            .unwrap();
        next.shutdown().await;
        let kinds = lab.trace.kinds();
        assert_eq!(kinds.last().unwrap(), "studio.runtime");
        let d = lab.trace.all();
        let runtime = d.last().unwrap();
        assert_eq!(runtime.status, Status::Ok);
        assert_eq!(runtime.payload["reason"], json!("studio stop"));
        // Child of the Play verdict (the 2nd event: request, verdict).
        assert_eq!(runtime.parent_event_id, Some(MemTrace::id(1)));
        let event_ok = d
            .iter()
            .find(|e| e.payload["action"] == json!("event") && e.status == Status::Ok)
            .unwrap();
        assert_eq!(event_ok.session_id.as_deref(), Some(session.as_str()));
        // Play again after a stop.
        assert_eq!(lab.ctl.play(None).await.code, StatusCode::OK);
        lab.ctl.stop().await;
    }

    /// Stop / event of a runtime Studio did not start: 409, the holder named;
    /// unknown scenario or loop: 422; nothing running: 409.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stop_refused_for_attached_runtime() {
        let (lab, _slow) = slow_lab(1);
        assert_eq!(lab.ctl.stop().await.code, StatusCode::CONFLICT);
        assert_eq!(lab.ctl.event("act", None).await.code, StatusCode::CONFLICT);
        let other = cli(&lab.state()).await;
        let holder = other.holder().to_string();
        let v = lab.ctl.stop().await;
        assert_eq!(v.code, StatusCode::CONFLICT);
        assert!(
            v.outcome.detail.contains("did not start") && v.outcome.detail.contains(&holder),
            "{}",
            v.outcome.detail
        );
        assert_eq!(lab.ctl.event("act", None).await.code, StatusCode::CONFLICT);
        let bad = lab.ctl.event("nope", None).await;
        assert_eq!(bad.code, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(
            bad.outcome.detail.contains("act, normal"),
            "{}",
            bad.outcome.detail
        );
        let map = lab.ctl.event("uncertain.map", None).await;
        assert_eq!(
            map.code,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a map is not an event"
        );
        let bad_loop = lab.ctl.event("act", Some("other")).await;
        assert_eq!(bad_loop.code, StatusCode::UNPROCESSABLE_ENTITY);
        // The CLI runtime is untouched: still running under its holder.
        let hb = read_heartbeat(&lab.state(), SANDBOX).unwrap().unwrap();
        assert_eq!(
            (hb.holder.as_str(), hb.state),
            (holder.as_str(), RunState::Running)
        );
        other.shutdown().await;
    }

    /// A start that fails (a feed whose agent cannot run its tool) is a
    /// `studio.control` failed event with the error, state `failed`, the
    /// lease free again; the runtime's own recording has `start_failed`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn start_failure_is_a_trace_event() {
        let rt_trace = Arc::new(MemTrace::default());
        let t2 = Arc::clone(&rt_trace);
        let lab = lab(move |state, base| {
            // No loop (Jev needs a key); a tool feed of a tool `lab` lacks.
            let mut cfg = base.clone();
            let feed: crate::config::feeds::FeedConfig = toml::from_str(
                "kind = \"tool\"\nagent = \"lab\"\ntool = \"read_fil\"\nevery_secs = 60\n",
            )
            .unwrap();
            cfg.feeds.insert("probe".into(), feed);
            cfg.decision_loops.clear();
            recorded_starter(state, cfg, t2)
        });
        let v = lab.ctl.play(None).await;
        assert_eq!(v.code, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(v.outcome.status, Status::Failed);
        assert!(
            v.outcome.detail.contains("cannot run `read_fil`"),
            "{}",
            v.outcome.detail
        );
        assert_eq!(state_of(&lab.ctl), "failed");
        let d = lab.trace.all();
        let (req, verdict) = (&d[0], &d[1]);
        assert_eq!(
            (req.kind.as_str(), req.status),
            ("studio.control", Status::Pending)
        );
        assert_eq!(
            (verdict.kind.as_str(), verdict.status),
            ("studio.control", Status::Failed)
        );
        assert_eq!(verdict.parent_event_id, Some(MemTrace::id(0)));
        assert!(verdict.payload["detail"]
            .as_str()
            .unwrap()
            .contains("read_fil"));
        assert!(verdict.duration_ms.is_some());
        assert!(rt_trace
            .kinds()
            .contains(&"runtime.start_failed".to_string()));
        let view = lab.ctl.view();
        assert_eq!(view["last"]["status"], json!("failed"));
        assert_eq!(view["tone"], json!("red"));
        // Lease free: Play may be pressed again (it fails the same way).
        let again = Runtime::begin(plan(&lab.state()), RuntimeConfig::default(), timing())
            .await
            .unwrap();
        again.shutdown().await;
    }

    /// Studio's SIGINT path (`Controller::shutdown`): the owned runtime is
    /// drained (heartbeat `stopped`, reason `studio SIGINT`, lease free),
    /// `studio.stopped` is the last line, and no Play starts afterwards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sigint_drains_owned_runtime() {
        let (lab, slow) = slow_lab(200);
        assert_eq!(lab.ctl.play(None).await.code, StatusCode::OK);
        assert_eq!(
            lab.ctl.event("normal", None).await.code,
            StatusCode::ACCEPTED
        );
        wait_until(|| !slow.seen.lock().unwrap().is_empty()).await;
        lab.ctl.shutdown("SIGINT").await;
        let hb = read_heartbeat(&lab.state(), SANDBOX).unwrap().unwrap();
        assert_eq!(hb.state, RunState::Stopped);
        assert_eq!(hb.stop_reason.as_deref(), Some("studio SIGINT"));
        assert_eq!(slow.running.load(std::sync::atomic::Ordering::SeqCst), 0);
        let d = lab.trace.all();
        let last = d.last().unwrap();
        assert_eq!(last.kind, "studio.stopped");
        assert_eq!(last.payload["drained"], json!(true));
        assert!(crate::domain::trace::closes_run(&last.kind, last.status));
        let v = lab.ctl.play(None).await;
        assert_eq!(v.code, StatusCode::CONFLICT);
        assert!(v.outcome.detail.contains("Studio is stopping"));
        let free = Runtime::begin(plan(&lab.state()), RuntimeConfig::default(), timing())
            .await
            .unwrap();
        free.shutdown().await;
    }

    /// A crashed owner (no heartbeat, a lease nobody renews): Play is
    /// refused until the TTL passes, then takes the lease over — nothing
    /// stays orphaned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_orphan_after_owner_crash_lease_expires() {
        let (lab, _slow) = slow_lab(1);
        let ghost = SqliteRuntimeStore::open(&lab.state()).unwrap();
        assert!(
            ghost
                .acquire_lease(&lease_resource(SANDBOX), "crashed:1:dead", 300, now_ms())
                .await
                .unwrap()
                .granted
        );
        let v = lab.ctl.play(None).await;
        assert_eq!(v.code, StatusCode::CONFLICT);
        assert_eq!(v.extra["holder"], json!("crashed:1:dead"));
        tokio::time::sleep(Duration::from_millis(400)).await;
        let v = lab.ctl.play(None).await;
        assert_eq!(v.code, StatusCode::OK, "{}", v.outcome.detail);
        lab.ctl.stop().await;
    }

    /// The browser goes away mid-Play (the HTTP handler future is dropped):
    /// the start still finishes, the runtime is Studio's (`running`), and
    /// Stop drains it — no half-started runtime holding the lease.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn play_survives_a_dropped_request() {
        let lab = lab(|state, _| {
            let inner = slow_starter(state, SlowLoop::new(1));
            Arc::new(move || {
                let start = inner();
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    start.await
                }) as StartFuture
            })
        });
        let dropped = tokio::time::timeout(Duration::from_millis(20), lab.ctl.play(None)).await;
        assert!(dropped.is_err(), "the request was dropped mid-start");
        assert_eq!(state_of(&lab.ctl), "starting");
        wait_until(|| state_of(&lab.ctl) == "running").await;
        let v = lab.ctl.stop().await;
        assert_eq!(v.code, StatusCode::OK, "{}", v.outcome.detail);
        assert_eq!(v.extra["end"]["lease_released"], json!(true));
    }

    /// Control off: every action 403 with the policy's reason, nothing
    /// recorded, nothing started.
    #[tokio::test]
    async fn read_only_refuses_every_action() {
        let dir = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let mut config = lab_like(dir.path(), ws.path(), "");
        config.studio.control = false;
        let policy = control_policy(&config, false);
        let ctx = StudioContext::at(
            config,
            Arc::new(SecretRegistry::new()),
            dir.path(),
            dir.path(),
        )
        .unwrap();
        let ctl = Arc::new(Controller::read_only(policy, &ctx));
        assert!(!ctl.enabled());
        for v in [
            ctl.play(None).await,
            ctl.stop().await,
            ctl.event("act", None).await,
        ] {
            assert_eq!(v.code, StatusCode::FORBIDDEN, "{}", v.outcome.detail);
            assert!(v.outcome.detail.contains("--allow-control"));
            assert_eq!(v.outcome.event_id, None, "read-only records nothing");
        }
        let view = ctl.view();
        assert_eq!(view["enabled"], json!(false));
        assert_eq!(view["scenarios"][0]["name"], json!("act"));
        assert!(view["scenarios"][0].get("raw").is_none());
    }
}
