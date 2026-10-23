//! Decision loop — a System One model (Jev) picks the next action and its
//! arguments; existing tools execute it. One `[decision_loops.<name>]` block
//! per loop (`config/decision_loop.rs`); design and probes in
//! `docs/decision-loop-plan-2026-09-24.md`.
//!
//! Per incoming event, up to `max_steps` times:
//!
//! 1. Read `world` (alias → observation key) from the `ObservationStore`
//!    (`world::World`; never fetched). Legal actions = terminal actions +
//!    actions whose every slot has candidates (`slots::candidates`, caps
//!    applied; `FromHistory` reads only this event's entries) and whose
//!    every `requires` alias is fresh.
//!    With a `sequence`: only its next step + the terminal actions (an
//!    optional step that is not legal is skipped; a failed or refused step
//!    halts the chain — terminal actions only).
//! 2. One decisions call: `next_action` (choice over legal actions) + one
//!    `choice` per multi-candidate slot (`<action>__<slot>`), against
//!    `state = {goal, event, world (omitted when empty), history, step}`.
//! 3. Gate: min(confidence of action, its slots) < `act_at` → escalate
//!    (orchestrator turn via `Escalator`) and stop.
//! 4. Terminal → stop. Write action under `dry_run` → log it (history
//!    `result = "dry_run"`, `ok` unset) and continue: a slot bound to it
//!    has no candidate, so what needs its output stays illegal.
//!    Otherwise render args, re-check caps, run the tool through the
//!    `ToolExecutor` port (`execute_typed`; same scopes / egress as an
//!    agent) and append to `history`: a typed result contributes its
//!    `decision_value` (or the reducer over `decision_root`), `ok = status
//!    != error` and `obs` meta; a text result goes through
//!    `reduce::parse_tool_output`. A typed result with `features.risk =
//!    "deny"` (an exec tool's `[risk]` gate refused the order) is outcome
//!    `Refused { rule }`, else `Executed`. Continue.
//!
//! Every decisions call writes one JSONL audit line (`AuditLog`) — a failed
//! call too (`outcome = "error"`), before its error propagates — and a
//! successful one also emits a `MetricsKind::Decision` record. A line is one
//! `write_all` on an append-mode file, so concurrent loops and processes
//! never interleave inside it. A step that ran a tool carries its `call_id`
//! (`{loop}:{session_id}:{t}`), which the exec tools' risk verdicts carry
//! too (`ledger.db` `risk_decisions`, `<TENGU_HOME>/logs/risk.jsonl`).
//! Lines of a traced process also name its recording: `runtime_id` (the
//! `tengu run` lease holder) and `run_id` (`AuditLog::ids`, additive keys).
//! History is in-process (lost on restart); events for one loop are
//! serialised by the state mutex so history stays ordered.
//!
//! Trace ([`DecisionLoop::with_trace`]; `TENGU_STUDIO_PLAN.md` ST-12): each
//! step writes, under what caused the event (`trace_exec::cause`:
//! `loop.started`, a decide `trigger.*`):
//!
//! | Event (node) | Status | When / payload |
//! |---|---|---|
//! | `observation.read` (`world:<loop>/<alias>`) | `ok` · `stale` · `missing` · `failed` | each `world` alias as `World::reads` classifies it: `key`, `age_ms` |
//! | `jev.completed` · `jev.failed` (`jev:<loop>`) | `ok` · `failed` | the decisions call: `decision_id`, `model`, `answers`, `usage`, `latency_ms` (= `duration_ms`), `legal` (action → slot → labels: what was offered), `legal_actions` (its action names), `questions`; both pinned (`EventDraft::keep`): the payload bound drops every other field first, then `legal`, then `legal_actions` |
//! | `action.selected` (`action:<loop>/<a>`) | `running` | a tool action past the gate and the caps, before its call; the call's `tool.*` are its children (`trace_exec::TracedExecutor`) |
//! | `action.completed` · `action.refused` | `ok` / `failed` (the history entry's `ok`) · `refused` (`[risk]` rule) | after the call: `ok`, `output`, `obs`, `call_id` |
//! | `action.selected` | `ok` (terminal) · `skipped` (dry-run write) | a terminal action; a write under `dry_run` |
//! | `action.escalated` (`gate:<loop>/act_at`) | `escalated` | confidence < `act_at`: `confidence`, `act_at`, `escalate` |
//! | `action.rejected` | `refused` | a label not offered, a missing slot answer, a cap |
//!
//! Outcome events carry the `StepOutcome` `apply` computed (`outcome`);
//! nothing is re-derived for the trace.
//!
//! | Time | Source |
//! |---|---|
//! | `world` freshness, typed-result ages, audit `ts` / `ts_ms`, metrics `ts_unix` | the loop's `Clock` ([`DecisionLoop::with_clock`]); none = the wall clock |
//! | `latency_ms` | real (`Instant`), also in replay |
//!
//! Replay (`bootstrap::decision::build_replay_loop`, the backtest gate arm):
//! the real loop on a `SimClock` set to each decision instant, terminal
//! actions only, no tools; [`DecisionLoop::decide_terminal`] returns the
//! one decision's `Verdict`.

pub(crate) mod reduce;
pub(crate) mod slots;
pub(crate) mod world;

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::application::trace_exec::{self, Cause};
use crate::config::decision_loop::DecisionLoopConfig;
use crate::domain::decision::{Decision, HistoryEntry, Question, StepOutcome, Verdict};
use crate::domain::message::ToolCall;
use crate::domain::metrics::{MetricsKind, MetricsRecord};
use crate::domain::observation::{now_ms, Observation};
use crate::domain::trace::{Component, EventDraft, Status};
use crate::domain::workflow::node_id;
use crate::ports::clock::Clock;
use crate::ports::decision::{DecisionEngine, Escalator};
use crate::ports::engine::ToolExecutor;
use crate::ports::observation::ObservationStore;
use crate::ports::trace::TraceSink;
use slots::Candidate;
use world::{Read, World};

const NEXT_ACTION: &str = "next_action";
/// `jev.*` payload fields the trace drops last (`EventDraft::keep`): the
/// action names, then the full legal set — what Studio greys nodes by.
const LEGAL_FIELDS: &[&str] = &["legal_actions", "legal"];

pub(crate) struct DecisionLoop {
    name: String,
    cfg: DecisionLoopConfig,
    engine: Arc<dyn DecisionEngine>,
    tools: Arc<dyn ToolExecutor>,
    /// Source of `state.world`; `None` = every world entry reads as an error.
    observations: Option<Arc<dyn ObservationStore>>,
    escalator: Option<Arc<dyn Escalator>>,
    audit: Option<AuditLog>,
    /// Time source ([`DecisionLoop::with_clock`]); `None` = the wall clock.
    clock: Option<Arc<dyn Clock>>,
    /// Where step events go ([`DecisionLoop::with_trace`]); `None` = nowhere.
    trace: Option<Arc<dyn TraceSink>>,
    state: Mutex<LoopState>,
}

/// What one step's trace events hang on.
#[derive(Debug, Default)]
struct StepTrace {
    /// The event that caused this loop event (`loop.started`, `trigger.*`).
    root: Option<String>,
    /// This step's `jev.completed`.
    jev: Option<String>,
    /// This step's `action.selected` of a tool action (before the call).
    selected: Option<String>,
    /// The chosen action's gate confidence (action + slots), once computed.
    confidence: Option<f64>,
}

/// Where the decision audit goes (`<TENGU_HOME>/logs/decisions.jsonl`; a
/// replay: the backtest run's `decisions.jsonl`) and what every line
/// carries besides the decision.
#[derive(Debug, Clone)]
pub(crate) struct AuditLog {
    pub path: PathBuf,
    /// `Config::sandbox_name`.
    pub sandbox: Option<String>,
    /// `trigger` on every line when set (`backtest` for replay); `None` =
    /// no `trigger` key (live lines keep their shape).
    pub trigger: Option<String>,
    /// The recording the line belongs to (`domain::trace::RunIds`):
    /// `runtime_id` (the `tengu run` lease holder) and `run_id` (its trace
    /// file, `tengu trace show --run`) on every line when set — additive
    /// keys, absent otherwise (replay lines keep their shape).
    pub ids: crate::domain::trace::RunIds,
}

#[derive(Default)]
struct LoopState {
    t: u64,
    /// `t` when the current event began: entries with a larger `t` are this
    /// event's. `FromHistory` slots read only those — `history` outlives
    /// events (webhook listener), and an earlier event's result may be
    /// hours old.
    event_start: u64,
    history: VecDeque<HistoryEntry>,
    /// `sequence` progress in the current event: the next step's index,
    /// and whether a failed / refused step halted the chain.
    seq_pos: usize,
    seq_halted: bool,
}

impl DecisionLoop {
    pub(crate) fn new(
        name: impl Into<String>,
        cfg: DecisionLoopConfig,
        engine: Arc<dyn DecisionEngine>,
        tools: Arc<dyn ToolExecutor>,
        observations: Option<Arc<dyn ObservationStore>>,
        escalator: Option<Arc<dyn Escalator>>,
        audit: Option<AuditLog>,
    ) -> Self {
        Self {
            name: name.into(),
            cfg,
            engine,
            tools,
            observations,
            escalator,
            audit,
            clock: None,
            trace: None,
            state: Mutex::new(LoopState::default()),
        }
    }

    /// Write every step's events to `sink` (module doc § Trace): the
    /// recording of `tengu run` / `tengu decide`. Replay loops record none.
    pub(crate) fn with_trace(mut self, sink: Arc<dyn TraceSink>) -> Self {
        self.trace = Some(sink);
        self
    }

    /// Read time from `clock` instead of the wall clock: `world` freshness,
    /// typed-result ages, the audit line's `ts` / `ts_ms` and the metrics
    /// timestamp. Replay sets a `SimClock` to each decision instant;
    /// `latency_ms` stays real.
    pub(crate) fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    fn now_ms(&self) -> i64 {
        self.clock.as_ref().map_or_else(now_ms, |c| c.now_ms())
    }

    /// Run the loop for one event. Returns what each step did.
    pub(crate) async fn handle_event(
        &self,
        event: &Value,
        session_id: &str,
    ) -> Result<Vec<StepOutcome>> {
        let steps = self.run_event(event, session_id).await?;
        Ok(steps.into_iter().map(|(outcome, _)| outcome).collect())
    }

    /// One event through a terminal-only loop (no action has a `tool`:
    /// `bootstrap::decision::build_replay_loop`). It stops after its first
    /// decision, whose `Verdict` this is: the chosen action, its confidence,
    /// p of each action, whether it was below `act_at`, and the outcome
    /// (`Stopped` / `Escalated` / `Rejected`). A failed decisions call
    /// (offline cache miss, Jev error) is audited, then returned as the
    /// error. A loop with a tool action is refused before any call.
    pub(crate) async fn decide_terminal(&self, event: &Value, session_id: &str) -> Result<Verdict> {
        if let Some((an, _)) = self.cfg.actions.iter().find(|(_, a)| a.tool.is_some()) {
            bail!(
                "decision loop `{}`: action `{an}` runs a tool — decide_terminal needs a \
                 terminal-only loop",
                self.name
            );
        }
        let (outcome, decision) = self
            .run_event(event, session_id)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("decision loop `{}` took no step", self.name))?;
        Ok(Verdict::of(
            outcome,
            decision.answers.get(NEXT_ACTION),
            self.cfg.act_at,
        ))
    }

    /// Up to `max_steps` steps for one event, each with the decision it
    /// acted on; stops after a terminal, escalated or rejected step (a
    /// dry-run write goes on, so a chain can be walked without its writes).
    async fn run_event(
        &self,
        event: &Value,
        session_id: &str,
    ) -> Result<Vec<(StepOutcome, Decision)>> {
        // Every step's events hang on what caused this event (`loop.started`,
        // `trigger.*`); read before the state lock, on the caller's task.
        let root = trace_exec::cause().and_then(|c| c.parent);
        let mut st = self.state.lock().await;
        st.event_start = st.t;
        st.seq_pos = 0;
        st.seq_halted = false;
        let event = reduce::reduce(event, &self.cfg.event_reduce);
        let mut steps = Vec::new();
        for step in 0..self.cfg.max_steps {
            let tr = StepTrace {
                root: root.clone(),
                ..Default::default()
            };
            let (outcome, decision) = self.step(&mut st, &event, step, session_id, tr).await?;
            let stop = !matches!(
                outcome,
                StepOutcome::Executed { .. }
                    | StepOutcome::Refused { .. }
                    | StepOutcome::DryRun { .. }
            );
            steps.push((outcome, decision));
            if stop {
                break;
            }
        }
        Ok(steps)
    }

    /// The history ring buffer, oldest first — what each step ran with and
    /// returned (`tengu decide` prints it).
    pub(crate) async fn history(&self) -> Vec<HistoryEntry> {
        self.state.lock().await.history.iter().cloned().collect()
    }

    async fn step(
        &self,
        st: &mut LoopState,
        event: &Value,
        step: u32,
        session_id: &str,
        mut tr: StepTrace,
    ) -> Result<(StepOutcome, Decision)> {
        // 1. World, legal actions + their slot candidates.
        let world = World::read(self.observations.as_deref(), &self.cfg, self.now_ms()).await;
        self.trace_world(&world, session_id, &tr, step);
        let mut legal: BTreeMap<&str, BTreeMap<&str, Vec<Candidate>>> = BTreeMap::new();
        for (an, action) in &self.cfg.actions {
            if !world.satisfies(&action.requires) {
                continue;
            }
            let mut slots = BTreeMap::new();
            for (sn, slot) in &action.slots {
                let this_event = st.history.iter().filter(|h| h.t > st.event_start);
                let cap = action.caps.get(sn).copied();
                let c = slots::candidates(sn, slot, cap, this_event, &world, event);
                if c.is_empty() {
                    break;
                }
                slots.insert(sn.as_str(), c);
            }
            if slots.len() == action.slots.len() {
                legal.insert(an.as_str(), slots);
            }
        }
        // `sequence`: the next step + the terminal actions, nothing else.
        if !self.cfg.sequence.is_empty() {
            let next = self.sequence_next(st, &legal);
            legal.retain(|an, _| self.cfg.actions[*an].tool.is_none() || Some(*an) == next);
        }

        // 2. Questions + state.
        let mut questions = BTreeMap::new();
        questions.insert(
            NEXT_ACTION.to_string(),
            Question::Choice {
                instructions: "Given the goal, the event and the action history, pick the next action. \
                               Do not repeat an action that already succeeded for this event; pick a \
                               terminal action when there is nothing useful left to do."
                    .into(),
                criteria: legal
                    .keys()
                    .map(|an| (an.to_string(), self.cfg.actions[*an].description.clone()))
                    .collect(),
            },
        );
        for (an, slots) in &legal {
            for (sn, cands) in slots.iter().filter(|(_, c)| c.len() > 1) {
                questions.insert(
                    slot_key(an, sn),
                    Question::Choice {
                        instructions: format!(
                            "If the next action is `{an}`, which `{sn}` should it use? Respect the goal's limits."
                        ),
                        criteria: cands
                            .iter()
                            .map(|c| (c.label.clone(), c.description.clone()))
                            .collect(),
                    },
                );
            }
        }
        let mut state = json!({
            "goal": self.cfg.goal,
            "event": event,
            "history": st.history,
            "step": step,
        });
        if let (Some(w), Value::Object(o)) = (world.to_state(), &mut state) {
            o.insert("world".into(), w);
        }

        // 3. Decide. A failed call (timeout, 402, 5xx) is audited, then propagates.
        let started = Instant::now();
        let decided = self.engine.decide(&state, &questions).await;
        let latency_ms = started.elapsed().as_millis() as u64;
        // What Jev was offered: the legal set (a configured action outside it is
        // drawn grey) — pinned ([`LEGAL_FIELDS`]): it survives the payload bound.
        let offered = json!({
            "step": step,
            "legal": legal_json(&legal),
            "legal_actions": legal.keys().collect::<Vec<_>>(),
            "questions": questions.keys().collect::<Vec<_>>(),
            "latency_ms": latency_ms,
            "act_at": self.cfg.act_at,
        });
        let decision = match decided {
            Ok(d) => d,
            Err(e) => {
                let outcome = StepOutcome::Error {
                    reason: format!("{e:#}"),
                };
                self.audit(session_id, st.t, None, &outcome, None, latency_ms);
                let mut p = offered;
                merge(
                    &mut p,
                    json!({"model": self.engine.model(), "outcome": outcome}),
                );
                let d = self
                    .draft("jev.failed", Component::Jev, Status::Failed, session_id)
                    .node(node_id::jev(&self.name))
                    .keep(LEGAL_FIELDS)
                    .duration(latency_ms)
                    .payload(p);
                self.emit(d, tr.root.as_deref());
                return Err(e);
            }
        };
        self.record_metrics(session_id, &state, &decision, latency_ms);
        let mut p = offered;
        merge(
            &mut p,
            json!({
                "decision_id": decision.id,
                "model": decision.model,
                "answers": decision.answers,
                "usage": decision.usage,
            }),
        );
        let mut d = self
            .draft("jev.completed", Component::Jev, Status::Ok, session_id)
            .node(node_id::jev(&self.name))
            .keep(LEGAL_FIELDS)
            .duration(latency_ms)
            .payload(p);
        if let Some(a) = &self.audit {
            d = d.artifact(a.path.display().to_string(), Some(decision.id.clone()));
        }
        tr.jev = self.emit(d, tr.root.as_deref());

        let t_before = st.t;
        let outcome = self
            .apply(st, &legal, &decision, &state, session_id, &mut tr)
            .await;
        if !self.cfg.sequence.is_empty() {
            Self::sequence_advance(st, &outcome);
        }
        // A step that ran (or dry-ran) appended one history entry with the resolved args.
        let entry = st.history.back().filter(|_| st.t > t_before);
        self.audit(
            session_id,
            st.t,
            Some(&decision),
            &outcome,
            entry,
            latency_ms,
        );
        self.trace_outcome(&outcome, entry, session_id, st.t, &tr);
        info!(
            decision_loop = %self.name,
            session_id = %session_id,
            step,
            outcome = ?outcome,
            "decision loop step"
        );
        Ok((outcome, decision))
    }

    async fn apply(
        &self,
        st: &mut LoopState,
        legal: &BTreeMap<&str, BTreeMap<&str, Vec<Candidate>>>,
        decision: &Decision,
        state: &Value,
        session_id: &str,
        tr: &mut StepTrace,
    ) -> StepOutcome {
        let Some(next) = decision.answers.get(NEXT_ACTION) else {
            return rejected("?", "decision has no next_action answer");
        };
        let action_name = next.choice.clone().unwrap_or_default();
        let Some(slots) = legal.get(action_name.as_str()) else {
            return rejected(&action_name, "not a legal action this step");
        };
        let action = &self.cfg.actions[&action_name];

        // Resolve slot values + overall confidence.
        let mut confidence = next.gate_confidence();
        let mut values = BTreeMap::new();
        for (sn, cands) in slots {
            let chosen = if cands.len() == 1 {
                &cands[0]
            } else {
                let Some(answer) = decision.answers.get(&slot_key(&action_name, sn)) else {
                    return rejected(&action_name, &format!("no answer for slot `{sn}`"));
                };
                confidence = confidence.min(answer.gate_confidence());
                let label = answer.choice.as_deref().unwrap_or_default();
                match cands.iter().find(|c| c.label == label) {
                    Some(c) => c,
                    None => {
                        return rejected(
                            &action_name,
                            &format!("slot `{sn}`: unknown label `{label}`"),
                        )
                    }
                }
            };
            values.insert(sn.to_string(), chosen.value.clone());
        }
        let args = slots::render_args(&action.args, &values);
        tr.confidence = Some(confidence);

        // Gate.
        if confidence < self.cfg.act_at {
            self.escalate(session_id, &action_name, &args, confidence, state)
                .await;
            return StepOutcome::Escalated {
                action: action_name,
                confidence,
            };
        }

        // Caps re-check on the final values — before `t` advances, so a
        // rejected step never claims a history slot (the audit line reads
        // "this step's entry" as `t > t_before`).
        for (sn, cap) in &action.caps {
            if let Some(x) = values.get(sn).and_then(slots::as_f64) {
                if x > *cap {
                    return rejected(
                        &action_name,
                        &format!("slot `{sn}` = {x} exceeds cap {cap}"),
                    );
                }
            }
        }

        st.t += 1;
        let t = st.t;
        let Some(tool) = &action.tool else {
            self.push(
                st,
                HistoryEntry {
                    t,
                    action: action_name.clone(),
                    args: Value::Null,
                    ok: None,
                    result: Value::Null,
                    obs: None,
                },
            );
            return StepOutcome::Stopped {
                action: action_name,
            };
        };

        if self.cfg.logs_only(action) {
            self.push(
                st,
                HistoryEntry {
                    t,
                    action: action_name.clone(),
                    args,
                    ok: None,
                    result: json!("dry_run"),
                    obs: None,
                },
            );
            return StepOutcome::DryRun {
                action: action_name,
            };
        }

        let call = ToolCall {
            id: self.call_id(session_id, t),
            name: tool.clone(),
            arguments: args.clone(),
        };
        // `action.selected` before the call: the tool events are its children.
        let d = self
            .draft(
                "action.selected",
                Component::Action,
                Status::Running,
                session_id,
            )
            .node(node_id::action(&self.name, &action_name))
            .call(call.id.clone())
            .payload(json!({
                "action": action_name,
                "tool": tool,
                "args": args,
                "confidence": confidence,
                "t": t,
            }));
        tr.selected = self.emit(d, tr.jev.as_deref());
        let now = self.now_ms();
        let cause = Cause::new(tr.selected.clone(), session_id);
        let typed = trace_exec::caused_by(cause, self.tools.execute_typed(&call, &[])).await;
        let mut refused = None;
        let (ok, result, obs) = match typed {
            // Typed: features (or the reducer over `{.., features, data}`);
            // a status-`error` observation is a failure.
            Ok(out) => match out.observation {
                Some(o) => {
                    refused = risk_refusal(&o);
                    let result = if action.reduce.is_empty() {
                        o.decision_value(now)
                    } else {
                        reduce::reduce(&o.decision_root(now), &action.reduce)
                    };
                    (o.status.usable(), result, Some(o.meta(now)))
                }
                // Legacy text tool.
                None => {
                    let (ok, raw) = reduce::parse_tool_output(&out.text);
                    (ok, reduce::reduce(&raw, &action.reduce), None)
                }
            },
            Err(e) => (false, json!(format!("{e:#}")), None),
        };
        self.push(
            st,
            HistoryEntry {
                t,
                action: action_name.clone(),
                args,
                ok: Some(ok),
                result,
                obs,
            },
        );
        match refused {
            Some(rule) => StepOutcome::Refused {
                action: action_name,
                rule,
            },
            None => StepOutcome::Executed {
                action: action_name,
            },
        }
    }

    /// The `sequence` action offered this step: optional steps that are not
    /// legal are skipped (for good — the order puts what feeds a step before
    /// it); `None` = the chain is halted, done, or its required next step is
    /// not legal — terminal actions only.
    fn sequence_next<'c>(
        &'c self,
        st: &mut LoopState,
        legal: &BTreeMap<&str, BTreeMap<&str, Vec<Candidate>>>,
    ) -> Option<&'c str> {
        if st.seq_halted {
            return None;
        }
        while let Some(s) = self.cfg.sequence.get(st.seq_pos) {
            if legal.contains_key(s.action.as_str()) {
                return Some(s.action.as_str());
            }
            if !s.optional {
                return None;
            }
            st.seq_pos += 1;
        }
        None
    }

    /// After a step: a successful (or dry-run) sequence step moves the chain
    /// on; a failed tool (`ok != true`) or a `[risk]` refusal halts it.
    fn sequence_advance(st: &mut LoopState, outcome: &StepOutcome) {
        match outcome {
            StepOutcome::DryRun { .. } => st.seq_pos += 1,
            StepOutcome::Executed { .. } => {
                if st.history.back().and_then(|e| e.ok) == Some(true) {
                    st.seq_pos += 1;
                } else {
                    st.seq_halted = true;
                }
            }
            StepOutcome::Refused { .. } => st.seq_halted = true,
            _ => {}
        }
    }

    /// `{loop}:{session_id}:{t}` — step `t`'s tool call id (`ToolCtx.call_id`
    /// in the tool); never repeats across events or restarts (session ids
    /// are per event). The audit line and a risk verdict carry it.
    fn call_id(&self, session_id: &str, t: u64) -> String {
        format!("{}:{session_id}:{t}", self.name)
    }

    fn push(&self, st: &mut LoopState, entry: HistoryEntry) {
        st.history.push_back(entry);
        while st.history.len() > self.cfg.history {
            st.history.pop_front();
        }
    }

    async fn escalate(
        &self,
        session_id: &str,
        action: &str,
        args: &Value,
        confidence: f64,
        state: &Value,
    ) {
        if !self.cfg.escalate {
            // Configured not to escalate (and every replay): the audit line
            // records `escalated`; nothing is wrong.
            debug!(decision_loop = %self.name, action, confidence, "low confidence; escalate = false — stopping");
            return;
        }
        let Some(escalator) = self.escalator.as_ref() else {
            warn!(decision_loop = %self.name, action, confidence, "low confidence; escalation unavailable — stopping");
            return;
        };
        // The args and the state carry event and tool text: fenced.
        let message = format!(
            "Decision loop `{}` is unsure (confidence {confidence:.2}) about the next step.\n\
             Proposed action: `{action}` with args:\n{}\n\
             Review the state, decide what should happen, and explain why. \
             Show every address, mint and signature in full.\n\nState:\n{}",
            self.name,
            crate::domain::untrusted::fence(&format!("loop-args:{}", self.name), &args.to_string()),
            crate::domain::untrusted::fence(
                &format!("loop-state:{}", self.name),
                &serde_json::to_string_pretty(state).unwrap_or_default()
            )
        );
        escalator.escalate(session_id.to_string(), message).await;
    }

    /// A step event of this loop's event `session_id` (module doc § Trace).
    fn draft(
        &self,
        kind: &str,
        component: Component,
        status: Status,
        session_id: &str,
    ) -> EventDraft {
        EventDraft::new(component, kind, status).session(session_id)
    }

    /// Write `d` under `parent`; its event id (`None`: no sink, or the
    /// write failed — fail-soft).
    fn emit(&self, d: EventDraft, parent: Option<&str>) -> Option<String> {
        let sink = self.trace.as_ref()?;
        sink.emit(match parent {
            Some(p) => d.parent(p),
            None => d,
        })
    }

    /// `observation.read` per `world` alias, as the step reads it
    /// (`World::reads`: the rule `state.world` renders).
    fn trace_world(&self, world: &World, session_id: &str, tr: &StepTrace, step: u32) {
        if self.trace.is_none() {
            return;
        }
        for (alias, read, age_ms) in world.reads() {
            let status = match read {
                Read::Fresh => Status::Ok,
                Read::Stale => Status::Stale,
                Read::Missing => Status::Missing,
                Read::Error => Status::Failed,
            };
            let d = self
                .draft(
                    "observation.read",
                    Component::Observation,
                    status,
                    session_id,
                )
                .node(node_id::world(&self.name, alias))
                .payload(json!({
                    "alias": alias,
                    "key": self.cfg.world.get(alias),
                    "read": read.as_str(),
                    "age_ms": age_ms,
                    "max_age_ms": self.cfg.world_max_age_secs.saturating_mul(1000),
                    "step": step,
                }));
            self.emit(d, tr.root.as_deref());
        }
    }

    /// The step's outcome event (module doc § Trace): the `StepOutcome`
    /// `apply` computed, as is — never re-derived here.
    fn trace_outcome(
        &self,
        outcome: &StepOutcome,
        entry: Option<&HistoryEntry>,
        session_id: &str,
        t: u64,
        tr: &StepTrace,
    ) {
        use StepOutcome as O;
        if self.trace.is_none() {
            return;
        }
        let action = match outcome {
            O::Executed { action }
            | O::Refused { action, .. }
            | O::DryRun { action }
            | O::Stopped { action }
            | O::Escalated { action, .. }
            | O::Rejected { action, .. } => action.as_str(),
            O::Error { .. } => return,
        };
        let action_node = node_id::action(&self.name, action);
        let ran = matches!(outcome, O::Executed { .. } | O::Refused { .. });
        let (kind, status, node, parent) = match outcome {
            O::Executed { .. } => {
                let ok = entry.and_then(|e| e.ok) == Some(true);
                let status = if ok { Status::Ok } else { Status::Failed };
                ("action.completed", status, action_node, &tr.selected)
            }
            O::Refused { .. } => ("action.refused", Status::Refused, action_node, &tr.selected),
            O::DryRun { .. } => ("action.selected", Status::Skipped, action_node, &tr.jev),
            O::Stopped { .. } => ("action.selected", Status::Ok, action_node, &tr.jev),
            O::Escalated { .. } => (
                "action.escalated",
                Status::Escalated,
                node_id::gate_act_at(&self.name),
                &tr.jev,
            ),
            O::Rejected { .. } => {
                let node = if self.cfg.actions.contains_key(action) {
                    action_node
                } else {
                    node_id::jev(&self.name)
                };
                ("action.rejected", Status::Refused, node, &tr.jev)
            }
            O::Error { .. } => return,
        };
        let mut p = json!({
            "action": action,
            "outcome": outcome,
            "confidence": tr.confidence,
            "t": t,
        });
        if let Some(e) = entry {
            merge(&mut p, json!({"args": e.args}));
        }
        if ran {
            merge(
                &mut p,
                json!({
                    "ok": entry.and_then(|e| e.ok),
                    "output": entry.map(|e| &e.result),
                    "obs": entry.and_then(|e| e.obs.as_ref()),
                }),
            );
        }
        if matches!(outcome, O::Escalated { .. }) {
            merge(
                &mut p,
                json!({
                    "act_at": self.cfg.act_at,
                    "escalate": self.cfg.escalate,
                    "escalator": self.escalator.is_some(),
                }),
            );
        }
        let mut d = self
            .draft(kind, Component::Action, status, session_id)
            .node(node)
            .payload(p);
        if ran {
            let call = self.call_id(session_id, t);
            if let Some(a) = &self.audit {
                d = d.artifact(a.path.display().to_string(), Some(call.clone()));
            }
            d = d.call(call);
        }
        self.emit(d, parent.as_deref());
    }

    fn record_metrics(&self, session_id: &str, state: &Value, d: &Decision, latency_ms: u64) {
        let prompt = state.to_string();
        crate::application::metrics::record(MetricsRecord {
            ts_unix: unix_secs(self.now_ms()),
            session_id: session_id.to_string(),
            kind: MetricsKind::Decision,
            agent: self.name.clone(),
            model: if d.model.is_empty() {
                self.engine.model().to_string()
            } else {
                d.model.clone()
            },
            prompt_tokens: d.usage.input_tokens,
            cached_input_tokens: 0,
            completion_tokens: d.usage.output_tokens,
            total_tokens: d.usage.input_tokens + d.usage.output_tokens,
            prompt_chars: prompt.chars().count() as u32,
            prompt_bytes: prompt.len() as u32,
            response_chars: 0,
            latency_ms,
            layers: Vec::new(),
            step_id: None,
        });
    }

    /// One JSONL line per decisions call; `d = None` = the call failed
    /// (`outcome = "error"`, no answers). `ts` (unix s) stays for old
    /// readers; `ts_ms`, `latency_ms` (the decisions call), `sandbox` and
    /// `act_at` join it; `call_id` when the step ran a tool (`Executed` /
    /// `Refused`) — the key its risk verdict carries; `trigger`,
    /// `runtime_id` and `run_id` when the `AuditLog` sets them (the trace
    /// recording of `tengu run` / `tengu decide`, `bootstrap/trace.rs`).
    /// `ts` / `ts_ms` come from the loop's clock.
    /// Fail-soft: audit errors only warn.
    fn audit(
        &self,
        session_id: &str,
        t: u64,
        d: Option<&Decision>,
        outcome: &StepOutcome,
        entry: Option<&HistoryEntry>,
        latency_ms: u64,
    ) {
        let Some(audit) = &self.audit else { return };
        let ran = matches!(
            outcome,
            StepOutcome::Executed { .. } | StepOutcome::Refused { .. }
        );
        let now = self.now_ms();
        let mut line = json!({
            "ts": unix_secs(now),
            "ts_ms": now,
            "loop": self.name,
            "sandbox": audit.sandbox,
            "session_id": session_id,
            "t": t,
            "call_id": ran.then(|| self.call_id(session_id, t)),
            "decision_id": d.map(|d| &d.id),
            // The build Jev reported; the configured slug when the call failed.
            "model": d.map_or(self.engine.model(), |d| d.model.as_str()),
            "act_at": self.cfg.act_at,
            "latency_ms": latency_ms,
            "answers": d.map(|d| &d.answers),
            "usage": d.map(|d| &d.usage),
            "result": outcome,
            "args": entry.map(|e| &e.args),
            // What the tool returned, as `history` holds it (reduced, redacted).
            "ok": entry.and_then(|e| e.ok),
            "output": entry.map(|e| &e.result),
            // Typed result meta (key / status / source / age / slot).
            "obs": entry.and_then(|e| e.obs.as_ref()),
        });
        if let Value::Object(o) = &mut line {
            for (k, v) in [
                ("trigger", &audit.trigger),
                ("runtime_id", &audit.ids.runtime_id),
                ("run_id", &audit.ids.run_id),
            ] {
                if let Some(v) = v {
                    o.insert(k.into(), json!(v));
                }
            }
        }
        if let Err(e) = append_line(&audit.path, &format!("{line}\n")) {
            warn!(path = %audit.path.display(), error = %e, "decision audit write failed");
        }
    }
}

/// Append one whole line (newline included) with a single `write_all` on an
/// append-mode file: concurrent writers never interleave inside a line.
/// `writeln!` over `serde_json::Value`'s `Display` issued one write per
/// token on the unbuffered `File`. Also the risk verdict mirror's writer
/// (`outbound/paper_store.rs`).
pub(crate) fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())
}

fn slot_key(action: &str, slot: &str) -> String {
    format!("{action}__{slot}")
}

/// A step's legal set as the trace carries it: action → slot → candidate
/// labels (`{}` for a slot-less action).
fn legal_json(legal: &BTreeMap<&str, BTreeMap<&str, Vec<Candidate>>>) -> Value {
    let actions = legal.iter().map(|(an, slots)| {
        let slots = slots.iter().map(|(sn, cands)| {
            let labels: Vec<&str> = cands.iter().map(|c| c.label.as_str()).collect();
            (sn.to_string(), json!(labels))
        });
        (an.to_string(), Value::Object(slots.collect()))
    });
    Value::Object(actions.collect())
}

/// `extra`'s fields into `base` (both objects; otherwise nothing).
fn merge(base: &mut Value, extra: Value) {
    if let (Value::Object(b), Value::Object(x)) = (base, extra) {
        b.extend(x);
    }
}

/// Whole seconds of `ms` since the epoch, 0 before it (as
/// `metrics::now_unix` floors the wall clock).
fn unix_secs(ms: i64) -> u64 {
    u64::try_from(ms.div_euclid(1000)).unwrap_or(0)
}

/// `Some(rule)` when a typed result says the `[risk]` gate refused the
/// order (`features.risk = "deny"`, rule `features.risk_rule`).
fn risk_refusal(o: &Observation) -> Option<String> {
    let deny = o.features.get("risk").and_then(Value::as_str) == Some("deny");
    deny.then(|| {
        let rule = o.features.get("risk_rule").and_then(Value::as_str);
        rule.unwrap_or("?").to_string()
    })
}

fn rejected(action: &str, reason: &str) -> StepOutcome {
    StepOutcome::Rejected {
        action: action.to_string(),
        reason: reason.to_string(),
    }
}

/// One audit line (see `DecisionLoop::audit`) as a short readable block:
/// the chosen action + confidence (a risk refusal names its rule), its slot
/// answers, the args it ran with, its call id and what came back; a failed
/// decisions call as one `decide failed` line. The TUI decision feed renders
/// it. `None` when the line has neither a `next_action` answer nor an
/// error. Values are never shortened.
pub(crate) fn render_audit(v: &Value) -> Option<String> {
    if v.pointer("/result/outcome").and_then(Value::as_str) == Some("error") {
        let reason = v.pointer("/result/reason").and_then(Value::as_str);
        return Some(format!(
            "jev {} #{} · decide failed → error: {}",
            v["loop"].as_str().unwrap_or("?"),
            v["t"],
            reason.unwrap_or("?")
        ));
    }
    let next = v.pointer("/answers/next_action")?;
    let action = next.get("choice")?.as_str()?;
    let conf = |a: &Value| match a.get("confidence").and_then(Value::as_f64) {
        Some(c) => format!("{c:.2}"),
        None => "?".into(),
    };
    let outcome = match v.pointer("/result/outcome").and_then(Value::as_str) {
        Some("escalated") => "escalated (below act_at)".to_string(),
        Some("rejected") => format!(
            "rejected: {}",
            v.pointer("/result/reason")
                .and_then(Value::as_str)
                .unwrap_or("?")
        ),
        Some("refused") => format!(
            "refused by the risk gate: {}",
            v.pointer("/result/rule")
                .and_then(Value::as_str)
                .unwrap_or("?")
        ),
        Some(o) => o.to_string(),
        None => "?".to_string(),
    };
    let mut out = format!(
        "jev {} #{} · {action} ({}) → {outcome}",
        v["loop"].as_str().unwrap_or("?"),
        v["t"],
        conf(next),
    );
    if let Some(Value::Object(answers)) = v.get("answers") {
        let prefix = format!("{action}__");
        for (key, a) in answers {
            if let Some(slot) = key.strip_prefix(&prefix) {
                let choice = a["choice"].as_str().unwrap_or("?");
                out += &format!("\n  {slot} = {choice} ({})", conf(a));
            }
        }
    }
    if let Some(args) = v.get("args").filter(|a| !a.is_null()) {
        out += &format!("\n  args   {args}");
    }
    if let Some(id) = v.get("call_id").and_then(Value::as_str) {
        out += &format!("\n  call   {id}");
    }
    if let Some(o) = v.get("output").filter(|o| !o.is_null()) {
        let label = match v.get("ok").and_then(Value::as_bool) {
            Some(true) => "ok",
            Some(false) => "failed",
            None => "result",
        };
        out += &format!("\n  {label:<6} {o}");
    }
    Some(out)
}

#[cfg(test)]
mod tests;
