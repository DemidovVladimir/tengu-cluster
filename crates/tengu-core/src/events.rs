//! Internal runtime domain events and event-bus abstraction.

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

/// Snapshot of in-process bus runtime diagnostics counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventBusDiagnosticsSnapshot {
    pub capacity: usize,
    pub policy: EventBusOverflowPolicy,
    pub active_subscribers: usize,
    pub published_total: u64,
    pub dropped_newest_total: u64,
    pub lagged_events_total: u64,
    pub send_errors_total: u64,
}

#[async_trait]
pub trait EventBus: Send + Sync {
    async fn publish(&self, event: DomainEvent) -> anyhow::Result<()>;
    fn subscribe(&self) -> DomainEventStream;
}

pub struct InProcessEventBus {
    capacity: usize,
    policy: EventBusOverflowPolicy,
    backend: InProcessEventBusBackend,
    diagnostics: Arc<EventBusDiagnostics>,
}

enum InProcessEventBusBackend {
    PerSubscriber {
        subscribers: RwLock<Vec<mpsc::Sender<DomainEvent>>>,
    },
    Broadcast {
        tx: broadcast::Sender<DomainEvent>,
    },
}

#[derive(Default)]
struct EventBusDiagnostics {
    published_total: AtomicU64,
    dropped_newest_total: AtomicU64,
    lagged_events_total: AtomicU64,
    send_errors_total: AtomicU64,
}

impl InProcessEventBus {
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

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn policy(&self) -> EventBusOverflowPolicy {
        self.policy
    }

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
