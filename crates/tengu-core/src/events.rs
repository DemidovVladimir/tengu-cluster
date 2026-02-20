//! Internal runtime domain events and event-bus abstraction.
//!
//! Potential use case:
//! Emit lifecycle events from orchestration code and route side-effects
//! (audit, metrics, policy reactions) through subscribers instead of direct calls.

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

/// Stable stream type returned by event-bus subscribers.
pub type DomainEventStream = Pin<Box<dyn Stream<Item = DomainEvent> + Send>>;

/// Generic metadata attached to every domain event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainEventMeta {
    /// Event creation timestamp (unix epoch milliseconds).
    pub ts_epoch_ms: u64,
    /// Optional flow key tied to this runtime action.
    pub flow_key: Option<String>,
    /// Optional agent id tied to this runtime action.
    pub agent_id: Option<String>,
    /// Optional correlation id for stitching multi-step traces.
    pub correlation_id: Option<String>,
    /// Optional source component name (for example `chat-runtime`).
    pub source: Option<String>,
}

/// One emitted runtime lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainEvent {
    /// Shared metadata for all lifecycle events.
    pub meta: DomainEventMeta,
    /// Event-specific payload.
    pub payload: DomainEventPayload,
}

/// Domain event payloads used by runtime orchestration.
///
/// Version note:
/// This v1 schema mirrors `EVENT_BUS_MIGRATION_PLAN.md` and is intentionally
/// minimal so producers/subscribers can evolve without breaking contracts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DomainEventPayload {
    /// New inbound turn arrived from a channel adapter.
    InboundTurnReceived(InboundTurnReceived),
    /// Runtime resolved active flow key/session.
    FlowResolved(FlowResolved),
    /// Runtime assembled prompt buckets for one engine turn.
    PromptAssembled(PromptAssembled),
    /// Engine execution started for the current turn.
    EngineTurnStarted(EngineTurnStarted),
    /// Engine execution completed successfully.
    EngineTurnCompleted(EngineTurnCompleted),
    /// Engine execution failed.
    EngineTurnFailed(EngineTurnFailed),
    /// Tool call started from engine stream.
    ToolCallStarted(ToolCallStarted),
    /// Tool call finished (success or failure).
    ToolCallCompleted(ToolCallCompleted),
    /// Tool call denied by policy/approval/runtime checks.
    ToolCallDenied(ToolCallDenied),
    /// Flow compaction replaced older history with summary.
    FlowCompacted(FlowCompacted),
}

/// Payload for `InboundTurnReceived`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundTurnReceived {
    /// Inbound channel identifier.
    pub pipe_id: String,
    /// Sender identity normalized by runtime.
    pub sender_id: String,
    /// Approximate user-input token count.
    pub input_tokens: u32,
}

/// Payload for `FlowResolved`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowResolved {
    /// Runtime flow key chosen for this turn.
    pub flow_key: String,
    /// Whether this resolved flow was loaded from existing persisted state.
    pub reused_existing: bool,
}

/// Payload for `PromptAssembled`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptAssembled {
    /// Tokens contributed by system prompt.
    pub system_tokens: u32,
    /// Tokens contributed by retrieval context.
    pub retrieval_tokens: u32,
    /// Tokens contributed by history context.
    pub history_tokens: u32,
    /// Reserved output tokens.
    pub reserved_output_tokens: u32,
    /// Total input budget available for this turn.
    pub total_input_budget: u32,
}

/// Payload for `EngineTurnStarted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineTurnStarted {
    /// Engine provider id (for example `ollama`, `openai`).
    pub engine_id: String,
    /// Model identifier used for this turn.
    pub model_id: String,
}

/// Payload for `EngineTurnCompleted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineTurnCompleted {
    /// Input tokens reported for this turn.
    pub input_tokens: u32,
    /// Output tokens reported for this turn.
    pub output_tokens: u32,
    /// Approximate assistant response tokens persisted to flow.
    pub response_tokens: u32,
}

/// Payload for `EngineTurnFailed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineTurnFailed {
    /// Human-readable failure reason.
    pub reason: String,
}

/// Payload for `ToolCallStarted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallStarted {
    /// Engine-emitted tool call id.
    pub tool_call_id: String,
    /// Tool name requested by model.
    pub tool_name: String,
}

/// Payload for `ToolCallCompleted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallCompleted {
    /// Engine-emitted tool call id.
    pub tool_call_id: String,
    /// Tool name that was executed.
    pub tool_name: String,
    /// Outcome status (`ok` or `error`).
    pub status: String,
}

/// Payload for `ToolCallDenied`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallDenied {
    /// Engine-emitted tool call id.
    pub tool_call_id: String,
    /// Tool name requested by model.
    pub tool_name: String,
    /// Policy/approval/runtime reason for denial.
    pub reason: String,
}

/// Payload for `FlowCompacted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowCompacted {
    /// Number of messages replaced by compaction.
    pub compacted_messages: u32,
    /// Approximate token usage before compaction.
    pub tokens_before: u64,
    /// Approximate token usage after compaction.
    pub tokens_after: u64,
}

/// Overflow handling policy for bounded bus implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventBusOverflowPolicy {
    /// Reject new event when buffer is full.
    DropNewest,
    /// Remove oldest queued event to make room for new event.
    DropOldest,
    /// Block producer until capacity is available.
    BlockProducer,
}

/// Event-bus publisher/subscriber contract for internal runtime events.
///
/// Delivery semantics:
/// - Best-effort delivery to currently active subscribers.
/// - Ordered delivery per subscriber stream.
/// - Bounded behavior and concrete overflow strategy are implementation details.
#[async_trait]
pub trait EventBus: Send + Sync {
    /// Publish one event into the bus.
    async fn publish(&self, event: DomainEvent) -> anyhow::Result<()>;

    /// Subscribe to future published events.
    ///
    /// Implementations may choose whether each subscriber receives all events
    /// or a filtered subset; behavior must be documented by the implementation.
    fn subscribe(&self) -> DomainEventStream;
}
