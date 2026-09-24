//! Decision loop — a System One model (Jev) picks the next action and its
//! arguments; existing tools execute it. One `[decision_loops.<name>]` block
//! per loop (`config/decision_loop.rs`); design and probes in
//! `docs/decision-loop-plan-2026-09-24.md`.
//!
//! Per incoming event, up to `max_steps` times:
//!
//! 1. Legal actions = terminal actions + actions whose every slot has
//!    candidates (`slots::candidates`, caps applied).
//! 2. One decisions call: `next_action` (choice over legal actions) + one
//!    `choice` per multi-candidate slot (`<action>__<slot>`), against
//!    `state = {goal, event, history, step}`.
//! 3. Gate: min(confidence of action, its slots) < `act_at` → escalate
//!    (orchestrator turn via `Escalator`) and stop.
//! 4. Terminal → stop. Write action under `dry_run` → log and stop.
//!    Otherwise render args, re-check caps, run the tool through the
//!    `ToolExecutor` port (same scopes / egress as an agent), reduce the
//!    result into `history`, continue.
//!
//! Every call emits a `MetricsKind::Decision` record and one JSONL audit line.
//! History is in-process (lost on restart); events for one loop are
//! serialised by the state mutex so history stays ordered.

pub(crate) mod reduce;
pub(crate) mod slots;

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
use crate::ports::decision::{DecisionEngine, Escalator};
use crate::ports::engine::ToolExecutor;
use slots::Candidate;

const NEXT_ACTION: &str = "next_action";

pub(crate) struct DecisionLoop {
    name: String,
    cfg: DecisionLoopConfig,
    engine: Arc<dyn DecisionEngine>,
    tools: Arc<dyn ToolExecutor>,
    escalator: Option<Arc<dyn Escalator>>,
    audit_path: Option<PathBuf>,
    state: Mutex<LoopState>,
}

#[derive(Default)]
struct LoopState {
    t: u64,
    history: VecDeque<HistoryEntry>,
}

impl DecisionLoop {
    pub(crate) fn new(
        name: impl Into<String>,
        cfg: DecisionLoopConfig,
        engine: Arc<dyn DecisionEngine>,
        tools: Arc<dyn ToolExecutor>,
        escalator: Option<Arc<dyn Escalator>>,
        audit_path: Option<PathBuf>,
    ) -> Self {
        Self {
            name: name.into(),
            cfg,
            engine,
            tools,
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

    async fn step(
        &self,
        st: &mut LoopState,
        event: &Value,
        step: u32,
        session_id: &str,
    ) -> Result<StepOutcome> {
        // 1. Legal actions + their slot candidates.
        let mut legal: BTreeMap<&str, BTreeMap<&str, Vec<Candidate>>> = BTreeMap::new();
        for (an, action) in &self.cfg.actions {
            let mut slots = BTreeMap::new();
            for (sn, slot) in &action.slots {
                let c = slots::candidates(sn, slot, action.caps.get(sn).copied(), &st.history);
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
        let state = json!({
            "goal": self.cfg.goal,
            "event": event,
            "history": st.history,
            "step": step,
        });

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
        let (ok, result) = match self.tools.execute(&call, &[]).await {
            Ok(text) => {
                let (ok, raw) = reduce::parse_tool_output(&text);
                (ok, reduce::reduce(&raw, &action.reduce))
            }
            Err(e) => (false, json!(format!("{e:#}"))),
        };
        self.push(
            st,
            HistoryEntry {
                t,
                action: action_name.clone(),
                args,
                ok: Some(ok),
                result,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::decision::Answer;
    use crate::domain::message::Message;
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;

    /// Scripted decision engine: returns queued decisions, records questions.
    struct Scripted {
        queue: StdMutex<VecDeque<Decision>>,
        seen: StdMutex<Vec<BTreeMap<String, Question>>>,
    }

    #[async_trait]
    impl DecisionEngine for Scripted {
        fn model(&self) -> &str {
            "test"
        }
        async fn decide(&self, _s: &Value, q: &BTreeMap<String, Question>) -> Result<Decision> {
            self.seen.lock().unwrap().push(q.clone());
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
        let engine = Arc::new(Scripted {
            queue: StdMutex::new(script.into()),
            seen: StdMutex::new(vec![]),
        });
        let tools = Arc::new(FakeTools(StdMutex::new(vec![])));
        let esc = Arc::new(FakeEscalator(StdMutex::new(vec![])));
        let l = DecisionLoop::new(
            "t",
            cfg(dry_run),
            engine.clone(),
            tools.clone(),
            Some(esc.clone()),
            None,
        );
        (l, engine, tools, esc)
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
}
