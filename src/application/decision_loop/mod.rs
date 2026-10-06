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
//! History is in-process (lost on restart); events for one loop are
//! serialised by the state mutex so history stays ordered.
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

use crate::config::decision_loop::DecisionLoopConfig;
use crate::domain::decision::{Decision, HistoryEntry, Question, StepOutcome, Verdict};
use crate::domain::message::ToolCall;
use crate::domain::metrics::{MetricsKind, MetricsRecord};
use crate::domain::observation::{now_ms, Observation};
use crate::ports::clock::Clock;
use crate::ports::decision::{DecisionEngine, Escalator};
use crate::ports::engine::ToolExecutor;
use crate::ports::observation::ObservationStore;
use slots::Candidate;
use world::World;

const NEXT_ACTION: &str = "next_action";

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
    state: Mutex<LoopState>,
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
            state: Mutex::new(LoopState::default()),
        }
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
        let mut st = self.state.lock().await;
        st.event_start = st.t;
        let event = reduce::reduce(event, &self.cfg.event_reduce);
        let mut steps = Vec::new();
        for step in 0..self.cfg.max_steps {
            let (outcome, decision) = self.step(&mut st, &event, step, session_id).await?;
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
    ) -> Result<(StepOutcome, Decision)> {
        // 1. World, legal actions + their slot candidates.
        let world = World::read(self.observations.as_deref(), &self.cfg, self.now_ms()).await;
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
        let decision = match decided {
            Ok(d) => d,
            Err(e) => {
                let outcome = StepOutcome::Error {
                    reason: format!("{e:#}"),
                };
                self.audit(session_id, st.t, None, &outcome, None, latency_ms);
                return Err(e);
            }
        };
        self.record_metrics(session_id, &state, &decision, latency_ms);

        let t_before = st.t;
        let outcome = self.apply(st, &legal, &decision, &state, session_id).await;
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

        if self.cfg.dry_run && !action.read_only {
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
        let now = self.now_ms();
        let mut refused = None;
        let (ok, result, obs) = match self.tools.execute_typed(&call, &[]).await {
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
        let message = format!(
            "Decision loop `{}` is unsure (confidence {confidence:.2}) about the next step.\n\
             Proposed action: `{action}` with args {args}\n\
             Review the state, decide what should happen, and explain why. \
             Show every address, mint and signature in full.\n\nState:\n{}",
            self.name,
            serde_json::to_string_pretty(state).unwrap_or_default()
        );
        escalator.escalate(session_id.to_string(), message).await;
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
    /// `Refused`) — the key its risk verdict carries; `trigger` when the
    /// `AuditLog` sets one. `ts` / `ts_ms` come from the loop's clock.
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
        if let (Some(trigger), Value::Object(o)) = (&audit.trigger, &mut line) {
            o.insert("trigger".into(), json!(trigger));
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
mod tests {
    use super::*;
    use crate::domain::decision::Answer;
    use crate::domain::message::Message;
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;

    /// Scripted decision engine: returns queued decisions, records questions
    /// and states.
    struct Scripted {
        queue: StdMutex<VecDeque<Decision>>,
        seen: StdMutex<Vec<BTreeMap<String, Question>>>,
        states: StdMutex<Vec<Value>>,
    }

    impl Scripted {
        fn new(script: Vec<Decision>) -> Self {
            Self {
                queue: StdMutex::new(script.into()),
                seen: StdMutex::new(vec![]),
                states: StdMutex::new(vec![]),
            }
        }
    }

    #[async_trait]
    impl DecisionEngine for Scripted {
        fn model(&self) -> &str {
            "test"
        }
        async fn decide(&self, s: &Value, q: &BTreeMap<String, Question>) -> Result<Decision> {
            self.seen.lock().unwrap().push(q.clone());
            self.states.lock().unwrap().push(s.clone());
            Ok(self
                .queue
                .lock()
                .unwrap()
                .pop_front()
                .expect("no scripted decision"))
        }
    }

    struct FakeTools(StdMutex<Vec<ToolCall>>);

    #[async_trait]
    impl ToolExecutor for FakeTools {
        async fn execute(&self, call: &ToolCall, _m: &[Message]) -> Result<String> {
            self.0.lock().unwrap().push(call.clone());
            Ok("HTTP 200 https://x\n{\"pools\":[{\"address\":\"A1\",\"fees\":9},{\"address\":\"B2\",\"fees\":1}]}".into())
        }
    }

    struct FakeEscalator(StdMutex<Vec<String>>);

    #[async_trait]
    impl Escalator for FakeEscalator {
        async fn escalate(&self, _s: String, m: String) {
            self.0.lock().unwrap().push(m);
        }
    }

    fn pick(pairs: &[(&str, &str, f64)]) -> Decision {
        Decision {
            answers: pairs
                .iter()
                .map(|(k, c, conf)| {
                    (
                        k.to_string(),
                        Answer {
                            kind: "choice".into(),
                            choice: Some(c.to_string()),
                            confidence: Some(*conf),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    fn cfg(dry_run: bool) -> DecisionLoopConfig {
        toml::from_str(&format!(
            r#"
goal = "open best pool, max 2"
agent = "a"
dry_run = {dry_run}
[actions.hold]
description = "nothing"
[actions.fetch]
description = "fetch pools"
tool = "http_request"
read_only = true
args = {{ method = "GET", url = "https://x/pools" }}
reduce = {{ pools = "/pools/*/{{address,fees}}" }}
[actions.open]
description = "open position"
tool = "open_position"
args = {{ pool = "{{pool}}", size = "{{size}}" }}
slots = {{ pool = {{ from = "fetch", items = "/pools/*", value = "address" }}, size = [0.5, 1, 2, 3] }}
caps = {{ size = 2.0 }}
"#
        ))
        .unwrap()
    }

    fn build(
        dry_run: bool,
        script: Vec<Decision>,
    ) -> (
        DecisionLoop,
        Arc<Scripted>,
        Arc<FakeTools>,
        Arc<FakeEscalator>,
    ) {
        let engine = Arc::new(Scripted::new(script));
        let tools = Arc::new(FakeTools(StdMutex::new(vec![])));
        let esc = Arc::new(FakeEscalator(StdMutex::new(vec![])));
        let l = DecisionLoop::new(
            "t",
            cfg(dry_run),
            engine.clone(),
            tools.clone(),
            None,
            Some(esc.clone()),
            None,
        );
        (l, engine, tools, esc)
    }

    // ── tool-call ids ─────────────────────────────────────────────────

    fn fetch_then_hold() -> Vec<Decision> {
        vec![
            pick(&[("next_action", "fetch", 0.95)]),
            pick(&[("next_action", "hold", 0.99)]),
        ]
    }

    /// `{loop}:{session_id}:{t}`: two sessions never share an id, and a
    /// restarted loop (`t` from 0 again) never repeats an earlier one.
    #[tokio::test]
    async fn call_ids_carry_the_session_and_survive_restarts() {
        let (l, _, tools, _) = build(false, [fetch_then_hold(), fetch_then_hold()].concat());
        l.handle_event(&json!({}), "webhook-a").await.unwrap();
        l.handle_event(&json!({}), "webhook-b").await.unwrap();
        let (restarted, _, after, _) = build(false, fetch_then_hold());
        restarted
            .handle_event(&json!({}), "webhook-c")
            .await
            .unwrap();
        let ids: Vec<String> = tools
            .0
            .lock()
            .unwrap()
            .iter()
            .chain(after.0.lock().unwrap().iter())
            .map(|c| c.id.clone())
            .collect();
        assert_eq!(ids, ["t:webhook-a:1", "t:webhook-b:3", "t:webhook-c:1"]);
    }

    /// Through the real executor the id reaches the tool as `ToolCtx.call_id`.
    #[tokio::test]
    async fn loop_call_id_reaches_the_tool_ctx() {
        use crate::application::tools::registry::{PluginToolExecutor, ToolRegistry};
        use crate::domain::message::ToolDef;
        use crate::ports::tool::{Tool, ToolCtx};

        struct Recorder {
            def: ToolDef,
            seen: Arc<StdMutex<Vec<Option<String>>>>,
        }
        #[async_trait]
        impl Tool for Recorder {
            fn definition(&self) -> &ToolDef {
                &self.def
            }
            async fn execute(&self, _args: &Value, ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
                // scope: pure-compute
                self.seen
                    .lock()
                    .unwrap()
                    .push(ctx.call_id.map(str::to_string));
                Ok(ToolOutput::from(
                    "HTTP 200 https://x\n{\"pools\":[]}".to_string(),
                ))
            }
        }
        struct Quiet;
        impl crate::ports::tool_activity::ToolActivityPort for Quiet {
            fn publish_tool_activity(&self, _call: &ToolCall) {}
        }

        let seen = Arc::new(StdMutex::new(vec![]));
        let mut registry = ToolRegistry::new();
        registry.register_tool(Arc::new(Recorder {
            def: ToolDef::new("http_request", "d", json!({})),
            seen: Arc::clone(&seen),
        }));
        let exec = PluginToolExecutor {
            registry,
            workspace: PathBuf::from("."),
            shell: Arc::new(crate::adapters::outbound::shell::LocalShellExecutor::new()),
            http: reqwest::Client::new(),
            memory_manager: None,
            secret_registry: Arc::new(crate::domain::secrets::SecretRegistry::new()),
            activity: Arc::new(Quiet),
            scopes: Default::default(),
            agent_config: None,
        };
        let l = DecisionLoop::new(
            "exec",
            cfg(false),
            Arc::new(Scripted::new(fetch_then_hold())),
            Arc::new(exec),
            None,
            None,
            None,
        );
        l.handle_event(&json!({}), "decide-exec-7").await.unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [Some("exec:decide-exec-7:1".to_string())]
        );
    }

    // ── typed tools, world, requires, FromObservation ─────────────────

    use crate::application::observe::tests::MemStore;
    use crate::domain::observation::{ErrorClass, ObsSource, ObsStatus, Observation, ReadError};
    use crate::ports::tool::ToolOutput;

    const MINT: &str = "So11111111111111111111111111111111111111112";
    const POOL_A: &str = "5rCf1DM8LjKTw4YqhnoLcngyZYeNnQqztScTogYHAS6";
    const POOL_B: &str = "BGm1tav58oGcsQJehL9WXBFXF7D27vZsKefj4xJKD5Y";
    const POOLS_KEY: &str = "dlmm_pools/1:SOL-USDC|fee_tvl_24h|10|100000";

    fn price_obs(observed_at_ms: i64, status: ObsStatus) -> Observation {
        Observation {
            key: format!("price_oracle/1:{MINT}"),
            schema: "price_oracle/1".into(),
            tool: "sol_price".into(),
            observed_at_ms,
            slot: None,
            ttl_ms: 10_000,
            source: ObsSource::Live,
            status,
            errors: if status == ObsStatus::Error {
                vec![ReadError::new(
                    "usd",
                    ErrorClass::Timeout,
                    "no source answered",
                )]
            } else {
                vec![]
            },
            headline: format!("sol_price {MINT} usd=150.25"),
            features: [("usd".to_string(), json!(150.25))].into(),
            data: json!({"mint": MINT, "usd": 150.25}),
        }
    }

    fn pools_obs(observed_at_ms: i64) -> Observation {
        Observation {
            key: POOLS_KEY.into(),
            schema: "dlmm_pools/1".into(),
            tool: "dlmm_pools".into(),
            observed_at_ms,
            slot: None,
            ttl_ms: 60_000,
            source: ObsSource::Live,
            status: ObsStatus::Ok,
            errors: vec![],
            headline: "dlmm_pools SOL-USDC n=2".into(),
            features: [("n_pools".to_string(), json!(2))].into(),
            data: json!({"pools": [{"address": POOL_A, "tvl_usd": 7.0e6}, {"address": POOL_B, "tvl_usd": 1.0e6}]}),
        }
    }

    /// Typed executor: every call returns a price observation with `status`.
    struct TypedTools {
        calls: StdMutex<Vec<ToolCall>>,
        status: ObsStatus,
    }

    #[async_trait]
    impl ToolExecutor for TypedTools {
        async fn execute(&self, call: &ToolCall, m: &[Message]) -> Result<String> {
            Ok(self.execute_typed(call, m).await?.text)
        }
        async fn execute_typed(&self, call: &ToolCall, _m: &[Message]) -> Result<ToolOutput> {
            self.calls.lock().unwrap().push(call.clone());
            let now = now_ms();
            Ok(ToolOutput::observed(price_obs(now, self.status), now))
        }
    }

    fn typed_cfg() -> DecisionLoopConfig {
        toml::from_str(&format!(
            r#"
goal = "keep the LP safe"
agent = "a"
dry_run = false
world = {{ price = "price_oracle/1:{MINT}", pools = "{POOLS_KEY}" }}
[actions.hold]
description = "nothing"
[actions.refresh]
description = "refresh price"
tool = "sol_price"
read_only = true
args = {{ mint = "{MINT}" }}
[actions.refresh_reduced]
description = "refresh price, reduced"
tool = "sol_price"
read_only = true
reduce = {{ usd = "/features/usd", mint = "/data/mint" }}
[actions.open]
description = "open position"
tool = "dlmm_open_position"
requires = {{ price = 30 }}
args = {{ pool = "{{pool}}" }}
slots = {{ pool = {{ observation = "pools", items = "/data/pools/*", value = "address" }} }}
"#
        ))
        .unwrap()
    }

    fn build_typed(
        script: Vec<Decision>,
        status: ObsStatus,
        store: Option<Arc<MemStore>>,
    ) -> (DecisionLoop, Arc<Scripted>, Arc<TypedTools>) {
        let engine = Arc::new(Scripted::new(script));
        let tools = Arc::new(TypedTools {
            calls: StdMutex::new(vec![]),
            status,
        });
        let store = store.map(|s| s as Arc<dyn ObservationStore>);
        let l = DecisionLoop::new(
            "t",
            typed_cfg(),
            engine.clone(),
            tools.clone(),
            store,
            None,
            None,
        );
        (l, engine, tools)
    }

    fn legal_actions(engine: &Scripted, step: usize) -> Vec<String> {
        let seen = engine.seen.lock().unwrap();
        let Question::Choice { criteria, .. } = &seen[step][NEXT_ACTION] else {
            panic!()
        };
        criteria.keys().cloned().collect()
    }

    #[tokio::test]
    async fn typed_result_lands_in_history_with_obs_meta() {
        let (l, _, tools) = build_typed(
            vec![
                pick(&[("next_action", "refresh", 0.95)]),
                pick(&[("next_action", "refresh_reduced", 0.95)]),
                pick(&[("next_action", "hold", 0.99)]),
            ],
            ObsStatus::Ok,
            None,
        );
        l.handle_event(&json!({}), "s").await.unwrap();
        assert_eq!(
            tools.calls.lock().unwrap()[0].arguments,
            json!({"mint": MINT})
        );
        let st = l.state.lock().await;
        let h = &st.history[0];
        assert_eq!(h.ok, Some(true));
        assert_eq!(h.result["features"]["usd"], json!(150.25));
        assert_eq!(h.result["status"], json!("ok"));
        assert!(h.result.get("data").is_none(), "unreduced = features only");
        let meta = h.obs.as_ref().expect("typed result carries obs meta");
        assert_eq!(meta.key, format!("price_oracle/1:{MINT}"));
        assert_eq!(meta.source, ObsSource::Live);
        // The reducer addresses `{.., features, data}`.
        let r = &st.history[1];
        assert_eq!(r.result, json!({"usd": 150.25, "mint": MINT}));
        assert!(st.history[2].obs.is_none(), "terminal action has no obs");
    }

    #[tokio::test]
    async fn typed_error_observation_is_not_ok() {
        let (l, _, _) = build_typed(
            vec![
                pick(&[("next_action", "refresh", 0.95)]),
                pick(&[("next_action", "hold", 0.99)]),
            ],
            ObsStatus::Error,
            None,
        );
        l.handle_event(&json!({}), "s").await.unwrap();
        let st = l.state.lock().await;
        assert_eq!(st.history[0].ok, Some(false));
        assert_eq!(st.history[0].obs.as_ref().unwrap().status, ObsStatus::Error);
        assert_eq!(st.history[0].result["errors"][0]["class"], json!("timeout"));
    }

    #[tokio::test]
    async fn requires_hides_action_while_world_is_stale() {
        let store = Arc::new(MemStore::default());
        store
            .put(&price_obs(now_ms() - 60_000, ObsStatus::Ok))
            .await
            .unwrap();
        store.put(&pools_obs(now_ms())).await.unwrap();
        let (l, engine, _) = build_typed(
            vec![pick(&[("next_action", "hold", 0.99)])],
            ObsStatus::Ok,
            Some(store),
        );
        l.handle_event(&json!({}), "s").await.unwrap();
        assert!(!legal_actions(&engine, 0).contains(&"open".to_string()));
        let states = engine.states.lock().unwrap();
        let world = &states[0]["world"];
        assert_eq!(world["price"]["status"], json!("stale"));
        assert!(
            !world["price"].to_string().contains("150.25"),
            "stale entry carries no numbers: {world}"
        );
        assert_eq!(world["pools"]["features"]["n_pools"], json!(2));
    }

    #[tokio::test]
    async fn observation_slot_offers_fresh_world_items() {
        let store = Arc::new(MemStore::default());
        store
            .put(&price_obs(now_ms(), ObsStatus::Ok))
            .await
            .unwrap();
        store.put(&pools_obs(now_ms())).await.unwrap();
        let (l, engine, tools) = build_typed(
            vec![
                pick(&[("next_action", "open", 0.95), ("open__pool", "pool_2", 0.9)]),
                pick(&[("next_action", "hold", 0.99)]),
            ],
            ObsStatus::Ok,
            Some(store),
        );
        l.handle_event(&json!({}), "s").await.unwrap();
        assert!(legal_actions(&engine, 0).contains(&"open".to_string()));
        let calls = tools.calls.lock().unwrap();
        assert_eq!(calls[0].name, "dlmm_open_position");
        assert_eq!(calls[0].arguments, json!({"pool": POOL_B}), "full address");
    }

    #[tokio::test]
    async fn world_is_omitted_without_aliases() {
        let (l, engine, _, _) = build(false, vec![pick(&[("next_action", "hold", 0.99)])]);
        l.handle_event(&json!({}), "s").await.unwrap();
        assert!(engine.states.lock().unwrap()[0].get("world").is_none());
    }

    #[tokio::test]
    async fn fetch_then_open_with_history_slots() {
        let (l, engine, tools, _) = build(
            false,
            vec![
                pick(&[("next_action", "fetch", 0.95)]),
                pick(&[
                    ("next_action", "open", 0.9),
                    ("open__pool", "pool_1", 0.93),
                    ("open__size", "2", 0.97),
                ]),
                pick(&[("next_action", "hold", 0.99)]),
            ],
        );
        let out = l.handle_event(&json!({"sig": "x"}), "s").await.unwrap();
        assert_eq!(
            out,
            vec![
                StepOutcome::Executed {
                    action: "fetch".into()
                },
                StepOutcome::Executed {
                    action: "open".into()
                },
                StepOutcome::Stopped {
                    action: "hold".into()
                },
            ]
        );
        // Step 1: `open` not legal yet (no fetch result) → not offered.
        let seen = engine.seen.lock().unwrap();
        let Question::Choice { criteria, .. } = &seen[0][NEXT_ACTION] else {
            panic!()
        };
        assert!(!criteria.contains_key("open"));
        // Step 2: pool candidates come from the fetch result; size capped at 2.
        let Question::Choice { criteria, .. } = &seen[1]["open__size"] else {
            panic!()
        };
        assert!(!criteria.contains_key("3"));
        let calls = tools.0.lock().unwrap();
        assert_eq!(calls[1].name, "open_position");
        assert_eq!(calls[1].arguments, json!({"pool": "A1", "size": 2}));
    }

    #[tokio::test]
    async fn history_slots_never_reuse_an_earlier_events_result() {
        let (l, engine, tools, _) = build(
            false,
            vec![
                // Event 1: fetch succeeds, then hold.
                pick(&[("next_action", "fetch", 0.95)]),
                pick(&[("next_action", "hold", 0.99)]),
                // Event 2: no fetch yet — the event-1 pools must not feed `open`.
                pick(&[
                    ("next_action", "open", 0.99),
                    ("open__pool", "pool_1", 0.99),
                    ("open__size", "1", 0.99),
                ]),
            ],
        );
        l.handle_event(&json!({"n": 1}), "s").await.unwrap();
        assert!(legal_actions(&engine, 1).contains(&"open".to_string()));
        let out = l.handle_event(&json!({"n": 2}), "s").await.unwrap();
        assert!(!legal_actions(&engine, 2).contains(&"open".to_string()));
        assert!(
            matches!(&out[0], StepOutcome::Rejected { action, .. } if action == "open"),
            "{out:?}"
        );
        assert_eq!(tools.0.lock().unwrap().len(), 1, "only event 1's fetch ran");
        // The earlier event's entries stay in `state.history` as context.
        let states = engine.states.lock().unwrap();
        assert_eq!(states[2]["history"][0]["action"], json!("fetch"));
    }

    #[tokio::test]
    async fn dry_run_blocks_write_actions_and_goes_on() {
        let (l, _, tools, _) = build(
            true,
            vec![
                pick(&[("next_action", "fetch", 0.95)]),
                pick(&[
                    ("next_action", "open", 0.9),
                    ("open__pool", "pool_2", 0.9),
                    ("open__size", "1", 0.9),
                ]),
                pick(&[("next_action", "hold", 0.99)]),
            ],
        );
        let out = l.handle_event(&json!({}), "s").await.unwrap();
        assert_eq!(
            out[1..],
            [
                StepOutcome::DryRun {
                    action: "open".into()
                },
                StepOutcome::Stopped {
                    action: "hold".into()
                }
            ]
        );
        assert_eq!(
            tools.0.lock().unwrap().len(),
            1,
            "only the read-only fetch ran"
        );
    }

    /// A chain bound end to end: the event (what a higher-order agent hands
    /// the loop) fills `wallet` / `size`, the earlier `fetch` fills `pool` —
    /// no slot question reaches Jev, and `open` is illegal until every
    /// binding resolves.
    fn chain(
        dry_run: bool,
        script: Vec<Decision>,
    ) -> (DecisionLoop, Arc<Scripted>, Arc<FakeTools>) {
        let cfg: DecisionLoopConfig = toml::from_str(&format!(
            r#"
goal = "fetch, then open what the event asks for"
agent = "a"
dry_run = {dry_run}
[actions.hold]
description = "nothing"
[actions.fetch]
description = "fetch pools"
tool = "http_request"
read_only = true
args = {{ method = "GET", url = "https://x/pools" }}
[actions.open]
description = "open position"
tool = "open_position"
args = {{ wallet = "{{wallet}}", pool = "{{pool}}", size = "{{size}}", mode = "simulate" }}
slots = {{ wallet = {{ event = "/wallet" }}, size = {{ event = "/size" }}, pool = {{ from = "fetch", path = "/pools/0/address" }} }}
caps = {{ size = 2.0 }}
"#
        ))
        .unwrap();
        assert!(cfg.validation_errors("chain").is_empty());
        let engine = Arc::new(Scripted::new(script));
        let tools = Arc::new(FakeTools(StdMutex::new(vec![])));
        let l = DecisionLoop::new(
            "chain",
            cfg,
            engine.clone(),
            tools.clone(),
            None,
            None,
            None,
        );
        (l, engine, tools)
    }

    fn fetch_open_hold() -> Vec<Decision> {
        vec![
            pick(&[("next_action", "fetch", 0.95)]),
            pick(&[("next_action", "open", 0.95)]),
            pick(&[("next_action", "hold", 0.99)]),
        ]
    }

    #[tokio::test]
    async fn bound_slots_fill_args_from_event_and_history() {
        let (l, engine, tools) = chain(false, fetch_open_hold());
        let out = l
            .handle_event(&json!({"wallet": "W1", "size": 1.5}), "s")
            .await
            .unwrap();
        assert_eq!(out.len(), 3, "{out:?}");
        assert!(!legal_actions(&engine, 0).contains(&"open".to_string()));
        assert!(legal_actions(&engine, 1).contains(&"open".to_string()));
        let seen = engine.seen.lock().unwrap();
        assert!(
            seen[1].keys().all(|k| !k.starts_with("open__")),
            "bound slots ask nothing: {:?}",
            seen[1].keys()
        );
        let calls = tools.0.lock().unwrap();
        assert_eq!(
            calls[1].arguments,
            json!({"wallet": "W1", "pool": "A1", "size": 1.5, "mode": "simulate"})
        );
    }

    #[tokio::test]
    async fn unresolved_or_capped_bindings_keep_the_action_illegal() {
        for event in [
            json!({"wallet": "W1", "size": 3}), // above the cap
            json!({"size": 1}),                 // no wallet
            json!({"wallet": null, "size": 1}), // null is nothing
        ] {
            let (l, engine, tools) = chain(
                false,
                vec![
                    pick(&[("next_action", "fetch", 0.95)]),
                    pick(&[("next_action", "hold", 0.99)]),
                ],
            );
            l.handle_event(&event, "s").await.unwrap();
            assert!(
                !legal_actions(&engine, 1).contains(&"open".to_string()),
                "{event}"
            );
            assert_eq!(tools.0.lock().unwrap().len(), 1, "{event}");
        }
    }

    #[tokio::test]
    async fn dry_run_walks_the_chain_without_its_write() {
        let (l, _, tools) = chain(true, fetch_open_hold());
        let out = l
            .handle_event(&json!({"wallet": "W1", "size": 1}), "s")
            .await
            .unwrap();
        assert_eq!(
            out,
            [
                StepOutcome::Executed {
                    action: "fetch".into()
                },
                StepOutcome::DryRun {
                    action: "open".into()
                },
                StepOutcome::Stopped {
                    action: "hold".into()
                },
            ]
        );
        assert_eq!(tools.0.lock().unwrap().len(), 1, "the write never ran");
        let h = l.history().await;
        assert_eq!(h[1].args["pool"], json!("A1"), "dry-run args are resolved");
    }

    #[tokio::test]
    async fn low_slot_confidence_escalates() {
        let (l, _, tools, esc) = build(
            false,
            vec![
                pick(&[("next_action", "fetch", 0.95)]),
                pick(&[
                    ("next_action", "open", 0.9),
                    ("open__pool", "pool_1", 0.4),
                    ("open__size", "1", 0.9),
                ]),
            ],
        );
        let out = l.handle_event(&json!({}), "s").await.unwrap();
        assert!(
            matches!(out.last(), Some(StepOutcome::Escalated { confidence, .. }) if *confidence == 0.4)
        );
        assert_eq!(tools.0.lock().unwrap().len(), 1);
        let msgs = esc.0.lock().unwrap();
        assert!(
            msgs[0].contains("\"A1\""),
            "escalation shows the full resolved value"
        );
    }

    // ── audit ──────────────────────────────────────────────────────────

    /// The decisions endpoint failing (402, timeout, 5xx).
    struct Failing;

    #[async_trait]
    impl DecisionEngine for Failing {
        fn model(&self) -> &str {
            "~typesafe/jev-latest"
        }
        async fn decide(&self, _s: &Value, _q: &BTreeMap<String, Question>) -> Result<Decision> {
            Err(anyhow::anyhow!("decisions HTTP 402: insufficient credits"))
        }
    }

    fn audited(name: &str, engine: Arc<dyn DecisionEngine>, path: &Path) -> DecisionLoop {
        DecisionLoop::new(
            name,
            cfg(false),
            engine,
            Arc::new(FakeTools(StdMutex::new(vec![]))),
            None,
            None,
            Some(AuditLog {
                path: path.to_path_buf(),
                sandbox: Some("xmarket".into()),
                trigger: None,
            }),
        )
    }

    /// 4 writers × 500 lines into one file (loops of one process; other
    /// processes append the same way): every line parses whole.
    #[test]
    fn concurrent_audit_lines_never_interleave() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("decisions.jsonl");
        let loops: Vec<DecisionLoop> = (0..4)
            .map(|i| audited(&format!("loop{i}"), Arc::new(Scripted::new(vec![])), &path))
            .collect();
        let d = pick(&[
            ("next_action", "open", 0.91),
            ("open__pool", "pool_2", 0.88),
            ("open__size", "2", 0.97),
        ]);
        let entry = HistoryEntry {
            t: 1,
            action: "open".into(),
            args: json!({"pool": POOL_B, "size": 2}),
            ok: Some(true),
            result: json!({"pools": (0..40).map(|i| json!({"address": POOL_A, "fees": i})).collect::<Vec<_>>()}),
            obs: None,
        };
        let outcome = StepOutcome::Executed {
            action: "open".into(),
        };
        let (d, entry, outcome) = (&d, &entry, &outcome);
        std::thread::scope(|s| {
            for l in &loops {
                s.spawn(move || {
                    for t in 0..500 {
                        l.audit("s", t, Some(d), outcome, Some(entry), 7);
                    }
                });
            }
        });
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2000);
        let mut per_loop: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for line in text.lines() {
            let v: Value = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("torn audit line ({e}): {line}"));
            assert_eq!(v["output"]["pools"].as_array().unwrap().len(), 40);
            per_loop
                .entry(v["loop"].as_str().unwrap().to_string())
                .or_default()
                .push(v["t"].as_u64().unwrap());
        }
        assert_eq!(per_loop.len(), 4);
        for (name, mut ts) in per_loop {
            ts.sort_unstable();
            assert_eq!(ts, (0..500).collect::<Vec<u64>>(), "{name}");
        }
    }

    /// Old keys stay, ms fields join; a failed decisions call leaves one
    /// `error` line, then its error propagates; the feed renders it.
    #[tokio::test]
    async fn audit_lines_for_decided_and_failed_calls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.jsonl");
        let hold = Arc::new(Scripted::new(vec![pick(&[("next_action", "hold", 0.99)])]));
        audited("t", hold, &path)
            .handle_event(&json!({}), "s1")
            .await
            .unwrap();
        let err = audited("t", Arc::new(Failing), &path)
            .handle_event(&json!({}), "s2")
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("402"), "{err:#}");

        let lines: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        let (ok, failed) = (&lines[0], &lines[1]);
        for key in [
            "ts",
            "ts_ms",
            "loop",
            "sandbox",
            "session_id",
            "t",
            "call_id",
            "decision_id",
            "model",
            "act_at",
            "latency_ms",
            "answers",
            "usage",
            "result",
            "args",
            "ok",
            "output",
            "obs",
        ] {
            assert!(ok.get(key).is_some(), "{key}: {ok}");
            assert!(failed.get(key).is_some(), "{key}: {failed}");
        }
        assert_eq!(
            ok["result"],
            json!({"outcome": "stopped", "action": "hold"})
        );
        assert!(ok["call_id"].is_null(), "a terminal step ran no tool");
        assert_eq!(ok["sandbox"], json!("xmarket"));
        assert_eq!(ok["act_at"], json!(0.8));
        assert!(ok["ts_ms"].as_u64().unwrap() >= ok["ts"].as_u64().unwrap() * 1000);
        assert!(ok["latency_ms"].is_u64());

        assert_eq!(
            failed["result"],
            json!({"outcome": "error", "reason": "decisions HTTP 402: insufficient credits"})
        );
        assert_eq!(failed["session_id"], json!("s2"));
        assert!(failed["answers"].is_null() && failed["decision_id"].is_null());
        assert_eq!(failed["model"], json!("~typesafe/jev-latest"));
        assert_eq!(
            render_audit(failed).unwrap(),
            "jev t #0 · decide failed → error: decisions HTTP 402: insufficient credits"
        );
    }

    // ── risk refusals + the verdict join ──────────────────────────────

    const TSLA: &str = "hyperliquid:xyz:TSLA";
    const OPP: &str = "xm_compare/1:hyperliquid:xyz:TSLA:hyperliquid:xyz:TSLA";

    /// `enter_small` ($20) and `enter_big` ($30) run `paper_order`; `done`
    /// stops.
    fn entry_cfg() -> DecisionLoopConfig {
        let args = |usd: u32| {
            format!(
                "{{ instrument = \"{TSLA}\", side = \"buy\", notional_usd = {usd}, kind = \
                 \"market\", max_slippage_bps = 30, opportunity = \"{OPP}\" }}"
            )
        };
        toml::from_str(&format!(
            r#"
goal = "enter TSLA within the budget"
agent = "exec"
dry_run = false
[actions.done]
description = "stop"
[actions.enter_small]
description = "buy $20 of TSLA"
tool = "paper_order"
args = {}
[actions.enter_big]
description = "buy $30 of TSLA"
tool = "paper_order"
args = {}
"#,
            args(20),
            args(30)
        ))
        .unwrap()
    }

    /// Every call returns the `paper_fill/1` row of an order the gate denied
    /// `min_edge`.
    struct DenyingTools;

    #[async_trait]
    impl ToolExecutor for DenyingTools {
        async fn execute(&self, call: &ToolCall, m: &[Message]) -> Result<String> {
            Ok(self.execute_typed(call, m).await?.text)
        }
        async fn execute_typed(&self, call: &ToolCall, _m: &[Message]) -> Result<ToolOutput> {
            let now = now_ms();
            let features = [
                ("risk", "deny"),
                ("risk_rule", "min_edge"),
                ("status", "denied"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect();
            let obs = Observation {
                key: format!("paper_fill/1:xmarket:{}", call.id),
                schema: "paper_fill/1".into(),
                tool: "paper_order".into(),
                observed_at_ms: now,
                slot: None,
                ttl_ms: 0,
                source: ObsSource::Live,
                status: ObsStatus::Error,
                errors: vec![ReadError::new(
                    "risk",
                    ErrorClass::NotApplicable,
                    "denied min_edge: edge 4 bps < 10 bps",
                )],
                headline: format!("paper_fill denied buy {TSLA} risk=deny rule=min_edge"),
                features,
                data: Value::Null,
            };
            Ok(ToolOutput::observed(obs, now))
        }
    }

    /// A gate denial is `Refused` (counted apart from `Executed`), the loop
    /// goes on; the audit line names the rule and the call; the feed shows it.
    #[tokio::test]
    async fn a_gate_denial_is_refused_and_the_loop_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.jsonl");
        let l = DecisionLoop::new(
            "entry",
            entry_cfg(),
            Arc::new(Scripted::new(vec![
                pick(&[("next_action", "enter_small", 0.95)]),
                pick(&[("next_action", "done", 0.99)]),
            ])),
            Arc::new(DenyingTools),
            None,
            None,
            Some(AuditLog {
                path: path.clone(),
                sandbox: None,
                trigger: None,
            }),
        );
        let out = l.handle_event(&json!({}), "s").await.unwrap();
        assert_eq!(
            out,
            vec![
                StepOutcome::Refused {
                    action: "enter_small".into(),
                    rule: "min_edge".into()
                },
                StepOutcome::Stopped {
                    action: "done".into()
                },
            ]
        );
        assert_eq!(l.history().await[0].ok, Some(false));
        let line: Value = serde_json::from_str(
            std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            line["result"],
            json!({"outcome": "refused", "action": "enter_small", "rule": "min_edge"})
        );
        assert_eq!(line["call_id"], json!("entry:s:1"));
        let text = render_audit(&line).unwrap();
        assert!(
            text.starts_with(
                "jev entry #1 · enter_small (0.95) → refused by the risk gate: min_edge"
            ),
            "{text}"
        );
        assert!(text.contains("\n  call   entry:s:1"), "{text}");
    }

    /// Runs `paper_order` through the real `run_exec` on an xm test rig
    /// (scripted clock + book, ledger mirroring to `risk.jsonl`), keyed by
    /// the loop's call id.
    struct RigTools(crate::adapters::outbound::tools::xm::exec_common::tests::Rig);

    #[async_trait]
    impl ToolExecutor for RigTools {
        async fn execute(&self, call: &ToolCall, m: &[Message]) -> Result<String> {
            Ok(self.execute_typed(call, m).await?.text)
        }
        async fn execute_typed(&self, call: &ToolCall, _m: &[Message]) -> Result<ToolOutput> {
            let limits = self.0.shared.risk.as_ref().unwrap().limits();
            let order =
                crate::adapters::outbound::tools::xm::paper::parse_order(&call.arguments, limits)?;
            let obs = self.0.run(order, &call.id).await?;
            Ok(ToolOutput::observed(obs, now_ms()))
        }
    }

    /// Tracker convention 9: the decision audit line of a step that placed
    /// an order and the order's risk verdict (`risk.jsonl`, the ledger row)
    /// share the call id `{loop}:{session}:{t}` — allowed and denied alike.
    #[tokio::test]
    async fn loop_audit_and_risk_verdicts_join_by_call_id() {
        use crate::adapters::outbound::tools::xm::exec_common::tests::Rig;
        use crate::ports::paper::PaperLedger;

        let rig = Rig::new(25).await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.jsonl");
        let tools = Arc::new(RigTools(rig));
        let l = DecisionLoop::new(
            "entry",
            entry_cfg(),
            Arc::new(Scripted::new(vec![
                pick(&[("next_action", "enter_small", 0.95)]),
                pick(&[("next_action", "enter_big", 0.95)]),
                pick(&[("next_action", "done", 0.99)]),
            ])),
            tools.clone(),
            None,
            None,
            Some(AuditLog {
                path: path.clone(),
                sandbox: Some("xmarket".into()),
                trigger: None,
            }),
        );
        let out = l
            .handle_event(&json!({}), "0001318605-26-000123")
            .await
            .unwrap();
        assert_eq!(
            out[..2],
            [
                StepOutcome::Executed {
                    action: "enter_small".into()
                },
                StepOutcome::Refused {
                    action: "enter_big".into(),
                    rule: "order_notional".into()
                },
            ]
        );
        let audit: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let risk = tools.0.risk_lines();
        assert_eq!(risk.len(), 2, "one verdict per order");
        let joined: Vec<(String, String, String)> = audit
            .iter()
            .filter_map(|a| {
                let call = a["call_id"].as_str()?;
                let r = risk.iter().find(|r| r["call_id"] == call)?;
                Some((
                    call.to_string(),
                    a["result"]["outcome"].as_str()?.to_string(),
                    r["verdict"].as_str()?.to_string(),
                ))
            })
            .collect();
        assert_eq!(
            joined,
            [
                (
                    "entry:0001318605-26-000123:1".to_string(),
                    "executed".to_string(),
                    "allow".to_string()
                ),
                (
                    "entry:0001318605-26-000123:2".to_string(),
                    "refused".to_string(),
                    "deny".to_string()
                ),
            ]
        );
        // The ledger rows carry the same keys.
        let rows = tools.0.ledger.decisions("xmarket", 10).await.unwrap();
        let mut calls: Vec<&str> = rows.iter().filter_map(|d| d.call_id.as_deref()).collect();
        calls.sort_unstable();
        assert_eq!(
            calls,
            [
                "entry:0001318605-26-000123:1",
                "entry:0001318605-26-000123:2"
            ]
        );
    }

    #[tokio::test]
    async fn illegal_choice_is_rejected() {
        let (l, _, tools, _) = build(false, vec![pick(&[("next_action", "open", 0.99)])]);
        let out = l.handle_event(&json!({}), "s").await.unwrap();
        assert!(matches!(&out[0], StepOutcome::Rejected { action, .. } if action == "open"));
        assert!(tools.0.lock().unwrap().is_empty());
    }

    #[test]
    fn render_audit_shows_action_slots_args_and_output() {
        let url = "https://api.coinbase.com/v2/prices/SOL-USD/spot";
        let line = json!({
            "loop": "executor", "t": 1,
            "answers": {
                "next_action": {"choice": "crypto_price", "confidence": 1.0},
                "crypto_price__pair": {"choice": "SOL-USD", "confidence": 0.97},
                "fx_rate__currency": {"choice": "EUR", "confidence": 0.9}
            },
            "result": {"action": "crypto_price", "outcome": "executed"},
            "args": {"method": "GET", "url": url},
            "ok": true,
            "output": {"price": {"amount": "118.29"}}
        });
        let text = render_audit(&line).unwrap();
        assert!(text.starts_with("jev executor #1 · crypto_price (1.00) → executed"));
        assert!(text.contains("pair = SOL-USD (0.97)"));
        assert!(!text.contains("EUR"), "other actions' slots are hidden");
        assert!(text.contains(url), "args are shown in full");
        assert!(text.contains("ok     {\"price\":{\"amount\":\"118.29\"}}"));
    }

    #[test]
    fn render_audit_terminal_and_rejected() {
        let done = json!({
            "loop": "executor", "t": 2,
            "answers": {"next_action": {"choice": "done", "confidence": 1.0}},
            "result": {"action": "done", "outcome": "stopped"},
            "args": null, "ok": null, "output": null
        });
        assert_eq!(
            render_audit(&done).unwrap(),
            "jev executor #2 · done (1.00) → stopped"
        );
        let rej = json!({
            "loop": "l", "t": 0,
            "answers": {"next_action": {"choice": "open", "confidence": 0.99}},
            "result": {"action": "open", "outcome": "rejected", "reason": "not a legal action this step"}
        });
        assert!(render_audit(&rej)
            .unwrap()
            .ends_with("rejected: not a legal action this step"));
        assert!(render_audit(&json!({"loop": "l"})).is_none());
    }

    // ── clock + replay ────────────────────────────────────────────────

    use crate::ports::clock::SimClock;

    /// 2026-09-21 — ten days before these tests were written: on the wall
    /// clock every row observed then is long stale.
    const T0: i64 = 1_790_000_000_000;

    /// `world` ages follow the loop's clock, not the wall: rows observed at
    /// `T0` are fresh at `T0 + 20 s` and stale at `T0 + 40 s`
    /// (`world_max_age_secs` 30; `open` requires `price` ≤ 30 s old).
    #[tokio::test]
    async fn world_freshness_follows_the_loop_clock() {
        let store = Arc::new(MemStore::default());
        store.put(&price_obs(T0, ObsStatus::Ok)).await.unwrap();
        store.put(&pools_obs(T0)).await.unwrap();
        let clock = Arc::new(SimClock::at(T0 + 20_000));
        let (l, engine, _) = build_typed(
            vec![
                pick(&[("next_action", "hold", 0.99)]),
                pick(&[("next_action", "hold", 0.99)]),
            ],
            ObsStatus::Ok,
            Some(store),
        );
        let l = l.with_clock(clock.clone());
        l.handle_event(&json!({}), "s").await.unwrap();
        clock.set(T0 + 40_000);
        l.handle_event(&json!({}), "s").await.unwrap();

        assert!(legal_actions(&engine, 0).contains(&"open".to_string()));
        assert!(!legal_actions(&engine, 1).contains(&"open".to_string()));
        let states = engine.states.lock().unwrap();
        assert_eq!(states[0]["world"]["price"]["age_s"], json!(20.0));
        assert_eq!(
            states[0]["world"]["price"]["features"]["usd"],
            json!(150.25)
        );
        assert_eq!(
            states[1]["world"]["price"],
            json!({"status": "stale", "age_s": 40.0})
        );
    }

    /// `ts` / `ts_ms` are the clock's; `trigger` only when the `AuditLog`
    /// sets one.
    #[tokio::test]
    async fn audit_lines_carry_the_clock_time_and_the_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.jsonl");
        let clock = Arc::new(SimClock::at(T0 + 20_500));
        let engine = Arc::new(Scripted::new(vec![
            pick(&[("next_action", "hold", 0.99)]),
            pick(&[("next_action", "hold", 0.99)]),
        ]));
        let replayed = DecisionLoop::new(
            "t",
            cfg(false),
            engine.clone(),
            Arc::new(FakeTools(StdMutex::new(vec![]))),
            None,
            None,
            Some(AuditLog {
                path: path.clone(),
                sandbox: Some("xlab".into()),
                trigger: Some("backtest".into()),
            }),
        )
        .with_clock(clock);
        replayed.handle_event(&json!({}), "s1").await.unwrap();
        let before = now_ms();
        audited("t", engine, &path)
            .handle_event(&json!({}), "s2")
            .await
            .unwrap();
        let after = now_ms();
        let lines: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["ts_ms"], json!(T0 + 20_500));
        assert_eq!(lines[0]["ts"], json!((T0 + 20_500) / 1000));
        assert_eq!(lines[0]["trigger"], json!("backtest"));
        assert_eq!(lines[0]["sandbox"], json!("xlab"));
        assert!(lines[0]["latency_ms"].is_u64(), "latency stays real");
        // No clock: the wall; no trigger: no key.
        let wall = lines[1]["ts_ms"].as_i64().unwrap();
        assert!((before..=after).contains(&wall), "{wall}");
        assert!(lines[1].get("trigger").is_none(), "{}", lines[1]);
    }

    fn gate_cfg() -> DecisionLoopConfig {
        let mut c: DecisionLoopConfig = toml::from_str(
            r#"
goal = "Take a weekend-fade candidate only when its move looks like noise"
agent = "xl_jev"
act_at = 0.7
[actions.take]
description = "Trade this candidate"
[actions.skip]
description = "Do not trade it"
[actions.ask_architect]
description = "Unsure: hand it to the architect"
"#,
        )
        .unwrap();
        c.escalate = false;
        c
    }

    fn gate_pick(choice: &str, confidence: f64, p: [(&str, f64); 3]) -> Decision {
        Decision {
            id: format!("gen-dec-{choice}"),
            answers: BTreeMap::from([(
                NEXT_ACTION.to_string(),
                Answer {
                    kind: "choice".into(),
                    choice: Some(choice.into()),
                    probabilities: p.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
                    confidence: Some(confidence),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        }
    }

    /// The gate arm's view of one candidate: a confident pick is `Stopped`
    /// with its confidence and p per action; one below `act_at` is
    /// `Escalated` (no history entry). One decision per event, every action
    /// offered, the event reaches the engine whole.
    #[tokio::test]
    async fn terminal_only_loop_returns_the_verdict() {
        let engine = Arc::new(Scripted::new(vec![
            gate_pick(
                "take",
                0.74,
                [("take", 0.83), ("skip", 0.15), ("ask_architect", 0.02)],
            ),
            gate_pick(
                "skip",
                0.55,
                [("take", 0.40), ("skip", 0.55), ("ask_architect", 0.05)],
            ),
        ]));
        let clock = Arc::new(SimClock::at(T0));
        let l = DecisionLoop::new(
            "xl_gate",
            gate_cfg(),
            engine.clone(),
            Arc::new(FakeTools(StdMutex::new(vec![]))),
            None,
            None,
            None,
        )
        .with_clock(clock.clone());
        let event = json!({
            "strategy": "weekend_fade", "instrument": TSLA, "side": "sell",
            "signal_bps": -182.5, "decided_at": "2026-09-27T22:00:00Z",
            "features": {"ret_24h_bps": -150.0, "hour_of_week": 166}
        });
        let take = l.decide_terminal(&event, "backtest:r1:0").await.unwrap();
        assert_eq!(
            take,
            Verdict {
                action: "take".into(),
                confidence: 0.74,
                probabilities: BTreeMap::from([
                    ("ask_architect".into(), 0.02),
                    ("skip".into(), 0.15),
                    ("take".into(), 0.83),
                ]),
                below_act_at: false,
                outcome: StepOutcome::Stopped {
                    action: "take".into()
                },
            }
        );
        clock.advance(3_600_000);
        let unsure = l.decide_terminal(&event, "backtest:r1:1").await.unwrap();
        assert!(unsure.below_act_at);
        assert_eq!(unsure.action, "skip");
        assert_eq!(unsure.probabilities["take"], 0.40);
        assert_eq!(
            unsure.outcome,
            StepOutcome::Escalated {
                action: "skip".into(),
                confidence: 0.55
            }
        );

        assert_eq!(legal_actions(&engine, 0), ["ask_architect", "skip", "take"]);
        // The confident pick is the next event's history; the escalated one is not.
        assert_eq!(l.history().await.len(), 1);
        let states = engine.states.lock().unwrap();
        assert_eq!(states.len(), 2, "one decision per event");
        assert_eq!(states[0]["event"], event);
        assert_eq!(states[1]["history"], json!([{"t": 1, "action": "take"}]));
    }

    #[tokio::test]
    async fn decide_terminal_refuses_a_loop_with_tools_before_any_call() {
        let (l, engine, tools, _) = build(false, vec![]);
        let err = l.decide_terminal(&json!({}), "s").await.unwrap_err();
        assert!(format!("{err}").contains("terminal-only"), "{err}");
        assert!(engine.states.lock().unwrap().is_empty());
        assert!(tools.0.lock().unwrap().is_empty());
    }

    /// A failed decisions call (an offline cache miss) is audited, then
    /// returned as the error.
    #[tokio::test]
    async fn decide_terminal_returns_the_engine_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("decisions.jsonl");
        let l = DecisionLoop::new(
            "xl_gate",
            gate_cfg(),
            Arc::new(Failing),
            Arc::new(FakeTools(StdMutex::new(vec![]))),
            None,
            None,
            Some(AuditLog {
                path: path.clone(),
                sandbox: None,
                trigger: Some("backtest".into()),
            }),
        )
        .with_clock(Arc::new(SimClock::at(T0)));
        let err = l.decide_terminal(&json!({}), "s").await.unwrap_err();
        assert!(format!("{err:#}").contains("402"), "{err:#}");
        let line: Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(line["result"]["outcome"], json!("error"));
        assert_eq!(line["ts_ms"], json!(T0));
    }
}
