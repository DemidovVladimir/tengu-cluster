//! Harness-owned orchestration.
//!
//! - `plan`         — Step, StepId, Plan types + topology helpers
//! - `planner`      — `RagPlanner`: the planner agent's LLM call over the file-backed registry
//! - `executor`     — DAG executor: parallel, ready-set scheduling
//! - `retry`        — per-step retry policy
//! - `replan`       — outer loop: re-invoke the planner on exhaustion
//! - `events`       — OrchestratorEvent enum + broadcast channel
//! - `shared_files` — `TENGU_PLANNER_REGISTRY.md` (from `[agents.*]` blocks with a `description`) + per-session plan state for subagents
//! - `wiring`       — channel-side glue

pub mod events;
pub mod executor;
pub mod plan;
pub mod planner;
pub mod replan;
pub mod retry;
pub mod shared_files;
// Phase 7.1 (partial) — `pub mod roster` removed; the only runtime caller
// (channel_runtime::build_orchestrator) now uses an inlined helper.
pub mod wiring;

// Public API re-exports
pub use events::{EventBus, EventReceiver, OrchestratorEvent};
// `Plan`, `Step`, `StepId` are no longer re-exported — the v2 RagPlanner /
// SubprocessRunner path consumes them via the internal `plan` module path.
// Keep the module `pub mod plan` above so external consumers can still reach
// them by full path if needed; Phase 7.1 will remove the static-mode plan
// machinery wholesale.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::adapters::memory::manager::MemoryManager;
use crate::adapters::orchestrator::executor::WorkerHandle;
use crate::adapters::orchestrator::planner::Planner;
use crate::adapters::orchestrator::retry::RetryPolicy;

pub struct Orchestrator {
    planner: Arc<dyn Planner>,
    worker: Arc<dyn WorkerHandle>,
    policy: RetryPolicy,
    max_replans: u32,
    bus: EventBus,
    /// Held to keep the manager alive for the lifetime of the orchestrator;
    /// Phase 6.4 (full) will read from this for cross-session message recall.
    #[allow(dead_code)]
    memory: Arc<MemoryManager>,
    cancel_flag: Arc<AtomicBool>,
}

impl Orchestrator {
    /// Phase 6.1 (full) — `bus` is now an explicit constructor argument
    /// rather than being minted internally, so the same bus can be wired
    /// into the `RagPlanner` (which emits `OrchestratorEvent::RagQueried`)
    /// before the orchestrator owns it. Callers without a planner that
    /// emits events can pass a fresh `events::new_bus()`.
    pub fn new(
        planner: Arc<dyn Planner>,
        worker: Arc<dyn WorkerHandle>,
        policy: RetryPolicy,
        max_replans: u32,
        memory: Arc<MemoryManager>,
        bus: EventBus,
    ) -> Self {
        Self {
            planner,
            worker,
            policy,
            max_replans,
            bus,
            memory,
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn subscribe(&self) -> EventReceiver {
        self.bus.subscribe()
    }

    /// Request cancellation of the in-flight plan. The DAG executor observes
    /// the flag at dispatch boundaries and returns `ExecResult::Cancelled`,
    /// which the replan driver translates into a terminal
    /// `PlanCompleted { cancelled: true }` event. Channel-side wiring
    /// (`/stop` for Telegram, Ctrl-C for CLI) is deferred until the
    /// full dispatch path routes through `Orchestrator::handle`.
    #[allow(dead_code)]
    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::SeqCst);
    }

    pub async fn handle(&self, user_message: String) -> String {
        // Reset the flag each turn so a stale cancellation from a previous
        // turn does not short-circuit this one.
        self.cancel_flag.store(false, Ordering::SeqCst);
        replan::drive(
            Arc::clone(&self.planner),
            &user_message,
            Arc::clone(&self.worker),
            &self.policy,
            self.max_replans,
            &self.bus,
            Arc::clone(&self.cancel_flag),
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

    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    use async_trait::async_trait;

    use crate::adapters::orchestrator::events::new_bus;
    use crate::adapters::orchestrator::executor::WorkerHandle;
    use crate::adapters::orchestrator::plan::{Plan, Step, StepId};
    use crate::adapters::orchestrator::planner::{Planner, PlannerVerdict};
    use crate::adapters::orchestrator::replan;
    use crate::adapters::orchestrator::retry::RetryPolicy;

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
                    compose: None,
                },
                Step {
                    id: StepId::new("b"),
                    agent: "x".into(),
                    goal: "parallel-research".into(),
                    depends_on: vec![],
                    compose: None,
                },
                Step {
                    id: StepId::new("c"),
                    agent: "x".into(),
                    goal: "synthesize".into(),
                    depends_on: vec![StepId::new("a"), StepId::new("b")],
                    compose: None,
                },
            ],
        };
        let mut policy = RetryPolicy::new(1);
        policy.backoff = vec![];
        let bus = new_bus();
        let cancel = Arc::new(AtomicBool::new(false));
        let out = replan::drive(
            Arc::new(StaticPlanner { plan }),
            "user question",
            Arc::new(OkWorker),
            &policy,
            0,
            &bus,
            cancel,
        )
        .await;
        assert!(out.contains("out(c)")); // synthesizer output
        assert!(out.contains("out(a)")); // a's output embedded
        assert!(out.contains("out(b)")); // b's output embedded
    }
}
