//! DAG executor: parallel step dispatch with retry escalation.

use async_trait::async_trait;

use crate::adapters::orchestrator::plan::Step;

/// Abstracts "how to run a worker step." The real impl calls
/// `ChatRuntimeService::process_user_text` under the hood. Tests
/// inject fake impls.
#[async_trait]
pub trait WorkerHandle: Send + Sync {
    /// Run `step.agent` with `step.goal + step_inputs` as the user turn.
    /// `step_inputs` is the rendered `<step-input>` blocks from upstream
    /// completed steps.
    async fn run_step(&self, step: &Step, step_inputs: &str) -> anyhow::Result<String>;
}

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::stream::{FuturesUnordered, StreamExt};
use tracing::info;

use crate::adapters::orchestrator::events::{EventBus, OrchestratorEvent};
use crate::adapters::orchestrator::plan::{Plan, StepId};
use crate::adapters::orchestrator::retry::{run_step_with_retry, RetryPolicy, StepOutcome};

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
            // steps. In-flight steps are left to drain naturally; the loop
            // returns `Cancelled` as soon as we observe the flag at a
            // dispatch boundary.
            if cancel.load(Ordering::SeqCst) {
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
                let _ = events.send(OrchestratorEvent::StepStarted {
                    step_id: step.id.clone(),
                    agent: step.agent.clone(),
                });
                futures.push(tokio::spawn(async move {
                    let outcome =
                        run_step_with_retry(&step_clone, &step_inputs, worker, &policy, &events)
                            .await;
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
                    return ExecResult::NeedsReplan {
                        failed: id,
                        error: err,
                    };
                }
                Some(Err(join_err)) => {
                    // Task panicked. Treat as catastrophic.
                    info!(?join_err, "executor task panicked");
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
                return ExecResult::Cancelled;
            }
        }

        // All steps completed — leaf's output is the final response.
        if let Some(leaf) = plan.single_leaf() {
            let out = completed_outputs.get(&leaf.id).cloned().unwrap_or_default();
            ExecResult::Done { final_output: out }
        } else {
            // Validation should have caught this — but be defensive.
            ExecResult::NeedsReplan {
                failed: StepId::new("<no-leaf>"),
                error: "plan has no single leaf".into(),
            }
        }
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
    use crate::adapters::orchestrator::events::new_bus;
    use crate::adapters::orchestrator::plan::{Plan, Step, StepId};
    use crate::adapters::orchestrator::retry::RetryPolicy;
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
                },
                Step {
                    id: StepId::new("s2"),
                    agent: "x".into(),
                    goal: "two".into(),
                    depends_on: vec![StepId::new("s1")],
                },
            ],
        }
    }

    fn no_cancel() -> Arc<std::sync::atomic::AtomicBool> {
        Arc::new(std::sync::atomic::AtomicBool::new(false))
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
