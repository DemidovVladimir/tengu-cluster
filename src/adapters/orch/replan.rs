//! Replan outer loop: on step exhaustion, re-invoke the planner.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::adapters::orch::events::{EventBus, OrchestratorEvent};
use crate::adapters::orch::executor::{DagExecutor, ExecResult, WorkerHandle};
use crate::adapters::orch::plan::Plan;
use crate::adapters::orch::planner::{Planner, PlannerVerdict};
use crate::adapters::orch::retry::RetryPolicy;

pub async fn drive(
    planner: Arc<dyn Planner>,
    user_message: &str,
    worker: Arc<dyn WorkerHandle>,
    policy: &RetryPolicy,
    max_replans: u32,
    events: &EventBus,
    cancel: Arc<AtomicBool>,
) -> String {
    // Respect a cancellation requested before we even begin planning.
    if cancel.load(Ordering::SeqCst) {
        let _ = events.send(OrchestratorEvent::PlanCompleted {
            final_response: "Stopped by user.".into(),
            cancelled: true,
        });
        return "Stopped by user.".into();
    }

    let mut plan_opt: Option<Plan> = match planner.plan(user_message).await {
        Ok(PlannerVerdict::Direct { response }) => {
            let _ = events.send(OrchestratorEvent::PlanCompleted {
                final_response: response.clone(),
                cancelled: false,
            });
            return response;
        }
        Ok(PlannerVerdict::Plan { plan }) => Some(plan),
        Err(err) => {
            let msg = format!("System error: orchestrator initial call failed: {}", err);
            let _ = events.send(OrchestratorEvent::PlanCompleted {
                final_response: msg.clone(),
                cancelled: false,
            });
            return msg;
        }
    };

    let mut replans_left = max_replans;
    loop {
        let plan = plan_opt.as_ref().unwrap().clone();
        let _ = events.send(OrchestratorEvent::PlanCreated { plan: plan.clone() });

        match DagExecutor::run(&plan, Arc::clone(&worker), policy, events, Arc::clone(&cancel)).await {
            ExecResult::Done { final_output } => {
                let _ = events.send(OrchestratorEvent::PlanCompleted {
                    final_response: final_output.clone(),
                    cancelled: false,
                });
                return final_output;
            }
            ExecResult::Cancelled => {
                let _ = events.send(OrchestratorEvent::PlanCompleted {
                    final_response: "Stopped by user.".into(),
                    cancelled: true,
                });
                return "Stopped by user.".into();
            }
            ExecResult::NeedsReplan { failed, error } => {
                if replans_left == 0 {
                    let msg = format!(
                        "Unable to complete the request after {} replans. Last error: {}",
                        max_replans, error
                    );
                    let _ = events.send(OrchestratorEvent::PlanCompleted {
                        final_response: msg.clone(),
                        cancelled: false,
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
    use crate::adapters::orch::events::new_bus;
    use crate::adapters::orch::plan::{Step, StepId};
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
        async fn run_step(&self, step: &Step, _: &str) -> anyhow::Result<String> {
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

    #[tokio::test]
    async fn replans_after_step_exhaustion() {
        let bus = new_bus();
        let bad_plan = Plan {
            steps: vec![Step {
                id: StepId::new("bad"),
                agent: "x".into(),
                goal: "g".into(),
                depends_on: vec![],
            }],
        };
        let good_plan = Plan {
            steps: vec![Step {
                id: StepId::new("good"),
                agent: "x".into(),
                goal: "g".into(),
                depends_on: vec![],
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
        let cancel = Arc::new(AtomicBool::new(false));
        let out = drive(planner, "msg", worker, &policy, 2, &bus, cancel).await;
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
        let cancel = Arc::new(AtomicBool::new(false));
        let out = drive(planner, "msg", worker, &policy, 2, &bus, cancel).await;
        assert!(out.contains("Unable to complete"));
    }

    #[tokio::test]
    async fn pre_flagged_cancel_short_circuits_drive() {
        let bus = new_bus();
        let planner = Arc::new(ScriptedPlanner {
            verdicts: Mutex::new(vec![PlannerVerdict::Plan {
                plan: Plan {
                    steps: vec![Step {
                        id: StepId::new("x"),
                        agent: "x".into(),
                        goal: "g".into(),
                        depends_on: vec![],
                    }],
                },
            }]),
        });
        let worker = Arc::new(FailOnceWorker {
            calls: Mutex::new(0),
        });
        let mut policy = RetryPolicy::new(1);
        policy.backoff = vec![];
        let cancel = Arc::new(AtomicBool::new(true));
        let out = drive(planner, "msg", worker, &policy, 0, &bus, cancel).await;
        assert_eq!(out, "Stopped by user.");
    }
}
