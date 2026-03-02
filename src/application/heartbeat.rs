//! Periodic heartbeat loop for fleet orchestration.

use crate::application::ports::TaskStorePort;
use crate::application::task_orchestrator::TaskOrchestratorService;
use std::sync::atomic::{AtomicU64, Ordering};
use tengu_core::events::{
    DomainEvent, DomainEventMeta, DomainEventPayload, EventBus, HeartbeatTick,
};
use tracing::info;

/// Run the heartbeat loop. This function never returns (runs until cancelled).
///
/// Every `interval` seconds it:
/// 1. Publishes a `HeartbeatTick` event.
/// 2. Calls `orchestrator.heartbeat_check()` to detect stalled tasks.
pub(crate) async fn run_heartbeat_loop(
    interval: std::time::Duration,
    store: &dyn TaskStorePort,
    event_bus: &dyn EventBus,
    max_retries: u32,
) {
    let seq = AtomicU64::new(0);

    loop {
        tokio::time::sleep(interval).await;

        let tick = seq.fetch_add(1, Ordering::Relaxed);
        info!(seq = tick, "heartbeat tick");

        let _ = event_bus
            .publish(DomainEvent {
                meta: DomainEventMeta {
                    ts_epoch_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                    flow_key: None,
                    agent_id: None,
                    correlation_id: None,
                    source: Some("heartbeat".to_string()),
                },
                payload: DomainEventPayload::HeartbeatTick(HeartbeatTick { seq: tick }),
            })
            .await;

        let orchestrator = TaskOrchestratorService {
            store,
            event_bus,
            max_retries,
        };
        let _ = orchestrator.heartbeat_check();
    }
}
