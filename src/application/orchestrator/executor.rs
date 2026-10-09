//! DAG executor: parallel step dispatch with retry escalation.

use crate::domain::plan::Step;
use crate::ports::orchestration::WorkerHandle;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use tracing::info;

use crate::application::orchestrator::events::{EventBus, OrchestratorEvent};
use crate::application::orchestrator::retry::{run_step_with_retry, RetryPolicy, StepOutcome};
use crate::domain::plan::{Plan, StepId};

pub enum ExecResult {
    Done { final_output: String },
    NeedsReplan { failed: StepId, error: String },
    Cancelled,
}

pub struct DagExecutor;

impl DagExecutor {
    pub async fn run(
        plan: &Plan,
        worker: Arc<dyn WorkerHandle>,
        policy: &RetryPolicy,
        events: &EventBus,
        cancel: Arc<AtomicBool>,
    ) -> ExecResult {
        let mut completed_outputs: HashMap<StepId, String> = HashMap::new();
        let mut in_flight: HashSet<StepId> = HashSet::new();
        let mut futures = FuturesUnordered::new();

        loop {
            // Honor cancellation before dispatching the next batch of ready
            // steps. Every early return aborts the steps still in flight
            // (`abort_in_flight`): a dropped `JoinHandle` only detaches its
            // task, which kept running (and its `run-agent` child with it).
            if cancel.load(Ordering::SeqCst) {
                abort_in_flight(&futures);
                return ExecResult::Cancelled;
            }

            // Dispatch ready steps.
            let completed: HashSet<StepId> = completed_outputs.keys().cloned().collect();
            for step in plan.ready_steps(&completed) {
                if in_flight.contains(&step.id) {
                    continue;
                }
                let step_inputs = render_step_inputs(step, &completed_outputs);
                let worker = Arc::clone(&worker);
                let policy = policy.clone();
                let events = events.clone();
                let step_clone = step.clone();
                in_flight.insert(step.id.clone());
                // A recording bus: the step's work (its `run-agent` child's
                // tool events) runs caused by its `step.started`.
                let cause = events.send_with_cause(OrchestratorEvent::StepStarted {
                    step_id: step.id.clone(),
                    agent: step.agent.clone(),
                });
                futures.push(tokio::spawn(async move {
                    let work =
                        run_step_with_retry(&step_clone, &step_inputs, worker, &policy, &events);
                    let outcome = match cause {
                        Some(c) => crate::application::trace_exec::caused_by(c, work).await,
                        None => work.await,
                    };
                    (step_clone.id, outcome)
                }));
            }

            if futures.is_empty() {
                // Nothing in flight and nothing ready — done.
                break;
            }

            // Await any completion.
            match futures.next().await {
                Some(Ok((id, StepOutcome::Ok(output)))) => {
                    in_flight.remove(&id);
                    let _ = events.send(OrchestratorEvent::StepSucceeded {
                        step_id: id.clone(),
                        output: output.clone(),
                    });
                    completed_outputs.insert(id, output);
                }
                Some(Ok((id, StepOutcome::Exhausted(err)))) => {
                    in_flight.remove(&id);
                    abort_in_flight(&futures);
                    return ExecResult::NeedsReplan {
                        failed: id,
                        error: err,
                    };
                }
                Some(Err(join_err)) => {
                    // Task panicked. Treat as catastrophic.
                    info!(?join_err, "executor task panicked");
                    abort_in_flight(&futures);
                    return ExecResult::NeedsReplan {
                        failed: StepId::new("<panicked>"),
                        error: format!("task panic: {}", join_err),
                    };
                }
                None => break,
            }

            // Re-check cancellation after each completion so a flag set
            // while steps were running is observed before the next dispatch.
            if cancel.load(Ordering::SeqCst) {
                abort_in_flight(&futures);
                return ExecResult::Cancelled;
            }
        }

        // Nothing in flight, nothing ready: a step that never ran depends on
        // an unknown step or sits in a cycle (plans are not validated up
        // front). Its section of the reply would be empty — replan instead.
        if let Some(stuck) = plan
            .steps
            .iter()
            .find(|s| !completed_outputs.contains_key(&s.id))
        {
            return ExecResult::NeedsReplan {
                failed: stuck.id.clone(),
                error: format!(
                    "step {} never became ready: its depends_on names an unknown step or forms a cycle",
                    stuck.id.0
                ),
            };
        }

        // All steps completed — the leaf's output is the final response;
        // parallel leaves (no join step) are joined in plan order, one section
        // per leaf (they used to run, then be thrown away by a replan).
        match plan.leaves().as_slice() {
            [] => ExecResult::NeedsReplan {
                failed: StepId::new("<no-leaf>"),
                error: "plan has no steps".into(),
            },
            [leaf] => ExecResult::Done {
                final_output: completed_outputs.get(&leaf.id).cloned().unwrap_or_default(),
            },
            leaves => ExecResult::Done {
                final_output: leaves
                    .iter()
                    .map(|s| {
                        let out = completed_outputs
                            .get(&s.id)
                            .map(String::as_str)
                            .unwrap_or_default();
                        format!("### {} ({})\n\n{}", s.id.0, s.agent, out.trim())
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            },
        }
    }
}

/// Abort every step task still running: dropping its `JoinHandle` would only
/// detach it. The task drops its `run-agent` child, which is `kill_on_drop`.
fn abort_in_flight<T>(futures: &FuturesUnordered<tokio::task::JoinHandle<T>>) {
    for handle in futures.iter() {
        handle.abort();
    }
}

fn render_step_inputs(step: &Step, completed: &HashMap<StepId, String>) -> String {
    if step.depends_on.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for dep in &step.depends_on {
        if let Some(body) = completed.get(dep) {
            out.push_str(&format!(
                "<step-input from=\"{}\">\n{}\n</step-input>\n\n",
                dep.0, body
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::orchestrator::events::new_bus;
    use crate::application::orchestrator::retry::RetryPolicy;
    use crate::domain::plan::{Plan, Step, StepId};
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    struct OkWorker;
    #[async_trait]
    impl WorkerHandle for OkWorker {
        async fn run_step(&self, step: &Step, inputs: &str) -> anyhow::Result<String> {
            Ok(format!("out({})[{}]", step.id.0, inputs.trim()))
        }
    }

    struct OrderRecordingWorker {
        order: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl WorkerHandle for OrderRecordingWorker {
        async fn run_step(&self, step: &Step, _inputs: &str) -> anyhow::Result<String> {
            self.order.lock().await.push(step.id.0.clone());
            Ok(format!("out-{}", step.id.0))
        }
    }

    fn linear_plan() -> Plan {
        Plan {
            steps: vec![
                Step {
                    id: StepId::new("s1"),
                    agent: "x".into(),
                    goal: "one".into(),
                    depends_on: vec![],
                    compose: None,
                },
                Step {
                    id: StepId::new("s2"),
                    agent: "x".into(),
                    goal: "two".into(),
                    depends_on: vec![StepId::new("s1")],
                    compose: None,
                },
            ],
        }
    }

    fn no_cancel() -> Arc<std::sync::atomic::AtomicBool> {
        Arc::new(std::sync::atomic::AtomicBool::new(false))
    }

    /// Each step id with the trace cause its work ran under.
    type Seen = Arc<Mutex<Vec<(String, Option<crate::application::trace_exec::Cause>)>>>;

    /// Records the trace cause each step's work runs under.
    struct CauseRecordingWorker {
        seen: Seen,
    }
    #[async_trait]
    impl WorkerHandle for CauseRecordingWorker {
        async fn run_step(&self, step: &Step, _inputs: &str) -> anyhow::Result<String> {
            let cause = crate::application::trace_exec::cause();
            self.seen.lock().await.push((step.id.0.clone(), cause));
            Ok(format!("out-{}", step.id.0))
        }
    }

    /// A recording bus: each step's work (what `SubprocessRunner` hands its
    /// `run-agent` child) runs caused by its own `step.started`, in the
    /// bus's session; an untraced bus sets no cause.
    #[tokio::test]
    async fn traced_bus_runs_each_step_under_its_step_started() {
        use crate::application::orchestrator::trace::{OrchestratorTrace, TraceBridge};
        use crate::application::trace_exec::tests::MemTrace;
        let sink = Arc::new(MemTrace::default());
        let bridge = TraceBridge::new(
            OrchestratorTrace {
                sink: sink.clone(),
                parent: None,
            },
            "chat-session".into(),
        );
        let bus = new_bus().traced(bridge);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let worker = Arc::new(CauseRecordingWorker { seen: seen.clone() });
        let mut policy = RetryPolicy::new(1);
        policy.backoff = vec![];
        let result = DagExecutor::run(&linear_plan(), worker, &policy, &bus, no_cancel()).await;
        assert!(matches!(result, ExecResult::Done { .. }));
        let d = sink.all();
        let started: Vec<String> = d
            .iter()
            .enumerate()
            .filter(|(_, e)| e.kind == "step.started")
            .map(|(i, _)| MemTrace::id(i))
            .collect();
        let seen = seen.lock().await.clone();
        let got: Vec<(&str, Option<&str>, Option<&str>)> = seen
            .iter()
            .map(|(s, c)| {
                (
                    s.as_str(),
                    c.as_ref().and_then(|c| c.parent.as_deref()),
                    c.as_ref().and_then(|c| c.session.as_deref()),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("s1", Some(started[0].as_str()), Some("chat-session")),
                ("s2", Some(started[1].as_str()), Some("chat-session")),
            ]
        );

        let untraced = Arc::new(Mutex::new(Vec::new()));
        let worker = Arc::new(CauseRecordingWorker {
            seen: untraced.clone(),
        });
        DagExecutor::run(&linear_plan(), worker, &policy, &new_bus(), no_cancel()).await;
        assert!(untraced.lock().await.iter().all(|(_, c)| c.is_none()));
    }

    #[tokio::test]
    async fn linear_plan_completes_in_order() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let worker = Arc::new(OrderRecordingWorker {
            order: order.clone(),
        });
        let bus = new_bus();
        let mut policy = RetryPolicy::new(1);
        policy.backoff = vec![];
        let result = DagExecutor::run(&linear_plan(), worker, &policy, &bus, no_cancel()).await;
        assert!(matches!(result, ExecResult::Done { .. }));
        assert_eq!(
            *order.lock().await,
            vec!["s1".to_string(), "s2".to_string()]
        );
    }

    #[tokio::test]
    async fn dependent_step_sees_upstream_output() {
        let bus = new_bus();
        let mut policy = RetryPolicy::new(1);
        policy.backoff = vec![];
        let result = DagExecutor::run(
            &linear_plan(),
            Arc::new(OkWorker),
            &policy,
            &bus,
            no_cancel(),
        )
        .await;
        match result {
            ExecResult::Done { final_output } => {
                assert!(final_output.contains("s1")); // s1's output fed into s2 via <step-input>
                assert!(final_output.contains("s2"));
            }
            _ => panic!("expected Done"),
        }
    }

    fn step(id: &str, deps: &[&str]) -> Step {
        Step {
            id: StepId::new(id),
            agent: format!("agent_{id}"),
            goal: id.into(),
            depends_on: deps.iter().map(|d| StepId::new(*d)).collect(),
            compose: None,
        }
    }

    /// Parallel steps with no join: both outputs make the reply, in plan
    /// order (they used to run, then be discarded by a replan).
    #[tokio::test]
    async fn parallel_leaves_are_joined_into_the_reply() {
        let bus = new_bus();
        let mut policy = RetryPolicy::new(1);
        policy.backoff = vec![];
        let plan = Plan {
            steps: vec![step("a", &[]), step("b", &[])],
        };
        match DagExecutor::run(&plan, Arc::new(OkWorker), &policy, &bus, no_cancel()).await {
            ExecResult::Done { final_output } => {
                let (a, b) = (
                    final_output.find("### a (agent_a)"),
                    final_output.find("### b (agent_b)"),
                );
                assert!(a.is_some() && b.is_some() && a < b, "{final_output}");
                assert!(final_output.contains("out(a)") && final_output.contains("out(b)"));
            }
            _ => panic!("expected Done"),
        }
    }

    /// A step that can never start (unknown dependency, or a cycle) makes
    /// the plan replan — never a reply with that step's section empty.
    #[tokio::test]
    async fn a_step_that_never_starts_needs_a_replan() {
        let bus = new_bus();
        let mut policy = RetryPolicy::new(1);
        policy.backoff = vec![];
        for steps in [
            vec![step("a", &[]), step("b", &["ghost"])],
            vec![step("a", &[]), step("b", &["a", "ghost"])],
            vec![step("a", &["b"]), step("b", &["a"])],
        ] {
            let plan = Plan { steps };
            match DagExecutor::run(&plan, Arc::new(OkWorker), &policy, &bus, no_cancel()).await {
                ExecResult::NeedsReplan { error, .. } => {
                    assert!(error.contains("never became ready"), "{error}")
                }
                _ => panic!("expected NeedsReplan"),
            }
        }
    }

    struct FailFastSlowWorker {
        slow_finished: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait]
    impl WorkerHandle for FailFastSlowWorker {
        async fn run_step(&self, step: &Step, _inputs: &str) -> anyhow::Result<String> {
            if step.id.0 == "fail" {
                anyhow::bail!("boom");
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            self.slow_finished
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok("late".into())
        }
    }

    /// A failure that triggers a replan aborts its siblings still in flight.
    #[tokio::test]
    async fn a_replan_aborts_steps_still_in_flight() {
        let bus = new_bus();
        let mut policy = RetryPolicy::new(1);
        policy.backoff = vec![];
        let slow_finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = Arc::new(FailFastSlowWorker {
            slow_finished: Arc::clone(&slow_finished),
        });
        let plan = Plan {
            steps: vec![step("fail", &[]), step("slow", &[])],
        };
        let result = DagExecutor::run(&plan, worker, &policy, &bus, no_cancel()).await;
        assert!(matches!(result, ExecResult::NeedsReplan { .. }));
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(
            !slow_finished.load(std::sync::atomic::Ordering::SeqCst),
            "the in-flight sibling kept running after the replan"
        );
    }

    #[tokio::test]
    async fn cancel_flag_short_circuits_before_dispatch() {
        let bus = new_bus();
        let mut policy = RetryPolicy::new(1);
        policy.backoff = vec![];
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let result =
            DagExecutor::run(&linear_plan(), Arc::new(OkWorker), &policy, &bus, cancel).await;
        assert!(matches!(result, ExecResult::Cancelled));
    }
}
