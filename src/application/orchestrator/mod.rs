//! Harness-owned orchestration.
//!
//! - `plan`         — Step, StepId, Plan types + topology helpers
//! - `planner`      — `RegistryPlanner`: the planner agent's LLM call over the file-backed registry
//! - `executor`     — DAG executor: parallel, ready-set scheduling
//! - `retry`        — per-step retry policy
//! - `replan`       — outer loop: re-invoke the planner on exhaustion
//! - `events`       — OrchestratorEvent enum + the event bus (broadcast + optional trace bridge)
//! - `trace`        — the bridge: each OrchestratorEvent as an execution-trace event (Studio)
//! - `shared_files` — `TENGU_PLANNER_REGISTRY.md` (from `[agents.*]` blocks with a `description`) + per-session plan state for subagents

pub mod events;
pub mod executor;
pub mod planner;
pub mod replan;
pub mod retry;
pub mod shared_files;
pub(crate) mod trace;

// Public API re-exports
pub use events::{EventBus, EventReceiver, OrchestratorEvent};

use std::sync::Arc;

use crate::application::metrics::MetricsBarrier;
use crate::application::orchestrator::retry::RetryPolicy;
use crate::ports::orchestration::Planner;
use crate::ports::orchestration::WorkerHandle;

pub struct Orchestrator {
    planner: Arc<dyn Planner>,
    worker: Arc<dyn WorkerHandle>,
    policy: RetryPolicy,
    max_replans: u32,
    bus: EventBus,
    /// The metrics forwarder onto `bus` (`bootstrap::orchestrator`); it
    /// ends with this orchestrator. `None`: no forwarder (tests).
    metrics: Option<MetricsBarrier>,
}

impl Orchestrator {
    /// `bus` is an explicit constructor argument so the caller can wire
    /// the metrics forwarder and the trace bridge onto it before the
    /// orchestrator owns it. Callers with nothing to wire pass a fresh
    /// `events::new_bus()`.
    pub fn new(
        planner: Arc<dyn Planner>,
        worker: Arc<dyn WorkerHandle>,
        policy: RetryPolicy,
        max_replans: u32,
        bus: EventBus,
    ) -> Self {
        Self {
            planner,
            worker,
            policy,
            max_replans,
            bus,
            metrics: None,
        }
    }

    /// Own the metrics forwarder onto this orchestrator's bus
    /// (`application::metrics::forward_sink`).
    pub(crate) fn with_metrics(mut self, barrier: MetricsBarrier) -> Self {
        self.metrics = Some(barrier);
        self
    }

    /// Resolves once every metrics record put on the process sink before
    /// this call is on the bus — and so in the trace (`metrics.recorded`)
    /// and with its subscribers. Await it before a surface closes its trace
    /// (`run.closed`): a record still in the sink would land after it.
    pub(crate) async fn flush_metrics(&self) {
        if let Some(m) = &self.metrics {
            m.flush().await;
        }
    }

    pub fn subscribe(&self) -> EventReceiver {
        self.bus.subscribe()
    }

    /// One turn: plan, run, replan; every way out sends `PlanCompleted`.
    /// Cancelling a turn = dropping this future (Telegram `/stop`, a
    /// shutdown deadline, an eval row's timeout): the steps still running
    /// are aborted, their `run-agent` children killed, and it sends
    /// `PlanCompleted { cancelled: true }` as it drops, so the trace closes
    /// the turn (`plan.completed`, dropped) instead of leaving it running.
    pub async fn handle(&self, user_message: String) -> String {
        let mut unfinished = Unfinished(Some(&self.bus));
        let reply = replan::drive(
            Arc::clone(&self.planner),
            &user_message,
            Arc::clone(&self.worker),
            &self.policy,
            self.max_replans,
            &self.bus,
        )
        .await;
        unfinished.0 = None;
        reply
    }
}

/// [`Orchestrator::handle`] still running: dropped, it reports the turn
/// cancelled (`drive` sent no `PlanCompleted`).
struct Unfinished<'a>(Option<&'a EventBus>);

impl Drop for Unfinished<'_> {
    fn drop(&mut self) {
        if let Some(bus) = self.0 {
            let _ = bus.send(OrchestratorEvent::PlanCompleted {
                final_response: String::new(),
                cancelled: true,
                failed: false,
            });
        }
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

    use crate::application::orchestrator::events::new_bus;
    use crate::application::orchestrator::replan;
    use crate::application::orchestrator::retry::RetryPolicy;
    use crate::domain::plan::{Plan, Step, StepId};
    use crate::ports::orchestration::WorkerHandle;
    use crate::ports::orchestration::{Planner, PlannerVerdict};

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
        async fn run_step(&self, step: &Step, inputs: &str, _: &Plan) -> anyhow::Result<String> {
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
