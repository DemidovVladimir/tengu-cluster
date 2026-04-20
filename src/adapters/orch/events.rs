//! `OrchestratorEvent` + broadcast channel.

use tokio::sync::broadcast;

use crate::adapters::orch::plan::{Plan, StepId};

#[derive(Debug, Clone)]
pub enum OrchestratorEvent {
    PlanCreated { plan: Plan },
    StepStarted { step_id: StepId, agent: String },
    StepProgress { step_id: StepId, chunk: String },
    StepFailed { step_id: StepId, attempt: u32, error: String },
    StepExhausted { step_id: StepId, final_error: String },
    StepSucceeded { step_id: StepId, output: String },
    ReplanTriggered { reason: String },
    PlanCompleted { final_response: String, cancelled: bool },
}

pub type EventBus = broadcast::Sender<OrchestratorEvent>;
pub type EventReceiver = broadcast::Receiver<OrchestratorEvent>;

/// Default channel capacity. Channels subscribe cheaply; old events
/// are dropped if a subscriber lags (standard broadcast semantics).
pub const DEFAULT_BUS_CAPACITY: usize = 256;

pub fn new_bus() -> EventBus {
    broadcast::channel(DEFAULT_BUS_CAPACITY).0
}
