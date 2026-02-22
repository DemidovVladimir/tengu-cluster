//! Runtime event-bus subscribers and delegated handoff execution helpers.
//!
//! Potential use case:
//! Keep `main.rs` focused on orchestration flow while moving event-driven side-effects
//! (audit persistence, metrics, policy reactions, delegated handoff queue/execution)
//! into one cohesive module.

use crate::control_plane::CapabilityAssignmentRecord;
use crate::control_plane_audit::{ControlPlaneAuditEvent, ControlPlaneAuditStore};
use crate::tool_audit::{ToolAuditEvent, ToolAuditStore};
use crate::{build_engine, now_epoch_ms};
use futures::StreamExt;
use std::collections::HashSet;
use std::sync::Arc;
use tengu_core::config::{Config, RuntimeProfile};
use tengu_core::events::{
    DomainEvent, DomainEventMeta, DomainEventPayload, EventBus, EventBusOverflowPolicy,
    HandoffResultReceived, InProcessEventBus,
};
use tengu_core::token::estimate_tokens_approx_min1;
use tengu_core::types::{
    HandoffResultEnvelope, HandoffResultStatus, HandoffTaskEnvelope, Message, Role,
    HANDOFF_SCHEMA_VERSION,
};
use tengu_core::EngineContext;
use tracing::{info, warn};

/// Runtime-tuned event bus settings derived from deployment profile.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EventBusRuntimeConfig {
    /// Queue capacity used by in-process bus.
    pub capacity: usize,
    /// Overflow policy for saturated queues.
    pub policy: EventBusOverflowPolicy,
    /// Event count interval for metrics snapshot logs.
    pub metrics_log_every: u64,
    /// Time interval for periodic diagnostics logs.
    pub diagnostics_interval_secs: u64,
}

/// Resolve event-bus settings from runtime profile for backpressure behavior.
///
/// Strategy:
/// - `Minimal`: `DropNewest` with small queue keeps single-core latency stable.
/// - `Desktop`/`Cloud`: `DropOldest` retains freshest events for lagging subscribers.
pub(crate) fn resolve_event_bus_runtime_config(profile: RuntimeProfile) -> EventBusRuntimeConfig {
    match profile {
        RuntimeProfile::Minimal => EventBusRuntimeConfig {
            capacity: 64,
            policy: EventBusOverflowPolicy::DropNewest,
            metrics_log_every: 100,
            diagnostics_interval_secs: 45,
        },
        RuntimeProfile::Desktop => EventBusRuntimeConfig {
            capacity: 256,
            policy: EventBusOverflowPolicy::DropOldest,
            metrics_log_every: 75,
            diagnostics_interval_secs: 30,
        },
        RuntimeProfile::Cloud => EventBusRuntimeConfig {
            capacity: 1024,
            policy: EventBusOverflowPolicy::DropOldest,
            metrics_log_every: 200,
            diagnostics_interval_secs: 15,
        },
    }
}

/// Spawn tool-audit subscriber that persists tool lifecycle domain events to JSONL.
pub(crate) fn spawn_tool_audit_subscriber(
    event_bus: Arc<InProcessEventBus>,
    store: Option<ToolAuditStore>,
) -> Option<tokio::task::JoinHandle<()>> {
    let store = store?;
    let mut stream = event_bus.subscribe();
    Some(tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            let Some(audit_event) = map_domain_event_to_tool_audit_event(&event) else {
                continue;
            };
            if let Err(err) = store.append(&audit_event) {
                warn!(
                    error = %err,
                    tool = %audit_event.tool_name,
                    phase = %audit_event.phase,
                    status = %audit_event.status,
                    "Failed to append tool audit event from subscriber"
                );
            }
        }
    }))
}

/// Spawn control-plane audit subscriber for delegated assignment lifecycle events.
pub(crate) fn spawn_control_plane_audit_subscriber(
    event_bus: Arc<InProcessEventBus>,
    store: Option<ControlPlaneAuditStore>,
) -> Option<tokio::task::JoinHandle<()>> {
    let store = store?;
    let mut stream = event_bus.subscribe();
    Some(tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            let Some(audit_event) = map_domain_event_to_control_plane_audit_event(&event) else {
                continue;
            };
            if let Err(err) = store.append(&audit_event) {
                warn!(
                    error = %err,
                    handoff_id = %audit_event.handoff_id,
                    status = %audit_event.status,
                    "Failed to append control-plane audit event from subscriber"
                );
            }
        }
    }))
}

/// Spawn delegated handoff acceptance subscriber for baseline execution loop.
///
/// Current behavior:
/// - listens for `HandoffTaskDispatched` events from orchestrator
/// - emits `HandoffResultReceived(status=accepted)` as queue acknowledgement
/// - dependent model execution is handled by separate execution subscriber
pub(crate) fn spawn_delegated_handoff_acceptance_subscriber(
    event_bus: Arc<InProcessEventBus>,
    orchestrator_agent_id: String,
) -> Option<tokio::task::JoinHandle<()>> {
    let mut stream = event_bus.subscribe();
    Some(tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            let (task, correlation_id) = match event.payload {
                DomainEventPayload::HandoffTaskDispatched(payload) => {
                    (payload.envelope, event.meta.correlation_id.clone())
                }
                _ => continue,
            };
            if task.from_agent_id != orchestrator_agent_id {
                continue;
            }

            let accepted =
                build_handoff_accepted_result(&task, "accepted by delegated handoff queue");
            if let Err(err) = accepted.validate_against(&task) {
                warn!(
                    error = %err,
                    handoff_id = %task.handoff_id,
                    "Invalid delegated handoff acceptance envelope"
                );
                continue;
            }

            let accepted_event = DomainEvent {
                meta: DomainEventMeta {
                    ts_epoch_ms: now_epoch_ms(),
                    flow_key: Some(task.flow_key.clone()),
                    agent_id: Some(task.to_agent_id.clone()),
                    correlation_id,
                    source: Some("delegated-handoff-queue".to_string()),
                },
                payload: DomainEventPayload::HandoffResultReceived(HandoffResultReceived {
                    envelope: accepted,
                }),
            };
            if let Err(err) = event_bus.publish(accepted_event).await {
                warn!(
                    error = %err,
                    handoff_id = %task.handoff_id,
                    "Failed to publish delegated handoff acceptance event"
                );
                continue;
            }
            info!(
                handoff_id = %task.handoff_id,
                dependent_agent_id = %task.to_agent_id,
                "Delegated handoff accepted by baseline queue"
            );
        }
    }))
}

/// Execute one delegated handoff task via dependent agent engine.
///
/// Baseline behavior:
/// - one model turn on dependent agent using envelope objective/context
/// - returns terminal `Completed` or `Failed` result envelope
/// - keeps output within envelope limits
pub(crate) async fn execute_delegated_handoff_task_once(
    config: &Config,
    envelope: &HandoffTaskEnvelope,
) -> HandoffResultEnvelope {
    if let Err(err) = envelope.validate() {
        return build_handoff_failed_result(
            envelope,
            &format!("invalid handoff task envelope: {}", err),
        );
    }

    let Some(dependent_cfg) = config.agents.get(envelope.to_agent_id.trim()) else {
        return build_handoff_failed_result(
            envelope,
            &format!(
                "dependent agent '{}' is not configured",
                envelope.to_agent_id.trim()
            ),
        );
    };

    let engine = match build_engine(envelope.to_agent_id.trim(), dependent_cfg) {
        Ok(engine) => engine,
        Err(err) => {
            return build_handoff_failed_result(
                envelope,
                &format!("dependent engine initialization failed: {}", err),
            );
        }
    };

    let dependent_prompt = build_delegated_handoff_prompt(envelope);
    let dependent_messages = vec![Message {
        role: Role::User,
        content: dependent_prompt,
        tool_call_id: None,
        tool_calls: None,
    }];
    let dependent_context = EngineContext {
        workspace: dependent_cfg.workspace.clone(),
        system_prompt: Some(format!(
            "You are agent '{}'. Execute delegated objective within provided constraints and return concise output.",
            envelope.to_agent_id
        )),
    };

    let mut stream = match engine
        .run(&dependent_messages, &[], &dependent_context)
        .await
    {
        Ok(stream) => stream,
        Err(err) => {
            return build_handoff_failed_result(
                envelope,
                &format!("dependent engine run failed: {}", err),
            );
        }
    };

    let mut response_text = String::new();
    let mut output_tokens = 0u32;
    while let Some(event) = stream.next().await {
        match event {
            tengu_core::types::StreamEvent::TextDelta { text } => response_text.push_str(&text),
            tengu_core::types::StreamEvent::Usage {
                output_tokens: out, ..
            } => output_tokens = output_tokens.max(out),
            tengu_core::types::StreamEvent::Error { message } => {
                return build_handoff_failed_result(
                    envelope,
                    &format!("dependent engine stream error: {}", message),
                );
            }
            _ => {}
        }
    }

    let summary = response_text.trim().to_string();
    if summary.is_empty() {
        return build_handoff_failed_result(
            envelope,
            "dependent engine produced empty terminal output",
        );
    }
    if output_tokens == 0 {
        output_tokens = estimate_tokens_approx_min1(&summary) as u32;
    }

    let completed = build_handoff_completed_result(envelope, summary, output_tokens);
    if let Err(err) = completed.validate_against(envelope) {
        return build_handoff_failed_result(
            envelope,
            &format!("dependent handoff result validation failed: {}", err),
        );
    }
    completed
}

/// Spawn delegated handoff execution subscriber for one-turn dependent execution.
///
/// Current scope:
/// - consumes `HandoffTaskDispatched` for configured orchestrator
/// - executes one dependent-agent model turn
/// - emits terminal `HandoffResultReceived` (`Completed`/`Failed`)
pub(crate) fn spawn_delegated_handoff_execution_subscriber(
    event_bus: Arc<InProcessEventBus>,
    config: Arc<Config>,
    orchestrator_agent_id: String,
) -> Option<tokio::task::JoinHandle<()>> {
    let mut stream = event_bus.subscribe();
    Some(tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            let (task, correlation_id) = match event.payload {
                DomainEventPayload::HandoffTaskDispatched(payload) => {
                    (payload.envelope, event.meta.correlation_id.clone())
                }
                _ => continue,
            };
            if task.from_agent_id != orchestrator_agent_id {
                continue;
            }

            let result = execute_delegated_handoff_task_once(config.as_ref(), &task).await;
            let status = result.status;
            let result_event = DomainEvent {
                meta: DomainEventMeta {
                    ts_epoch_ms: now_epoch_ms(),
                    flow_key: Some(task.flow_key.clone()),
                    agent_id: Some(task.to_agent_id.clone()),
                    correlation_id,
                    source: Some("delegated-handoff-exec".to_string()),
                },
                payload: DomainEventPayload::HandoffResultReceived(HandoffResultReceived {
                    envelope: result,
                }),
            };
            if let Err(err) = event_bus.publish(result_event).await {
                warn!(
                    error = %err,
                    handoff_id = %task.handoff_id,
                    "Failed to publish delegated handoff execution result"
                );
                continue;
            }
            info!(
                handoff_id = %task.handoff_id,
                dependent_agent_id = %task.to_agent_id,
                status = ?status,
                "Delegated handoff executed by dependent engine baseline"
            );
        }
    }))
}

#[derive(Debug, Default)]
struct EventMetricsState {
    total_events: u64,
    engine_failures: u64,
    tool_denials: u64,
    tool_completions: u64,
}

/// Spawn metrics subscriber that tracks high-level domain event counts.
pub(crate) fn spawn_event_metrics_subscriber(
    event_bus: Arc<InProcessEventBus>,
    log_every: u64,
) -> Option<tokio::task::JoinHandle<()>> {
    let log_every = log_every.max(1);
    let mut stream = event_bus.subscribe();
    Some(tokio::spawn(async move {
        let mut metrics = EventMetricsState::default();
        while let Some(event) = stream.next().await {
            metrics.total_events = metrics.total_events.saturating_add(1);
            match event.payload {
                DomainEventPayload::EngineTurnFailed(_) => {
                    metrics.engine_failures = metrics.engine_failures.saturating_add(1);
                }
                DomainEventPayload::ToolCallDenied(_) => {
                    metrics.tool_denials = metrics.tool_denials.saturating_add(1);
                }
                DomainEventPayload::ToolCallCompleted(_) => {
                    metrics.tool_completions = metrics.tool_completions.saturating_add(1);
                }
                _ => {}
            }

            if metrics.total_events % log_every == 0 {
                let snapshot = event_bus.diagnostics_snapshot();
                info!(
                    total_events = metrics.total_events,
                    engine_failures = metrics.engine_failures,
                    tool_denials = metrics.tool_denials,
                    tool_completions = metrics.tool_completions,
                    bus_published_total = snapshot.published_total,
                    bus_active_subscribers = snapshot.active_subscribers,
                    "Runtime event metrics snapshot"
                );
            }
        }
    }))
}

/// Spawn policy reaction subscriber that handles policy-denied tool events.
pub(crate) fn spawn_policy_reaction_subscriber(
    event_bus: Arc<InProcessEventBus>,
) -> Option<tokio::task::JoinHandle<()>> {
    let mut stream = event_bus.subscribe();
    Some(tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            if let DomainEventPayload::ToolCallDenied(payload) = &event.payload {
                if payload.phase != "policy" {
                    continue;
                }
                warn!(
                    agent_id = event.meta.agent_id.as_deref().unwrap_or("n/a"),
                    flow_key = event.meta.flow_key.as_deref().unwrap_or("n/a"),
                    tool = %payload.tool_name,
                    reason = %payload.reason,
                    "Policy subscriber observed tool denial"
                );
            }
        }
    }))
}

/// Spawn periodic diagnostics reporter for event bus lag/saturation counters.
pub(crate) fn spawn_event_bus_diagnostics_reporter(
    event_bus: Arc<InProcessEventBus>,
    interval_secs: u64,
) -> Option<tokio::task::JoinHandle<()>> {
    let interval_secs = interval_secs.max(1);
    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
            let snapshot = event_bus.diagnostics_snapshot();
            if snapshot.published_total == 0 {
                continue;
            }

            info!(
                capacity = snapshot.capacity,
                policy = ?snapshot.policy,
                active_subscribers = snapshot.active_subscribers,
                published_total = snapshot.published_total,
                dropped_newest_total = snapshot.dropped_newest_total,
                lagged_events_total = snapshot.lagged_events_total,
                send_errors_total = snapshot.send_errors_total,
                "Event bus diagnostics snapshot"
            );

            if snapshot.dropped_newest_total > 0
                || snapshot.lagged_events_total > 0
                || snapshot.send_errors_total > 0
            {
                warn!(
                    dropped_newest_total = snapshot.dropped_newest_total,
                    lagged_events_total = snapshot.lagged_events_total,
                    send_errors_total = snapshot.send_errors_total,
                    "Event bus saturation or lag indicators detected"
                );
            }
        }
    }))
}

/// Convert emitted domain events into persisted tool audit records.
pub(crate) fn map_domain_event_to_tool_audit_event(event: &DomainEvent) -> Option<ToolAuditEvent> {
    let flow_key = event.meta.flow_key.clone()?;
    let agent_id = event.meta.agent_id.clone()?;
    let ts_epoch_s = event.meta.ts_epoch_ms / 1000;

    match &event.payload {
        DomainEventPayload::ToolCallStarted(payload) => Some(ToolAuditEvent {
            ts_epoch_s,
            flow_key,
            agent_id,
            tool_call_id: payload.tool_call_id.clone(),
            tool_name: payload.tool_name.clone(),
            phase: "protocol".to_string(),
            status: "started".to_string(),
            reason: None,
            arguments_preview: None,
            result_preview: None,
        }),
        DomainEventPayload::ToolCallCompleted(payload) => Some(ToolAuditEvent {
            ts_epoch_s,
            flow_key,
            agent_id,
            tool_call_id: payload.tool_call_id.clone(),
            tool_name: payload.tool_name.clone(),
            phase: "execute".to_string(),
            status: payload.status.clone(),
            reason: payload.reason.clone(),
            arguments_preview: None,
            result_preview: None,
        }),
        DomainEventPayload::ToolCallDenied(payload) => Some(ToolAuditEvent {
            ts_epoch_s,
            flow_key,
            agent_id,
            tool_call_id: payload.tool_call_id.clone(),
            tool_name: payload.tool_name.clone(),
            phase: payload.phase.clone(),
            status: "denied".to_string(),
            reason: Some(payload.reason.clone()),
            arguments_preview: None,
            result_preview: None,
        }),
        _ => None,
    }
}

/// Convert emitted domain events into persisted delegated assignment audit records.
///
/// Note:
/// `Accepted` handoff results are treated as non-terminal queue acknowledgements
/// and are intentionally not persisted as audit rows to keep replay semantics
/// tied to approved/terminal lifecycle states.
pub(crate) fn map_domain_event_to_control_plane_audit_event(
    event: &DomainEvent,
) -> Option<ControlPlaneAuditEvent> {
    let ts_epoch_s = event.meta.ts_epoch_ms / 1000;

    match &event.payload {
        DomainEventPayload::HandoffTaskDispatched(payload) => Some(ControlPlaneAuditEvent {
            ts_epoch_s,
            flow_key: payload.envelope.flow_key.clone(),
            handoff_id: payload.envelope.handoff_id.clone(),
            orchestrator_agent_id: payload.envelope.from_agent_id.clone(),
            dependent_agent_id: payload.envelope.to_agent_id.clone(),
            status: "approved".to_string(),
            reason: None,
            requested_capabilities: payload.envelope.requested_capabilities.clone(),
            objective: Some(payload.envelope.objective.clone()),
        }),
        DomainEventPayload::HandoffResultReceived(payload) => {
            let status = match payload.envelope.status {
                HandoffResultStatus::Denied => {
                    let reason = payload.envelope.error_reason.as_deref().unwrap_or_default();
                    if reason.contains("revoked by orchestrator") {
                        "revoked".to_string()
                    } else if reason.contains("expired by retention policy") {
                        "expired".to_string()
                    } else {
                        "denied".to_string()
                    }
                }
                HandoffResultStatus::Failed => "failed".to_string(),
                HandoffResultStatus::Completed => "completed".to_string(),
                HandoffResultStatus::Accepted => return None,
            };
            Some(ControlPlaneAuditEvent {
                ts_epoch_s,
                flow_key: payload.envelope.flow_key.clone(),
                handoff_id: payload.envelope.handoff_id.clone(),
                orchestrator_agent_id: payload.envelope.to_agent_id.clone(),
                dependent_agent_id: payload.envelope.from_agent_id.clone(),
                status,
                reason: payload.envelope.error_reason.clone(),
                requested_capabilities: Vec::new(),
                objective: None,
            })
        }
        _ => None,
    }
}

/// Recover approved delegated assignments from persisted audit events.
///
/// Replay strategy:
/// - keep only assignments initiated by current orchestrator agent
/// - include only latest approved events not superseded by terminal statuses
/// - skip approvals that are already expired by TTL policy
/// - bound replay to recent `limit` rows for predictable startup latency
pub(crate) fn load_persisted_capability_assignments(
    store: Option<&ControlPlaneAuditStore>,
    orchestrator_agent_id: &str,
    limit: usize,
    ttl_secs: u64,
) -> Vec<CapabilityAssignmentRecord> {
    let Some(store) = store else {
        return Vec::new();
    };
    let Ok(events) = store.read_recent(limit) else {
        return Vec::new();
    };

    let mut closed_handoffs = HashSet::<String>::new();
    let mut recovered_rev = Vec::<CapabilityAssignmentRecord>::new();
    let now_ms = now_epoch_ms();
    let ttl_ms = ttl_secs.saturating_mul(1_000);

    for event in events.into_iter().rev() {
        let handoff_id = event.handoff_id.trim().to_string();
        if handoff_id.is_empty() {
            continue;
        }
        if event.status == "approved" {
            if closed_handoffs.contains(&handoff_id) {
                continue;
            }
            if event.orchestrator_agent_id.trim() != orchestrator_agent_id {
                continue;
            }
            let issued_at_epoch_ms = event.ts_epoch_s.saturating_mul(1_000);
            let expires_at_epoch_ms = issued_at_epoch_ms.saturating_add(ttl_ms);
            if now_ms >= expires_at_epoch_ms {
                closed_handoffs.insert(handoff_id);
                continue;
            }
            recovered_rev.push(CapabilityAssignmentRecord {
                handoff_id: handoff_id.clone(),
                flow_key: event.flow_key,
                orchestrator_agent_id: event.orchestrator_agent_id,
                dependent_agent_id: event.dependent_agent_id,
                requested_capabilities: event.requested_capabilities,
                objective: event.objective.unwrap_or_else(|| "n/a".to_string()),
                issued_at_epoch_ms,
            });
        } else {
            closed_handoffs.insert(handoff_id);
        }
    }

    recovered_rev.reverse();
    recovered_rev
}

/// Build non-terminal handoff acceptance envelope for delegated queue baseline.
pub(crate) fn build_handoff_accepted_result(
    envelope: &HandoffTaskEnvelope,
    summary: &str,
) -> HandoffResultEnvelope {
    HandoffResultEnvelope {
        schema_version: HANDOFF_SCHEMA_VERSION,
        handoff_id: envelope.handoff_id.clone(),
        flow_key: envelope.flow_key.clone(),
        from_agent_id: envelope.to_agent_id.clone(),
        to_agent_id: envelope.from_agent_id.clone(),
        status: HandoffResultStatus::Accepted,
        summary: summary.to_string(),
        artifacts: Vec::new(),
        output_tokens: 0,
        error_reason: None,
        metadata: std::collections::HashMap::from([(
            "runtime".to_string(),
            "delegated-handoff-queue".to_string(),
        )]),
    }
}

/// Build terminal failed handoff result envelope with reason text.
pub(crate) fn build_handoff_failed_result(
    envelope: &HandoffTaskEnvelope,
    reason: &str,
) -> HandoffResultEnvelope {
    HandoffResultEnvelope {
        schema_version: HANDOFF_SCHEMA_VERSION,
        handoff_id: envelope.handoff_id.clone(),
        flow_key: envelope.flow_key.clone(),
        from_agent_id: envelope.to_agent_id.clone(),
        to_agent_id: envelope.from_agent_id.clone(),
        status: HandoffResultStatus::Failed,
        summary: String::new(),
        artifacts: Vec::new(),
        output_tokens: 0,
        error_reason: Some(reason.to_string()),
        metadata: std::collections::HashMap::from([(
            "runtime".to_string(),
            "delegated-handoff-exec".to_string(),
        )]),
    }
}

/// Build terminal completed handoff result envelope with bounded output tokens.
pub(crate) fn build_handoff_completed_result(
    envelope: &HandoffTaskEnvelope,
    summary: String,
    output_tokens: u32,
) -> HandoffResultEnvelope {
    HandoffResultEnvelope {
        schema_version: HANDOFF_SCHEMA_VERSION,
        handoff_id: envelope.handoff_id.clone(),
        flow_key: envelope.flow_key.clone(),
        from_agent_id: envelope.to_agent_id.clone(),
        to_agent_id: envelope.from_agent_id.clone(),
        status: HandoffResultStatus::Completed,
        summary,
        artifacts: Vec::new(),
        output_tokens: output_tokens.min(envelope.max_output_tokens),
        error_reason: None,
        metadata: std::collections::HashMap::from([(
            "runtime".to_string(),
            "delegated-handoff-exec".to_string(),
        )]),
    }
}

/// Render delegated handoff task prompt for one dependent engine turn.
pub(crate) fn build_delegated_handoff_prompt(envelope: &HandoffTaskEnvelope) -> String {
    let requested = if envelope.requested_capabilities.is_empty() {
        "none".to_string()
    } else {
        envelope.requested_capabilities.join(", ")
    };
    let constraints = if envelope.constraints.is_empty() {
        "none".to_string()
    } else {
        envelope.constraints.join(", ")
    };
    let context = envelope.context_summary.as_deref().unwrap_or("n/a");
    format!(
        "Objective:\n{}\n\nRequested capabilities:\n{}\n\nConstraints:\n{}\n\nContext summary:\n{}\n\nReturn concise actionable result.",
        envelope.objective, requested, constraints, context
    )
}
