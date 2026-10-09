//! Orchestration in the execution trace (`domain/trace.rs`,
//! `TENGU_STUDIO_PLAN.md` § 5): the bridge that writes one `ExecutionEvent`
//! per `OrchestratorEvent`, synchronously, when the event is sent
//! (`EventBus::send`) — in send order, before any subscriber sees it. A
//! surface that records hands `bootstrap::orchestrator::build_orchestrator`
//! an [`OrchestratorTrace`] (`tengu chat`, `tengu telegram`, `tengu eval`,
//! webhook agent endpoints and loop escalations under `tengu webhooks` /
//! `tengu run` / Studio Play); `None` = no bridge, nothing written.
//!
//! | `OrchestratorEvent` | `kind` (status) | `node_id` | Parent | Payload |
//! |---|---|---|---|---|
//! | `PlanCreated` | `plan.created` (ok) | `planner` | the turn's `plan.replanned`, else the surface's root (a webhook's `trigger.webhook`, an escalation's cause), else none | `steps`, `agents` (sorted, whole), `step_ids`, `depends_on`, `replan` |
//! | `StepStarted` | `step.started` (running) | `agent:<agent>` | its `plan.created` | `step`, `agent` |
//! | `StepFailed`, another attempt follows | `step.retrying` (pending) | `agent:<agent>` | its `step.started` | `step`, `attempt`, `error` (line 1), `error_bytes`, `retry_in_ms`; `duration_ms` = the attempt |
//! | `StepFailed`, the last attempt | `step.failed` (failed) | `agent:<agent>` | its `step.started` | `step`, `attempt`, `error` (line 1), `error_bytes`; `duration_ms` = the step |
//! | `StepSucceeded` | `step.completed` (ok) | `agent:<agent>` | its `step.started` | `step`, `output_chars`; `duration_ms` = the step |
//! | `ReplanTriggered` | `plan.replanned` (pending) | `planner` | the last `step.failed`, else the `plan.created` | `reason` (line 1), `plans` |
//! | `PlanCompleted` | `plan.completed` (ok · failed when `failed` · dropped when `cancelled`) | `planner` | the turn's last `plan.created`, else the root | `direct` (no plan: the planner answered itself), `cancelled`, `failed`, `response_chars`, `error` (line 1, failed only); `duration_ms` = the turn |
//! | `MetricsRecorded` of this session, a planner / subagent record | `metrics.recorded` (ok) | none | the step's `step.started` (subagent — also when the record reaches the bus after its turn's `plan.completed`), else the running turn's `plan.created`, else the root | `kind`, `agent`, `model`, `step`, token / char counts; `duration_ms` = `latency_ms` |
//! | `StepExhausted` · `StepProgress` · `RagQueried` | not written (the last `step.failed` says it · never sent · the query is the user's text) | | | |
//!
//! A turn = one `Orchestrator::handle` (its first event to its
//! `plan.completed`); session = the surface's (`resolve_session_id`,
//! `webhook-<endpoint>-<uuid>`, a loop's for an escalation). Never written:
//! a goal, the user's message, a step's output, the final response (sizes
//! only); an error or a reason keeps its first line, redacted by the sink.
//! `metrics.recorded` names no node: its record reaches the bus later
//! (through the process metrics sink), so it must not repaint a node.
//!
//! A step's work runs caused by its `step.started` ([`TraceBridge::cause`]
//! via `EventBus::send_with_cause` → `trace_exec::caused_by`): the
//! `SubprocessRunner` hands that id to its `run-agent` child
//! (`AgentIpcInput.trace`), whose `tool.*` events come back in
//! `AgentIpcOutput.trace` and are written under that step
//! (`trace_exec::replay_child`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::application::orchestrator::events::OrchestratorEvent;
use crate::application::trace_exec::Cause;
use crate::domain::metrics::{MetricsKind, MetricsRecord};
use crate::domain::trace::{line1, Component, EventDraft, Status};
use crate::domain::workflow::node_id;
use crate::ports::trace::TraceSink;

/// What a recording surface hands `build_orchestrator`.
#[derive(Clone)]
pub(crate) struct OrchestratorTrace {
    pub sink: Arc<dyn TraceSink>,
    /// The event every turn of this orchestrator descends from (a webhook
    /// request's `trigger.webhook`, an escalation's cause); `None` = each
    /// turn is a root (a chat / telegram message, an eval row).
    pub parent: Option<String>,
}

impl OrchestratorTrace {
    /// Turns under `parent` into `sink`; `None` when `sink` records nothing
    /// (`NoopTrace`: no bridge to run).
    pub(crate) fn of(sink: &Arc<dyn TraceSink>, parent: Option<String>) -> Option<Self> {
        sink.run_id()?;
        Some(Self {
            sink: Arc::clone(sink),
            parent,
        })
    }
}

/// One orchestrator's bridge (module table).
pub(crate) struct TraceBridge {
    sink: Arc<dyn TraceSink>,
    session: String,
    root: Option<String>,
    turn: Mutex<Option<Turn>>,
}

/// The current turn: what later events of it are parented to.
struct Turn {
    started: Instant,
    /// `plan.completed` written: the next event starts a new turn; until
    /// then a late `metrics.recorded` (the forwarder hands records to the
    /// bus after the step that made them) still finds its step here.
    ended: bool,
    /// `plan.created` written so far this turn.
    plans: u32,
    plan: Option<String>,
    replanned: Option<String>,
    last_failed: Option<String>,
    steps: HashMap<String, StepSeen>,
}

struct StepSeen {
    agent: String,
    event: Option<String>,
    started: Instant,
    attempt: Instant,
}

fn ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

impl TraceBridge {
    pub(crate) fn new(trace: OrchestratorTrace, session: String) -> Self {
        Self {
            sink: trace.sink,
            session,
            root: trace.parent,
            turn: Mutex::new(None),
        }
    }

    /// The cause of a step's work (its task, its `run-agent` child).
    pub(crate) fn cause(&self, step_event: Option<String>) -> Cause {
        Cause::new(step_event, self.session.clone())
    }

    fn draft(&self, kind: &str, status: Status) -> EventDraft {
        EventDraft::new(Component::Orchestrator, kind, status).session(self.session.clone())
    }

    fn emit(&self, d: EventDraft, parent: Option<String>) -> Option<String> {
        self.sink.emit(match parent {
            Some(p) => d.parent(p),
            None => d,
        })
    }

    /// Write `ev`'s event (module table); its `event_id`, `None` when it
    /// writes none.
    pub(crate) fn record(&self, ev: &OrchestratorEvent) -> Option<String> {
        if let OrchestratorEvent::MetricsRecorded { record } = ev {
            return self.metrics(record);
        }
        if matches!(
            ev,
            OrchestratorEvent::StepExhausted { .. }
                | OrchestratorEvent::StepProgress { .. }
                | OrchestratorEvent::RagQueried { .. }
        ) {
            return None;
        }
        let mut guard = self.turn.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        if guard.as_ref().is_some_and(|t| t.ended) {
            *guard = None;
        }
        let turn = guard.get_or_insert_with(|| Turn {
            started: now,
            ended: false,
            plans: 0,
            plan: None,
            replanned: None,
            last_failed: None,
            steps: HashMap::new(),
        });
        match ev {
            OrchestratorEvent::PlanCreated { plan } => {
                let mut agents: Vec<&str> = plan.steps.iter().map(|s| s.agent.as_str()).collect();
                agents.sort_unstable();
                agents.dedup();
                let ids: Vec<&str> = plan.steps.iter().map(|s| s.id.0.as_str()).collect();
                let depends: Map<String, Value> = plan
                    .steps
                    .iter()
                    .filter(|s| !s.depends_on.is_empty())
                    .map(|s| {
                        let on: Vec<&str> = s.depends_on.iter().map(|d| d.0.as_str()).collect();
                        (s.id.0.clone(), json!(on))
                    })
                    .collect();
                let d = self
                    .draft("plan.created", Status::Ok)
                    .node(node_id::planner())
                    .payload(json!({
                        "steps": plan.steps.len(),
                        "agents": agents,
                        "step_ids": ids,
                        "depends_on": depends,
                        "replan": turn.plans > 0,
                    }));
                let parent = turn.replanned.clone().or_else(|| self.root.clone());
                let id = self.emit(d, parent);
                turn.plans += 1;
                turn.plan.clone_from(&id);
                turn.last_failed = None;
                turn.steps.clear();
                id
            }
            OrchestratorEvent::StepStarted { step_id, agent } => {
                let d = self
                    .draft("step.started", Status::Running)
                    .node(node_id::agent(agent))
                    .payload(json!({"step": step_id.0, "agent": agent}));
                let id = self.emit(d, turn.plan.clone());
                turn.steps.insert(
                    step_id.0.clone(),
                    StepSeen {
                        agent: agent.clone(),
                        event: id.clone(),
                        started: now,
                        attempt: now,
                    },
                );
                id
            }
            OrchestratorEvent::StepFailed {
                step_id,
                attempt,
                error,
                retry_in_ms,
            } => {
                let step = turn.steps.get_mut(&step_id.0);
                let (kind, status) = match retry_in_ms {
                    Some(_) => ("step.retrying", Status::Pending),
                    None => ("step.failed", Status::Failed),
                };
                let mut payload = json!({
                    "step": step_id.0,
                    "attempt": attempt,
                    "error": line1(error),
                    "error_bytes": error.len(),
                });
                if let (Some(r), Value::Object(o)) = (retry_in_ms, &mut payload) {
                    o.insert("retry_in_ms".into(), json!(r));
                }
                let mut d = self.draft(kind, status).payload(payload);
                let mut parent = None;
                if let Some(s) = &step {
                    d = d
                        .node(node_id::agent(&s.agent))
                        .duration(ms(match retry_in_ms {
                            Some(_) => s.attempt,
                            None => s.started,
                        }));
                    parent.clone_from(&s.event);
                }
                let id = self.emit(d, parent.or_else(|| turn.plan.clone()));
                if let (Some(s), Some(r)) = (step, retry_in_ms) {
                    s.attempt = now + Duration::from_millis(*r);
                }
                if retry_in_ms.is_none() {
                    turn.last_failed.clone_from(&id);
                }
                id
            }
            OrchestratorEvent::StepSucceeded { step_id, output } => {
                let step = turn.steps.get(&step_id.0);
                let mut d = self.draft("step.completed", Status::Ok).payload(json!({
                    "step": step_id.0,
                    "output_chars": output.chars().count(),
                }));
                if let Some(s) = step {
                    d = d.node(node_id::agent(&s.agent)).duration(ms(s.started));
                }
                let parent = step.and_then(|s| s.event.clone());
                self.emit(d, parent.or_else(|| turn.plan.clone()))
            }
            OrchestratorEvent::ReplanTriggered { reason } => {
                let d = self
                    .draft("plan.replanned", Status::Pending)
                    .node(node_id::planner())
                    .payload(json!({"reason": line1(reason), "plans": turn.plans}));
                let parent = turn.last_failed.clone().or_else(|| turn.plan.clone());
                let id = self.emit(d, parent);
                turn.replanned.clone_from(&id);
                id
            }
            OrchestratorEvent::PlanCompleted {
                final_response,
                cancelled,
                failed,
            } => {
                let status = if *failed {
                    Status::Failed
                } else if *cancelled {
                    Status::Dropped
                } else {
                    Status::Ok
                };
                let mut payload = json!({
                    "direct": turn.plans == 0 && !failed && !cancelled,
                    "cancelled": cancelled,
                    "failed": failed,
                    "response_chars": final_response.chars().count(),
                });
                if let (true, Value::Object(o)) = (*failed, &mut payload) {
                    o.insert("error".into(), json!(line1(final_response)));
                }
                let d = self
                    .draft("plan.completed", status)
                    .node(node_id::planner())
                    .duration(ms(turn.started))
                    .payload(payload);
                let parent = turn.plan.clone().or_else(|| self.root.clone());
                let id = self.emit(d, parent);
                turn.ended = true;
                id
            }
            OrchestratorEvent::StepExhausted { .. }
            | OrchestratorEvent::StepProgress { .. }
            | OrchestratorEvent::RagQueried { .. }
            | OrchestratorEvent::MetricsRecorded { .. } => None,
        }
    }

    /// `metrics.recorded` (module table): this session's planner and
    /// subagent records only — the process metrics sink feeds every bus.
    fn metrics(&self, r: &MetricsRecord) -> Option<String> {
        if r.session_id != self.session
            || !matches!(r.kind, MetricsKind::Planner | MetricsKind::Subagent)
        {
            return None;
        }
        let parent = {
            let guard = self.turn.lock().unwrap_or_else(|p| p.into_inner());
            let turn = guard.as_ref();
            let step = match (r.kind, &r.step_id) {
                (MetricsKind::Subagent, Some(s)) => turn
                    .and_then(|t| t.steps.get(s))
                    .and_then(|s| s.event.clone()),
                _ => None,
            };
            // An ended turn lends only its steps: a planner record seen
            // now may be the next turn's, before its `plan.created`.
            step.or_else(|| turn.filter(|t| !t.ended).and_then(|t| t.plan.clone()))
                .or_else(|| self.root.clone())
        };
        let d = self
            .draft("metrics.recorded", Status::Ok)
            .duration(r.latency_ms)
            .payload(json!({
                "kind": r.kind.as_str(),
                "agent": r.agent,
                "model": r.model,
                "step": r.step_id,
                "prompt_tokens": r.prompt_tokens,
                "completion_tokens": r.completion_tokens,
                "total_tokens": r.total_tokens,
                "prompt_chars": r.prompt_chars,
                "response_chars": r.response_chars,
            }));
        self.emit(d, parent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::orchestrator::events::new_bus;
    use crate::application::trace_exec::tests::MemTrace;
    use crate::domain::plan::{Plan, Step, StepId};

    const SESSION: &str = "webhook-solana_events-0f0e0d0c-0b0a-4908-8706-050403020100";

    fn bridge(parent: Option<&str>) -> (Arc<MemTrace>, TraceBridge) {
        let sink = Arc::new(MemTrace::default());
        let t = OrchestratorTrace {
            sink: sink.clone(),
            parent: parent.map(str::to_string),
        };
        (sink, TraceBridge::new(t, SESSION.into()))
    }

    fn step(id: &str, agent: &str, deps: &[&str]) -> Step {
        Step {
            id: StepId::new(id),
            agent: agent.into(),
            goal: "a goal that never reaches the trace".into(),
            depends_on: deps.iter().map(|d| StepId::new(*d)).collect(),
            compose: None,
        }
    }

    fn metrics(kind: MetricsKind, session: &str, step: Option<&str>) -> MetricsRecord {
        MetricsRecord {
            ts_unix: 0,
            session_id: session.into(),
            kind,
            agent: "crypto_researcher".into(),
            model: "anthropic/claude-sonnet-4-6".into(),
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            prompt_chars: 400,
            prompt_bytes: 400,
            response_chars: 80,
            latency_ms: 950,
            layers: Vec::new(),
            step_id: step.map(str::to_string),
        }
    }

    /// Every event of a turn that retries, fails, replans and completes:
    /// kind, node, status, session, parent chain; no free text but line 1.
    #[test]
    fn each_event_maps_to_its_kind_node_status_and_parent() {
        let (sink, b) = bridge(Some("run:3"));
        let plan = Plan {
            steps: vec![
                step("a", "crypto_researcher", &[]),
                step("b", "writer", &["a"]),
            ],
        };
        let evs = [
            OrchestratorEvent::RagQueried {
                phase: "plan",
                query: "the user's words".into(),
                hits: vec![],
            },
            OrchestratorEvent::PlanCreated { plan: plan.clone() },
            OrchestratorEvent::StepStarted {
                step_id: StepId::new("a"),
                agent: "crypto_researcher".into(),
            },
            OrchestratorEvent::StepFailed {
                step_id: StepId::new("a"),
                attempt: 1,
                error: "subagent failed: HTTP 503\npartial output:\nsecret-ish text".into(),
                retry_in_ms: Some(1000),
            },
            OrchestratorEvent::StepSucceeded {
                step_id: StepId::new("a"),
                output: "four".into(),
            },
            OrchestratorEvent::StepStarted {
                step_id: StepId::new("b"),
                agent: "writer".into(),
            },
            OrchestratorEvent::StepFailed {
                step_id: StepId::new("b"),
                attempt: 3,
                error: "no agent 'writer'".into(),
                retry_in_ms: None,
            },
            OrchestratorEvent::StepExhausted {
                step_id: StepId::new("b"),
                final_error: "no agent 'writer'".into(),
            },
            OrchestratorEvent::ReplanTriggered {
                reason: "no agent 'writer'\nmore".into(),
            },
            OrchestratorEvent::PlanCreated { plan },
            OrchestratorEvent::PlanCompleted {
                final_response: "done".into(),
                cancelled: false,
                failed: false,
            },
        ];
        let ids: Vec<Option<String>> = evs.iter().map(|e| b.record(e)).collect();
        assert_eq!(ids[0], None, "a recall query is user text: not written");
        assert_eq!(ids[7], None, "StepExhausted = the last step.failed");
        let d = sink.all();
        let got: Vec<(&str, Option<&str>, Status)> = d
            .iter()
            .map(|d| (d.kind.as_str(), d.node_id.as_deref(), d.status))
            .collect();
        assert_eq!(
            got,
            [
                ("plan.created", Some("planner"), Status::Ok),
                (
                    "step.started",
                    Some("agent:crypto_researcher"),
                    Status::Running
                ),
                (
                    "step.retrying",
                    Some("agent:crypto_researcher"),
                    Status::Pending
                ),
                (
                    "step.completed",
                    Some("agent:crypto_researcher"),
                    Status::Ok
                ),
                ("step.started", Some("agent:writer"), Status::Running),
                ("step.failed", Some("agent:writer"), Status::Failed),
                ("plan.replanned", Some("planner"), Status::Pending),
                ("plan.created", Some("planner"), Status::Ok),
                ("plan.completed", Some("planner"), Status::Ok),
            ]
        );
        assert!(d.iter().all(|e| e.session_id.as_deref() == Some(SESSION)));
        assert!(d.iter().all(|e| e.component == Component::Orchestrator));
        let id = MemTrace::id;
        let parents: Vec<Option<&str>> = d.iter().map(|e| e.parent_event_id.as_deref()).collect();
        assert_eq!(
            parents,
            [
                Some("run:3"),        // the surface's root
                Some(id(0).as_str()), // step a ← plan
                Some(id(1).as_str()), // retry ← step a
                Some(id(1).as_str()), // completed ← step a
                Some(id(0).as_str()), // step b ← plan
                Some(id(4).as_str()), // failed ← step b
                Some(id(5).as_str()), // replan ← the failure
                Some(id(6).as_str()), // new plan ← replan
                Some(id(7).as_str()), // completed ← the new plan
            ]
        );
        assert_eq!(d[0].payload["steps"], json!(2));
        assert_eq!(
            d[0].payload["agents"],
            json!(["crypto_researcher", "writer"])
        );
        assert_eq!(d[0].payload["depends_on"], json!({"b": ["a"]}));
        assert_eq!(d[0].payload["replan"], json!(false));
        assert_eq!(d[7].payload["replan"], json!(true));
        assert_eq!(d[2].payload["error"], json!("subagent failed: HTTP 503"));
        assert_eq!(d[2].payload["retry_in_ms"], json!(1000));
        assert_eq!(d[5].payload["attempt"], json!(3));
        assert!(d[5].payload.get("retry_in_ms").is_none());
        assert_eq!(d[6].payload["reason"], json!("no agent 'writer'"));
        assert_eq!(d[8].payload["direct"], json!(false));
        assert!(d.iter().all(|e| e.duration_ms.is_some()
            == matches!(
                e.kind.as_str(),
                "step.retrying" | "step.failed" | "step.completed" | "plan.completed"
            )));
        let text =
            serde_json::to_string(&d.iter().map(|e| &e.payload).collect::<Vec<_>>()).unwrap();
        for hidden in [
            "a goal that never",
            "the user's words",
            "secret-ish",
            "four",
            "more",
        ] {
            assert!(!text.contains(hidden), "{hidden}: {text}");
        }
    }

    /// A direct answer is one `plan.completed` under the root; a failed
    /// planner call is red with its first line; a cancel is dropped; each
    /// turn starts fresh (a second turn's plan is not parented to the first).
    #[test]
    fn direct_failed_cancelled_turns() {
        let (sink, b) = bridge(None);
        let done = |failed, cancelled, text: &str| OrchestratorEvent::PlanCompleted {
            final_response: text.into(),
            cancelled,
            failed,
        };
        b.record(&done(false, false, "hello"));
        b.record(&done(
            true,
            false,
            "System error: orchestrator initial call failed: 401\nbody",
        ));
        b.record(&done(false, true, "Stopped by user."));
        b.record(&OrchestratorEvent::PlanCreated {
            plan: Plan {
                steps: vec![step("a", "x", &[])],
            },
        });
        let d = sink.all();
        assert_eq!(
            d.iter().map(|e| e.status).collect::<Vec<_>>(),
            [Status::Ok, Status::Failed, Status::Dropped, Status::Ok]
        );
        assert_eq!(d[0].payload["direct"], json!(true));
        assert_eq!(d[0].parent_event_id, None, "a chat turn is a root");
        assert_eq!(
            d[1].payload["error"],
            json!("System error: orchestrator initial call failed: 401")
        );
        assert!(d[0].payload.get("error").is_none());
        assert_eq!(d[3].parent_event_id, None);
        assert_eq!(d[3].payload["replan"], json!(false));
    }

    /// Only this session's planner / subagent records; no node (it must
    /// not repaint one); a subagent record hangs under its step.
    #[test]
    fn metrics_of_this_session_only() {
        let (sink, b) = bridge(Some("run:9"));
        b.record(&OrchestratorEvent::PlanCreated {
            plan: Plan {
                steps: vec![step("s1", "crypto_researcher", &[])],
            },
        });
        b.record(&OrchestratorEvent::StepStarted {
            step_id: StepId::new("s1"),
            agent: "crypto_researcher".into(),
        });
        for r in [
            metrics(MetricsKind::Subagent, SESSION, Some("s1")),
            metrics(MetricsKind::Planner, SESSION, None),
            metrics(MetricsKind::Subagent, "another-session", Some("s1")),
            metrics(MetricsKind::Embedding, SESSION, None),
        ] {
            b.record(&OrchestratorEvent::MetricsRecorded { record: r });
        }
        let d = sink.all();
        assert_eq!(
            sink.kinds(),
            [
                "plan.created",
                "step.started",
                "metrics.recorded",
                "metrics.recorded"
            ]
        );
        assert_eq!(d[2].node_id, None);
        assert_eq!(d[2].parent_event_id, Some(MemTrace::id(1)));
        assert_eq!(d[3].parent_event_id, Some(MemTrace::id(0)));
        assert_eq!(d[2].payload["total_tokens"], json!(120));
        assert_eq!(d[2].duration_ms, Some(950));
    }

    /// The metrics forwarder hands a record to the bus after the step that
    /// made it, so a step's record can land after its turn's
    /// `plan.completed`: it still hangs under that step. A planner record
    /// then (the next turn's, before its plan) is a root of a chat turn,
    /// and the next turn starts fresh.
    #[test]
    fn a_late_subagent_record_keeps_its_step() {
        let (sink, b) = bridge(None);
        b.record(&OrchestratorEvent::PlanCreated {
            plan: Plan {
                steps: vec![step("s1", "crypto_researcher", &[])],
            },
        });
        b.record(&OrchestratorEvent::StepStarted {
            step_id: StepId::new("s1"),
            agent: "crypto_researcher".into(),
        });
        b.record(&OrchestratorEvent::StepSucceeded {
            step_id: StepId::new("s1"),
            output: "x".into(),
        });
        b.record(&OrchestratorEvent::PlanCompleted {
            final_response: "x".into(),
            cancelled: false,
            failed: false,
        });
        for r in [
            metrics(MetricsKind::Subagent, SESSION, Some("s1")),
            metrics(MetricsKind::Planner, SESSION, None),
        ] {
            b.record(&OrchestratorEvent::MetricsRecorded { record: r });
        }
        b.record(&OrchestratorEvent::PlanCreated {
            plan: Plan {
                steps: vec![step("s1", "crypto_researcher", &[])],
            },
        });
        let d = sink.all();
        assert_eq!(d[4].kind, "metrics.recorded");
        assert_eq!(d[4].parent_event_id, Some(MemTrace::id(1)), "its step");
        assert_eq!(d[5].parent_event_id, None, "not the ended turn's plan");
        assert_eq!(d[6].kind, "plan.created");
        assert_eq!(d[6].parent_event_id, None);
        assert_eq!(d[6].payload["replan"], json!(false), "a new turn");
    }

    /// Through the bus: recorded in send order before subscribers see it;
    /// a step's cause is its `step.started` in the bus's session; an
    /// untraced bus has no cause and writes nothing.
    #[tokio::test]
    async fn bus_records_before_broadcast_and_hands_the_step_cause() {
        let (sink, b) = bridge(None);
        let bus = new_bus().traced(b);
        let mut rx = bus.subscribe();
        bus.send(OrchestratorEvent::PlanCreated {
            plan: Plan {
                steps: vec![step("a", "x", &[])],
            },
        })
        .unwrap();
        let cause = bus
            .send_with_cause(OrchestratorEvent::StepStarted {
                step_id: StepId::new("a"),
                agent: "x".into(),
            })
            .unwrap();
        assert_eq!(cause.parent, Some(MemTrace::id(1)));
        assert_eq!(cause.session.as_deref(), Some(SESSION));
        assert!(matches!(
            rx.recv().await.unwrap(),
            OrchestratorEvent::PlanCreated { .. }
        ));
        assert_eq!(sink.kinds(), ["plan.created", "step.started"]);
        let weak = bus.downgrade();
        assert!(weak.upgrade().is_some());
        drop(bus);
        assert!(weak.upgrade().is_none(), "the forwarder ends with the bus");
        assert!(new_bus()
            .send_with_cause(OrchestratorEvent::ReplanTriggered { reason: "r".into() })
            .is_none());
    }
}
