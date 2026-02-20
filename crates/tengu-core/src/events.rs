//! Internal runtime domain events and event-bus abstraction.
//!
//! Potential use case:
//! Emit lifecycle events from orchestration code and route side-effects
//! (audit, metrics, policy reactions) through subscribers instead of direct calls.

use async_trait::async_trait;
use futures::{stream, Stream};
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::{broadcast, mpsc};

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
    /// Outcome status (for example: `ok`, `parse_error`, `exec_error`).
    pub status: String,
    /// Optional reason for non-ok outcomes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Payload for `ToolCallDenied`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallDenied {
    /// Engine-emitted tool call id.
    pub tool_call_id: String,
    /// Tool name requested by model.
    pub tool_name: String,
    /// Decision phase (`policy` or `protocol`).
    pub phase: String,
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

/// Snapshot of in-process bus runtime diagnostics counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventBusDiagnosticsSnapshot {
    /// Configured queue capacity for this bus.
    pub capacity: usize,
    /// Active overflow handling policy.
    pub policy: EventBusOverflowPolicy,
    /// Best-effort active subscriber count.
    pub active_subscribers: usize,
    /// Total publish attempts observed by this bus.
    pub published_total: u64,
    /// Events dropped on `DropNewest` policy due to full queues.
    pub dropped_newest_total: u64,
    /// Events skipped by lagging subscribers on broadcast queues.
    pub lagged_events_total: u64,
    /// Publish/send failures (for example closed/no receivers).
    pub send_errors_total: u64,
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

/// In-process bounded event bus used by runtime orchestration.
///
/// Strategy by overflow policy:
/// - `DropNewest`: per-subscriber bounded channels + non-blocking `try_send`
/// - `DropOldest`: shared bounded broadcast ring buffer (slow subscribers lag)
/// - `BlockProducer`: per-subscriber bounded channels + blocking `send().await`
pub struct InProcessEventBus {
    capacity: usize,
    policy: EventBusOverflowPolicy,
    backend: InProcessEventBusBackend,
    diagnostics: Arc<EventBusDiagnostics>,
}

enum InProcessEventBusBackend {
    /// Per-subscriber queue fanout used by `DropNewest` and `BlockProducer`.
    PerSubscriber {
        subscribers: RwLock<Vec<mpsc::Sender<DomainEvent>>>,
    },
    /// Shared broadcast queue used by `DropOldest`.
    Broadcast { tx: broadcast::Sender<DomainEvent> },
}

#[derive(Default)]
struct EventBusDiagnostics {
    published_total: AtomicU64,
    dropped_newest_total: AtomicU64,
    lagged_events_total: AtomicU64,
    send_errors_total: AtomicU64,
}

impl InProcessEventBus {
    /// Create a new in-process bounded bus.
    pub fn new(capacity: usize, policy: EventBusOverflowPolicy) -> Self {
        let capacity = capacity.max(1);
        let backend = match policy {
            EventBusOverflowPolicy::DropOldest => {
                let (tx, _) = broadcast::channel(capacity);
                InProcessEventBusBackend::Broadcast { tx }
            }
            EventBusOverflowPolicy::DropNewest | EventBusOverflowPolicy::BlockProducer => {
                InProcessEventBusBackend::PerSubscriber {
                    subscribers: RwLock::new(Vec::new()),
                }
            }
        };
        Self {
            capacity,
            policy,
            backend,
            diagnostics: Arc::new(EventBusDiagnostics::default()),
        }
    }

    /// Effective bounded queue capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Overflow policy used by this bus.
    pub fn policy(&self) -> EventBusOverflowPolicy {
        self.policy
    }

    /// Capture a point-in-time diagnostics snapshot for this bus.
    pub fn diagnostics_snapshot(&self) -> EventBusDiagnosticsSnapshot {
        let active_subscribers = match &self.backend {
            InProcessEventBusBackend::Broadcast { tx } => tx.receiver_count(),
            InProcessEventBusBackend::PerSubscriber { subscribers } => subscribers
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .filter(|sender| !sender.is_closed())
                .count(),
        };

        EventBusDiagnosticsSnapshot {
            capacity: self.capacity,
            policy: self.policy,
            active_subscribers,
            published_total: self.diagnostics.published_total.load(Ordering::Relaxed),
            dropped_newest_total: self
                .diagnostics
                .dropped_newest_total
                .load(Ordering::Relaxed),
            lagged_events_total: self.diagnostics.lagged_events_total.load(Ordering::Relaxed),
            send_errors_total: self.diagnostics.send_errors_total.load(Ordering::Relaxed),
        }
    }
}

impl Default for InProcessEventBus {
    fn default() -> Self {
        Self::new(256, EventBusOverflowPolicy::DropNewest)
    }
}

#[async_trait]
impl EventBus for InProcessEventBus {
    async fn publish(&self, event: DomainEvent) -> anyhow::Result<()> {
        self.diagnostics
            .published_total
            .fetch_add(1, Ordering::Relaxed);
        match &self.backend {
            InProcessEventBusBackend::Broadcast { tx } => {
                // Best-effort delivery: no active subscribers is not an error.
                if tx.send(event).is_err() {
                    self.diagnostics
                        .send_errors_total
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            InProcessEventBusBackend::PerSubscriber { subscribers } => {
                let senders: Vec<mpsc::Sender<DomainEvent>> = subscribers
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();

                for sender in &senders {
                    match self.policy {
                        EventBusOverflowPolicy::DropNewest => {
                            match sender.try_send(event.clone()) {
                                Ok(_) => {}
                                Err(mpsc::error::TrySendError::Full(_)) => {
                                    self.diagnostics
                                        .dropped_newest_total
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                    self.diagnostics
                                        .send_errors_total
                                        .fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        EventBusOverflowPolicy::BlockProducer => {
                            if sender.send(event.clone()).await.is_err() {
                                self.diagnostics
                                    .send_errors_total
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        EventBusOverflowPolicy::DropOldest => {
                            unreachable!("DropOldest always uses broadcast backend")
                        }
                    }
                }

                subscribers
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .retain(|sender| !sender.is_closed());
            }
        }
        Ok(())
    }

    fn subscribe(&self) -> DomainEventStream {
        match &self.backend {
            InProcessEventBusBackend::Broadcast { tx } => {
                let diagnostics = Arc::clone(&self.diagnostics);
                let rx = tx.subscribe();
                Box::pin(stream::unfold(rx, move |mut rx| {
                    let diagnostics = Arc::clone(&diagnostics);
                    async move {
                        loop {
                            match rx.recv().await {
                                Ok(event) => return Some((event, rx)),
                                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                    diagnostics
                                        .lagged_events_total
                                        .fetch_add(skipped as u64, Ordering::Relaxed);
                                    continue;
                                }
                                Err(broadcast::error::RecvError::Closed) => return None,
                            }
                        }
                    }
                }))
            }
            InProcessEventBusBackend::PerSubscriber { subscribers } => {
                let (tx, rx) = mpsc::channel(self.capacity);
                subscribers
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(tx);
                Box::pin(stream::unfold(rx, |mut rx| async move {
                    rx.recv().await.map(|event| (event, rx))
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::sync::Arc;
    use tokio::time::{timeout, Duration};

    fn test_event(seq: &str) -> DomainEvent {
        DomainEvent {
            meta: DomainEventMeta {
                ts_epoch_ms: 1,
                flow_key: Some(format!("flow-{}", seq)),
                agent_id: Some("main".to_string()),
                correlation_id: Some(format!("corr-{}", seq)),
                source: Some("test".to_string()),
            },
            payload: DomainEventPayload::FlowResolved(FlowResolved {
                flow_key: format!("flow-{}", seq),
                reused_existing: false,
            }),
        }
    }

    #[tokio::test]
    async fn drop_newest_keeps_oldest_when_queue_is_full() {
        let bus = InProcessEventBus::new(1, EventBusOverflowPolicy::DropNewest);
        let mut subscriber = bus.subscribe();

        bus.publish(test_event("a")).await.expect("publish a");
        bus.publish(test_event("b")).await.expect("publish b");

        let first = subscriber.next().await.expect("first event");
        let payload = match first.payload {
            DomainEventPayload::FlowResolved(payload) => payload,
            _ => panic!("unexpected payload"),
        };
        assert_eq!(payload.flow_key, "flow-a");

        let maybe_second = timeout(Duration::from_millis(30), subscriber.next()).await;
        assert!(
            maybe_second.is_err(),
            "queue should have dropped newest event"
        );
    }

    #[tokio::test]
    async fn block_producer_waits_for_capacity() {
        let bus = Arc::new(InProcessEventBus::new(
            1,
            EventBusOverflowPolicy::BlockProducer,
        ));
        let mut subscriber = bus.subscribe();

        bus.publish(test_event("a")).await.expect("publish a");
        let bus_publish = Arc::clone(&bus);
        let mut publish_task =
            tokio::spawn(async move { bus_publish.publish(test_event("b")).await });

        // Queue is full and subscriber has not consumed yet, so producer should block.
        let blocked = timeout(Duration::from_millis(40), &mut publish_task).await;
        assert!(blocked.is_err(), "producer publish unexpectedly completed");

        let _ = subscriber
            .next()
            .await
            .expect("consume first to free capacity");

        let join_result = timeout(Duration::from_millis(100), &mut publish_task)
            .await
            .expect("blocked producer should complete after capacity frees")
            .expect("publish task join");
        assert!(
            join_result.is_ok(),
            "publish should succeed after unblocking"
        );
    }

    #[tokio::test]
    async fn drop_oldest_yields_latest_after_lag() {
        let bus = InProcessEventBus::new(2, EventBusOverflowPolicy::DropOldest);
        let mut subscriber = bus.subscribe();

        bus.publish(test_event("1")).await.expect("publish 1");
        bus.publish(test_event("2")).await.expect("publish 2");
        bus.publish(test_event("3")).await.expect("publish 3");
        bus.publish(test_event("4")).await.expect("publish 4");

        let first_visible = subscriber.next().await.expect("first visible after lag");
        let payload = match first_visible.payload {
            DomainEventPayload::FlowResolved(payload) => payload,
            _ => panic!("unexpected payload"),
        };
        assert_eq!(
            payload.flow_key, "flow-3",
            "lagging receiver should observe most recent retained events"
        );
    }

    #[tokio::test]
    async fn diagnostics_track_drop_newest_and_publish_totals() {
        let bus = InProcessEventBus::new(1, EventBusOverflowPolicy::DropNewest);
        let _subscriber = bus.subscribe();

        bus.publish(test_event("a")).await.expect("publish a");
        bus.publish(test_event("b")).await.expect("publish b");

        let snapshot = bus.diagnostics_snapshot();
        assert_eq!(snapshot.published_total, 2);
        assert_eq!(snapshot.dropped_newest_total, 1);
        assert_eq!(snapshot.active_subscribers, 1);
    }

    #[tokio::test]
    async fn diagnostics_track_lagged_events_for_drop_oldest() {
        let bus = InProcessEventBus::new(2, EventBusOverflowPolicy::DropOldest);
        let mut subscriber = bus.subscribe();

        bus.publish(test_event("1")).await.expect("publish 1");
        bus.publish(test_event("2")).await.expect("publish 2");
        bus.publish(test_event("3")).await.expect("publish 3");
        bus.publish(test_event("4")).await.expect("publish 4");

        let _ = subscriber.next().await.expect("receive latest event");
        let snapshot = bus.diagnostics_snapshot();
        assert!(
            snapshot.lagged_events_total >= 1,
            "expected lagged counter to increase"
        );
    }

    #[tokio::test]
    async fn diagnostics_track_send_errors_without_subscribers() {
        let bus = InProcessEventBus::new(2, EventBusOverflowPolicy::DropOldest);
        bus.publish(test_event("solo"))
            .await
            .expect("publish without subscribers");

        let snapshot = bus.diagnostics_snapshot();
        assert_eq!(snapshot.send_errors_total, 1);
    }
}
