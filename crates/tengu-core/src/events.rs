//! Internal runtime domain events and event-bus abstraction.

use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;

/// Stable stream type returned by event-bus subscribers.
pub type DomainEventStream = Pin<Box<dyn Stream<Item = DomainEvent> + Send>>;

/// Generic metadata attached to every domain event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainEventMeta {
    pub ts_epoch_ms: u64,
    pub flow_key: Option<String>,
    pub agent_id: Option<String>,
    pub correlation_id: Option<String>,
    pub source: Option<String>,
}

/// One emitted runtime lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainEvent {
    pub meta: DomainEventMeta,
    pub payload: DomainEventPayload,
}

/// Domain event payloads used by runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DomainEventPayload {
    InboundTurnReceived(InboundTurnReceived),
    FlowResolved(FlowResolved),
    PromptAssembled(PromptAssembled),
    EngineTurnStarted(EngineTurnStarted),
    EngineTurnCompleted(EngineTurnCompleted),
    EngineTurnFailed(EngineTurnFailed),
    FlowCompacted(FlowCompacted),
    TaskAssigned(TaskAssigned),
    TaskCompleted(TaskCompleted),
    TaskFailed(TaskFailed),
    HeartbeatTick(HeartbeatTick),
    AgentStatusReport(AgentStatusReport),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundTurnReceived {
    pub pipe_id: String,
    pub sender_id: String,
    pub input_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowResolved {
    pub flow_key: String,
    pub reused_existing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptAssembled {
    pub system_tokens: u32,
    pub retrieval_tokens: u32,
    pub history_tokens: u32,
    pub reserved_output_tokens: u32,
    pub total_input_budget: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineTurnStarted {
    pub engine_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineTurnCompleted {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub response_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineTurnFailed {
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowCompacted {
    pub compacted_messages: u32,
    pub tokens_before: u64,
    pub tokens_after: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAssigned {
    pub task_id: String,
    pub agent_id: String,
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCompleted {
    pub task_id: String,
    pub agent_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFailed {
    pub task_id: String,
    pub agent_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatTick {
    pub seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStatusReport {
    pub agent_id: String,
    pub status: String,
    pub current_task_id: Option<String>,
}

/// Overflow handling policy for bounded bus implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventBusOverflowPolicy {
    DropNewest,
    DropOldest,
    BlockProducer,
}

#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish(&self, event: DomainEvent) -> anyhow::Result<()>;
    fn subscribe(&self) -> DomainEventStream;
}
