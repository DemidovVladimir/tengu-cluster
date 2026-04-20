//! Harness-owned orchestration.
//!
//! - `config`    — OrchestratorConfig bridging (re-export from crate::adapters::config)
//! - `plan`      — Step, StepId, Plan types + topology helpers
//! - `planner`   — runs the orchestrator agent's LLM call
//! - `executor`  — DAG executor: parallel, ready-set scheduling
//! - `retry`     — per-step retry policy
//! - `replan`    — outer loop: re-invoke orchestrator on exhaustion
//! - `events`    — OrchestratorEvent enum + broadcast channel
//! - `roster`    — agent roster rendering + template substitution
//! - `telemetry` — event → tracing bridge
//!
//! NOTE: Temporarily named `orch` (not `orchestrator`) to avoid collision
//! with the legacy `src/adapters/orchestrator.rs` file. Will be renamed
//! to `orchestrator/` in Phase 7 when the legacy file is deleted.

pub mod config;
pub mod events;
pub mod executor;
pub mod plan;
pub mod planner;
pub mod replan;
pub mod retry;
pub mod roster;
pub mod telemetry;

// Public API re-exports
pub use events::{EventBus, EventReceiver, OrchestratorEvent};
pub use plan::{Plan, Step, StepId};

use std::sync::Arc;

use crate::adapters::memory::manager::MemoryManager;
use crate::adapters::orch::executor::WorkerHandle;
use crate::adapters::orch::planner::Planner;
use crate::adapters::orch::retry::RetryPolicy;

pub struct Orchestrator {
    planner: Arc<dyn Planner>,
    worker: Arc<dyn WorkerHandle>,
    policy: RetryPolicy,
    max_replans: u32,
    bus: EventBus,
    memory: Arc<MemoryManager>,
}

impl Orchestrator {
    pub fn new(
        planner: Arc<dyn Planner>,
        worker: Arc<dyn WorkerHandle>,
        policy: RetryPolicy,
        max_replans: u32,
        memory: Arc<MemoryManager>,
    ) -> Self {
        Self {
            planner,
            worker,
            policy,
            max_replans,
            bus: events::new_bus(),
            memory,
        }
    }

    pub fn subscribe(&self) -> EventReceiver {
        self.bus.subscribe()
    }

    pub async fn handle(&self, user_message: String) -> String {
        replan::drive(
            Arc::clone(&self.planner),
            &user_message,
            Arc::clone(&self.worker),
            &self.policy,
            self.max_replans,
            &self.bus,
        )
        .await
    }
}

#[cfg(test)]
mod e2e_tests {
    //! End-to-end orchestrator test with in-memory planner + worker.
    //!
    //! Kept inline (rather than in `tests/orchestrator_e2e.rs`) because
    //! the crate has no `[lib]` target — external integration tests
    //! cannot reach crate-private types.

    use std::sync::Arc;

    use async_trait::async_trait;

    use crate::adapters::orch::events::new_bus;
    use crate::adapters::orch::executor::WorkerHandle;
    use crate::adapters::orch::plan::{Plan, Step, StepId};
    use crate::adapters::orch::planner::{Planner, PlannerVerdict};
    use crate::adapters::orch::replan;
    use crate::adapters::orch::retry::RetryPolicy;

    struct StaticPlanner {
        plan: Plan,
    }
    #[async_trait]
    impl Planner for StaticPlanner {
        async fn plan(&self, _: &str) -> anyhow::Result<PlannerVerdict> {
            Ok(PlannerVerdict::Plan {
                plan: self.plan.clone(),
            })
        }
        async fn replan(
            &self,
            _: &str,
            _: &Plan,
            _: &str,
            _: &str,
        ) -> anyhow::Result<PlannerVerdict> {
            unreachable!()
        }
    }

    struct OkWorker;
    #[async_trait]
    impl WorkerHandle for OkWorker {
        async fn run_step(&self, step: &Step, inputs: &str) -> anyhow::Result<String> {
            Ok(format!("out({})[{}]", step.id.0, inputs.trim()))
        }
    }

    #[tokio::test]
    async fn fan_out_plus_synthesizer() {
        let plan = Plan {
            steps: vec![
                Step {
                    id: StepId::new("a"),
                    agent: "x".into(),
                    goal: "research".into(),
                    depends_on: vec![],
                },
                Step {
                    id: StepId::new("b"),
                    agent: "x".into(),
                    goal: "parallel-research".into(),
                    depends_on: vec![],
                },
                Step {
                    id: StepId::new("c"),
                    agent: "x".into(),
                    goal: "synthesize".into(),
                    depends_on: vec![StepId::new("a"), StepId::new("b")],
                },
            ],
        };
        let mut policy = RetryPolicy::new(1);
        policy.backoff = vec![];
        let bus = new_bus();
        let out = replan::drive(
            Arc::new(StaticPlanner { plan }),
            "user question",
            Arc::new(OkWorker),
            &policy,
            0,
            &bus,
        )
        .await;
        assert!(out.contains("out(c)")); // synthesizer output
        assert!(out.contains("out(a)")); // a's output embedded
        assert!(out.contains("out(b)")); // b's output embedded
    }
}
