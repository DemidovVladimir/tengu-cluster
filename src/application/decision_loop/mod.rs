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
//! 4. Terminal → stop. Write action under `dry_run` → log and stop.
//!    Otherwise render args, re-check caps, run the tool through the
//!    `ToolExecutor` port (`execute_typed`; same scopes / egress as an
//!    agent) and append to `history`: a typed result contributes its
//!    `decision_value` (or the reducer over `decision_root`), `ok = status
//!    != error` and `obs` meta; a text result goes through
//!    `reduce::parse_tool_output`. Continue.
//!
//! Every call emits a `MetricsKind::Decision` record and one JSONL audit line.
//! History is in-process (lost on restart); events for one loop are
//! serialised by the state mutex so history stays ordered.

pub(crate) mod reduce;
pub(crate) mod slots;
pub(crate) mod world;

use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::decision_loop::DecisionLoopConfig;
use crate::domain::decision::{Decision, HistoryEntry, Question, StepOutcome};
use crate::domain::message::ToolCall;
use crate::domain::metrics::{now_unix, MetricsKind, MetricsRecord};
use crate::domain::observation::now_ms;
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
    audit_path: Option<PathBuf>,
    state: Mutex<LoopState>,
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
        audit_path: Option<PathBuf>,
    ) -> Self {
        Self {
            name: name.into(),
            cfg,
            engine,
            tools,
            observations,
            escalator,
            audit_path,
            state: Mutex::new(LoopState::default()),
        }
    }

    /// Run the loop for one event. Returns what each step did.
    pub(crate) async fn handle_event(
        &self,
        event: &Value,
        session_id: &str,
    ) -> Result<Vec<StepOutcome>> {
        let mut st = self.state.lock().await;
        st.event_start = st.t;
        let event = reduce::reduce(event, &self.cfg.event_reduce);
        let mut outcomes = Vec::new();
        for step in 0..self.cfg.max_steps {
            let outcome = self.step(&mut st, &event, step, session_id).await?;
            let stop = !matches!(outcome, StepOutcome::Executed { .. });
            outcomes.push(outcome);
            if stop {
                break;
            }
        }
        Ok(outcomes)
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
    ) -> Result<StepOutcome> {
        // 1. World, legal actions + their slot candidates.
        let world = World::read(self.observations.as_deref(), &self.cfg, now_ms()).await;
        let mut legal: BTreeMap<&str, BTreeMap<&str, Vec<Candidate>>> = BTreeMap::new();
        for (an, action) in &self.cfg.actions {
            if !world.satisfies(&action.requires) {
                continue;
            }
            let mut slots = BTreeMap::new();
            for (sn, slot) in &action.slots {
                let this_event = st.history.iter().filter(|h| h.t > st.event_start);
                let c =
                    slots::candidates(sn, slot, action.caps.get(sn).copied(), this_event, &world);
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

        // 3. Decide.
        let started = Instant::now();
        let decision = self.engine.decide(&state, &questions).await?;
        self.record_metrics(
            session_id,
            &state,
            &decision,
            started.elapsed().as_millis() as u64,
        );

        let t_before = st.t;
        let outcome = self.apply(st, &legal, &decision, &state, session_id).await;
        // A step that ran (or dry-ran) appended one history entry with the resolved args.
        let entry = st.history.back().filter(|_| st.t > t_before);
        self.audit(session_id, st.t, &decision, &outcome, entry);
        info!(
            decision_loop = %self.name,
            session_id = %session_id,
            step,
            outcome = ?outcome,
            "decision loop step"
        );
        Ok(outcome)
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
            id: format!("{}-{t}", self.name),
            name: tool.clone(),
            arguments: args.clone(),
        };
        let now = now_ms();
        let (ok, result, obs) = match self.tools.execute_typed(&call, &[]).await {
            // Typed: features (or the reducer over `{.., features, data}`);
            // a status-`error` observation is a failure.
            Ok(out) => match out.observation {
                Some(o) => {
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
        StepOutcome::Executed {
            action: action_name,
        }
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
        let Some(escalator) = self.escalator.as_ref().filter(|_| self.cfg.escalate) else {
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
            ts_unix: now_unix(),
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

    /// One JSONL line per decision. Fail-soft: audit errors only warn.
    fn audit(
        &self,
        session_id: &str,
        t: u64,
        d: &Decision,
        outcome: &StepOutcome,
        entry: Option<&HistoryEntry>,
    ) {
        let Some(path) = &self.audit_path else { return };
        let line = json!({
            "ts": now_unix(),
            "loop": self.name,
            "session_id": session_id,
            "t": t,
            "decision_id": d.id,
            "model": d.model,
            "answers": d.answers,
            "usage": d.usage,
            "result": outcome,
            "args": entry.map(|e| &e.args),
            // What the tool returned, as `history` holds it (reduced, redacted).
            "ok": entry.and_then(|e| e.ok),
            "output": entry.map(|e| &e.result),
            // Typed result meta (key / status / source / age / slot).
            "obs": entry.and_then(|e| e.obs.as_ref()),
        });
        let res = (|| -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            writeln!(f, "{line}")
        })();
        if let Err(e) = res {
            warn!(path = %path.display(), error = %e, "decision audit write failed");
        }
    }
}

fn slot_key(action: &str, slot: &str) -> String {
    format!("{action}__{slot}")
}

fn rejected(action: &str, reason: &str) -> StepOutcome {
    StepOutcome::Rejected {
        action: action.to_string(),
        reason: reason.to_string(),
    }
}

/// One audit line (see `DecisionLoop::audit`) as a short readable block:
/// the chosen action + confidence, its slot answers, the args it ran with
/// and what came back. The TUI decision feed renders it. `None` when the
/// line has no `next_action` answer. Values are never shortened.
pub(crate) fn render_audit(v: &Value) -> Option<String> {
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
    async fn dry_run_blocks_write_actions() {
        let (l, _, tools, _) = build(
            true,
            vec![
                pick(&[("next_action", "fetch", 0.95)]),
                pick(&[
                    ("next_action", "open", 0.9),
                    ("open__pool", "pool_2", 0.9),
                    ("open__size", "1", 0.9),
                ]),
            ],
        );
        let out = l.handle_event(&json!({}), "s").await.unwrap();
        assert_eq!(
            out.last(),
            Some(&StepOutcome::DryRun {
                action: "open".into()
            })
        );
        assert_eq!(
            tools.0.lock().unwrap().len(),
            1,
            "only the read-only fetch ran"
        );
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
}
