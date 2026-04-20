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
