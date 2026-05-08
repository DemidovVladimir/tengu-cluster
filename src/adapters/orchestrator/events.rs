//! `OrchestratorEvent` + broadcast channel.

use tokio::sync::broadcast;

use crate::adapters::metrics::MetricsRecord;
use crate::adapters::orchestrator::plan::{Plan, StepId};

/// Phase 6.1 (full) — flat projection of a single RAG search hit attached
/// to `OrchestratorEvent::RagQueried`. We deliberately don't ship the full
/// `RagResult` (which carries the embedding vector + payload) on the event
/// bus — subscribers that want richer detail can hit the registry directly.
#[derive(Debug, Clone)]
pub struct RagQueriedHit {
    pub kind: String,
    pub name: String,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub enum OrchestratorEvent {
    PlanCreated {
        plan: Plan,
    },
    StepStarted {
        step_id: StepId,
        agent: String,
    },
    /// Reserved for future streaming-progress wiring (Phase 6.1 full event
    /// bus); the SubprocessRunner does not emit incremental chunks today.
    #[allow(dead_code)]
    StepProgress {
        step_id: StepId,
        chunk: String,
    },
    StepFailed {
        step_id: StepId,
        attempt: u32,
        error: String,
    },
    StepExhausted {
        step_id: StepId,
        final_error: String,
    },
    StepSucceeded {
        step_id: StepId,
        output: String,
    },
    ReplanTriggered {
        reason: String,
    },
    PlanCompleted {
        final_response: String,
        cancelled: bool,
    },
    /// Phase 6.1 (full) — emitted by `RagPlanner` on every `plan()` and
    /// `replan()` call, carrying the top-K registry hits the planner LLM
    /// saw. `phase` is `"plan"` or `"replan"`. Subscribers (TUI debug
    /// panel, future eval recorders) get the same data the existing
    /// `tracing::info!` line surfaces, but in structured form.
    RagQueried {
        phase: &'static str,
        query: String,
        hits: Vec<RagQueriedHit>,
    },
    /// Metrics — context/token consumption for one LLM or embedding call.
    /// Carried inline so subscribers (the TUI bottom bar, future eval
    /// recorders) get the same record as the global metrics bus and the
    /// `tracing::info!` baseline. Emitted by:
    /// - `RagPlanner::plan/replan` after the planner LLM returns,
    /// - `SubprocessRunner::run_step` once per IPC-returned subagent record,
    /// - `Embedder::embed_batch` after every API call (re-broadcast from the
    ///   global sink onto the orchestrator bus when an aggregator is wired).
    MetricsRecorded {
        record: MetricsRecord,
    },
}

pub type EventBus = broadcast::Sender<OrchestratorEvent>;
pub type EventReceiver = broadcast::Receiver<OrchestratorEvent>;

/// Default channel capacity. Channels subscribe cheaply; old events
/// are dropped if a subscriber lags (standard broadcast semantics).
pub const DEFAULT_BUS_CAPACITY: usize = 256;

pub fn new_bus() -> EventBus {
    broadcast::channel(DEFAULT_BUS_CAPACITY).0
}
