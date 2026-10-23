//! Replan outer loop: on step exhaustion, re-invoke the planner.

use std::sync::Arc;

use crate::application::orchestrator::events::{EventBus, OrchestratorEvent};
use crate::application::orchestrator::executor::{DagExecutor, ExecResult, Finished};
use crate::application::orchestrator::retry::RetryPolicy;
use crate::domain::plan::{Plan, StepId};
use crate::ports::orchestration::WorkerHandle;
use crate::ports::orchestration::{Planner, PlannerVerdict};

pub async fn drive(
    planner: Arc<dyn Planner>,
    user_message: &str,
    worker: Arc<dyn WorkerHandle>,
    policy: &RetryPolicy,
    max_replans: u32,
    events: &EventBus,
) -> String {
    let mut plan_opt: Option<Plan> = match planner.plan(user_message).await {
        Ok(PlannerVerdict::Direct { response }) => {
            let _ = events.send(OrchestratorEvent::PlanCompleted {
                final_response: response.clone(),
                cancelled: false,
                failed: false,
            });
            return response;
        }
        Ok(PlannerVerdict::Plan { plan }) => Some(plan),
        Err(err) => {
            let msg = format!("System error: orchestrator initial call failed: {}", err);
            let _ = events.send(OrchestratorEvent::PlanCompleted {
                final_response: msg.clone(),
                cancelled: false,
                failed: true,
            });
            return msg;
        }
    };

    let mut replans_left = max_replans;
    // Steps that finished in any attempt so far: an identical step in the
    // replanned DAG keeps its output (`DagExecutor::run_reusing`).
    let mut finished = Finished::new();
    loop {
        let plan = plan_opt.as_ref().unwrap().clone();
        let _ = events.send(OrchestratorEvent::PlanCreated { plan: plan.clone() });

        // A plan that cannot run (unknown agent, unknown dependency, cycle,
        // no steps) goes straight back to the planner: no step runs, no
        // retry waits.
        let result = match plan.validate(worker.known_agents().as_deref()) {
            Err(e) => ExecResult::NeedsReplan {
                failed: e.step().cloned().unwrap_or_else(|| StepId::new("<plan>")),
                error: format!("invalid plan: {e}"),
                finished: Finished::new(),
            },
            Ok(()) => {
                DagExecutor::run_reusing(&plan, Arc::clone(&worker), policy, events, &finished)
                    .await
            }
        };
        match result {
            ExecResult::Done { final_output } => {
                let _ = events.send(OrchestratorEvent::PlanCompleted {
                    final_response: final_output.clone(),
                    cancelled: false,
                    failed: false,
                });
                return final_output;
            }
            ExecResult::NeedsReplan {
                failed,
                error,
                finished: done,
            } => {
                finished.extend(done);
                if replans_left == 0 {
                    let msg = format!(
                        "Unable to complete the request after {} replans. Last error: {}",
                        max_replans, error
                    );
                    let _ = events.send(OrchestratorEvent::PlanCompleted {
                        final_response: msg.clone(),
                        cancelled: false,
                        failed: true,
                    });
                    return msg;
                }
                replans_left -= 1;
                let _ = events.send(OrchestratorEvent::ReplanTriggered {
                    reason: error.clone(),
                });
                match planner.replan(user_message, &plan, &failed.0, &error).await {
                    Ok(PlannerVerdict::Direct { response }) => {
                        let _ = events.send(OrchestratorEvent::PlanCompleted {
                            final_response: response.clone(),
                            cancelled: false,
                            failed: false,
                        });
                        return response;
                    }
                    Ok(PlannerVerdict::Plan { plan: new_plan }) => {
                        plan_opt = Some(new_plan);
                    }
                    Err(err) => {
                        let msg = format!("System error: replan call failed: {}", err);
                        let _ = events.send(OrchestratorEvent::PlanCompleted {
                            final_response: msg.clone(),
                            cancelled: false,
                            failed: true,
                        });
                        return msg;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::orchestrator::events::new_bus;
    use crate::domain::plan::Step;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::time::Duration;

    struct ScriptedPlanner {
        verdicts: Mutex<Vec<PlannerVerdict>>,
    }
    #[async_trait]
    impl Planner for ScriptedPlanner {
        async fn plan(&self, _: &str) -> anyhow::Result<PlannerVerdict> {
            Ok(self.verdicts.lock().unwrap().remove(0))
        }
        async fn replan(
            &self,
            _: &str,
            _: &Plan,
            _: &str,
            _: &str,
        ) -> anyhow::Result<PlannerVerdict> {
            Ok(self.verdicts.lock().unwrap().remove(0))
        }
    }

    struct FailOnceWorker {
        calls: Mutex<u32>,
    }
    #[async_trait]
    impl WorkerHandle for FailOnceWorker {
        async fn run_step(&self, step: &Step, _: &str, _: &Plan) -> anyhow::Result<String> {
            let mut c = self.calls.lock().unwrap();
            *c += 1;
            // Bad steps always fail; good steps succeed. The counter is
            // retained to preserve the `FailOnceWorker` shape from the plan,
            // but the gate `*c <= 3` in the original plan snippet was buggy
            // across replans (counter keeps climbing), which made
            // `bails_out_when_replans_exhausted` succeed on the 2nd bad plan.
            if step.id.0 == "bad" {
                anyhow::bail!("always fails")
            }
            Ok(format!("ok-{}", step.id.0))
        }
    }

    /// Knows one agent `x` and counts the steps it runs.
    struct KnownAgentsWorker {
        ran: Mutex<Vec<String>>,
    }
    #[async_trait]
    impl WorkerHandle for KnownAgentsWorker {
        async fn run_step(&self, step: &Step, _: &str, _: &Plan) -> anyhow::Result<String> {
            self.ran.lock().unwrap().push(step.id.0.clone());
            Ok(format!("ok-{}", step.id.0))
        }
        fn known_agents(&self) -> Option<Vec<String>> {
            Some(vec!["x".into()])
        }
    }

    /// A plan naming an unknown agent goes straight back to the planner:
    /// no step of it runs and no retry backoff is waited (10 s here).
    #[tokio::test]
    async fn invalid_plan_replans_before_any_step_runs() {
        let bus = new_bus();
        let mut rx = bus.subscribe();
        let plan = |agent: &str| Plan {
            steps: vec![
                Step {
                    id: StepId::new("first"),
                    agent: "x".into(),
                    goal: "g".into(),
                    depends_on: vec![],
                    compose: None,
                },
                Step {
                    id: StepId::new("second"),
                    agent: agent.into(),
                    goal: "g".into(),
                    depends_on: vec![StepId::new("first")],
                    compose: None,
                },
            ],
        };
        let planner = Arc::new(ScriptedPlanner {
            verdicts: Mutex::new(vec![
                PlannerVerdict::Plan {
                    plan: plan("web_search"),
                },
                PlannerVerdict::Plan { plan: plan("x") },
            ]),
        });
        let worker = Arc::new(KnownAgentsWorker {
            ran: Mutex::new(vec![]),
        });
        let mut policy = RetryPolicy::new(3);
        policy.backoff = vec![Duration::from_secs(10); 3];
        let out = tokio::time::timeout(
            Duration::from_secs(2),
            drive(planner, "msg", worker.clone(), &policy, 1, &bus),
        )
        .await
        .expect("no retry wait");
        assert!(out.contains("ok-second"), "{out}");
        assert_eq!(*worker.ran.lock().unwrap(), ["first", "second"]);
        let mut reason = None;
        while let Ok(ev) = rx.try_recv() {
            if let OrchestratorEvent::ReplanTriggered { reason: r } = ev {
                reason = Some(r);
            }
        }
        let reason = reason.expect("a replan");
        assert!(reason.starts_with("invalid plan: step"), "{reason}");
        assert!(reason.contains("\"web_search\""), "{reason}");
    }

    #[tokio::test]
    async fn replans_after_step_exhaustion() {
        let bus = new_bus();
        let bad_plan = Plan {
            steps: vec![Step {
                id: StepId::new("bad"),
                agent: "x".into(),
                goal: "g".into(),
                depends_on: vec![],
                compose: None,
            }],
        };
        let good_plan = Plan {
            steps: vec![Step {
                id: StepId::new("good"),
                agent: "x".into(),
                goal: "g".into(),
                depends_on: vec![],
                compose: None,
            }],
        };
        let planner = Arc::new(ScriptedPlanner {
            verdicts: Mutex::new(vec![
                PlannerVerdict::Plan { plan: bad_plan },
                PlannerVerdict::Plan { plan: good_plan },
            ]),
        });
        let worker = Arc::new(FailOnceWorker {
            calls: Mutex::new(0),
        });
        let mut policy = RetryPolicy::new(3);
        policy.backoff = vec![Duration::from_millis(1); 3];
        let out = drive(planner, "msg", worker, &policy, 2, &bus).await;
        assert!(out.contains("ok-good"));
    }

    #[tokio::test]
    async fn bails_out_when_replans_exhausted() {
        let bus = new_bus();
        let bad = || Plan {
            steps: vec![Step {
                id: StepId::new("bad"),
                agent: "x".into(),
                goal: "g".into(),
                depends_on: vec![],
                compose: None,
            }],
        };
        let planner = Arc::new(ScriptedPlanner {
            verdicts: Mutex::new(vec![
                PlannerVerdict::Plan { plan: bad() },
                PlannerVerdict::Plan { plan: bad() },
                PlannerVerdict::Plan { plan: bad() },
            ]),
        });
        let worker = Arc::new(FailOnceWorker {
            calls: Mutex::new(0),
        });
        let mut policy = RetryPolicy::new(3);
        policy.backoff = vec![Duration::from_millis(1); 3];
        let out = drive(planner, "msg", worker, &policy, 2, &bus).await;
        assert!(out.contains("Unable to complete"));
    }
}
