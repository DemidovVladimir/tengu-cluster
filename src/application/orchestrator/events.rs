//! `OrchestratorEvent` + the event bus: a broadcast channel (TUI, Telegram,
//! eval subscribe) and, on a surface that records, the execution-trace
//! bridge (`orchestrator/trace.rs`) — each event is written to the trace in
//! send order, synchronously, before it is broadcast.

use std::sync::{Arc, Weak};

use tokio::sync::broadcast;

use crate::application::orchestrator::trace::TraceBridge;
use crate::application::trace_exec::Cause;
use crate::domain::metrics::MetricsRecord;
use crate::domain::plan::{Plan, StepId};

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
    /// One attempt failed. `retry_in_ms` = the wait before the next attempt
    /// (`retry.rs`, the policy's backoff); `None` = this was the last one
    /// (`StepExhausted` follows).
    StepFailed {
        step_id: StepId,
        attempt: u32,
        error: String,
        retry_in_ms: Option<u64>,
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
    /// The turn's end. `failed` = `final_response` is a system error (the
    /// planner or replan call failed, or the replans ran out); `cancelled` =
    /// stopped by the user.
    PlanCompleted {
        final_response: String,
        cancelled: bool,
        failed: bool,
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

/// One orchestrator's events: broadcast to subscribers; recorded first when
/// the surface records ([`Self::traced`]).
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<OrchestratorEvent>,
    trace: Option<Arc<TraceBridge>>,
}

pub type EventReceiver = broadcast::Receiver<OrchestratorEvent>;

/// Default channel capacity. Channels subscribe cheaply; old events
/// are dropped if a subscriber lags (standard broadcast semantics).
pub const DEFAULT_BUS_CAPACITY: usize = 256;

pub fn new_bus() -> EventBus {
    EventBus {
        tx: broadcast::channel(DEFAULT_BUS_CAPACITY).0,
        trace: None,
    }
}

impl EventBus {
    /// This bus, recording every event through `bridge`.
    pub(crate) fn traced(mut self, bridge: TraceBridge) -> Self {
        self.trace = Some(Arc::new(bridge));
        self
    }

    /// Record `ev` (when traced), then broadcast it. `Err` only when no one
    /// subscribes (the event is still recorded) — the broadcast sender's own
    /// result, kept so every `events.send(…)` site reads as before.
    #[allow(clippy::result_large_err)]
    pub fn send(
        &self,
        ev: OrchestratorEvent,
    ) -> Result<usize, broadcast::error::SendError<OrchestratorEvent>> {
        if let Some(t) = &self.trace {
            t.record(&ev);
        }
        self.tx.send(ev)
    }

    /// [`Self::send`], returning the trace cause of the work `ev` starts (a
    /// step's task runs caused by its `step.started`); `None` untraced.
    pub(crate) fn send_with_cause(&self, ev: OrchestratorEvent) -> Option<Cause> {
        let cause = self.trace.as_ref().map(|t| t.cause(t.record(&ev)));
        let _ = self.tx.send(ev);
        cause
    }

    pub fn subscribe(&self) -> EventReceiver {
        self.tx.subscribe()
    }

    /// A handle that does not keep the bus alive (the metrics forwarder in
    /// `bootstrap::orchestrator` ends at its first record after the
    /// orchestrator is gone).
    pub(crate) fn downgrade(&self) -> WeakEventBus {
        WeakEventBus {
            tx: self.tx.downgrade(),
            trace: self.trace.as_ref().map(Arc::downgrade),
        }
    }
}

/// [`EventBus::downgrade`].
pub(crate) struct WeakEventBus {
    tx: broadcast::WeakSender<OrchestratorEvent>,
    trace: Option<Weak<TraceBridge>>,
}

impl WeakEventBus {
    /// The bus while its orchestrator lives.
    pub(crate) fn upgrade(&self) -> Option<EventBus> {
        let tx = self.tx.upgrade()?;
        let trace = match &self.trace {
            Some(w) => Some(w.upgrade()?),
            None => None,
        };
        Some(EventBus { tx, trace })
    }
}
