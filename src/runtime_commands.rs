//! Runtime slash-command handlers for chat/control-plane flows.
//!
//! Potential use case:
//! Keep `main.rs` focused on orchestration while command parsing/mutation logic
//! lives in one place that is easier to test and evolve.

use crate::{
    build_assignment_envelope, emit_domain_event, ensure_delegated_orchestrator, now_epoch_ms,
    parse_assign_command, parse_unassign_command, ChatLoopState, FlowCompactionPolicy,
};
use tengu_core::config::Config;
use tengu_core::events::{
    DomainEventPayload, EventBus, HandoffResultReceived, HandoffTaskDispatched,
};
use tengu_core::token::estimate_tokens_approx_min1;
use tengu_core::types::{HandoffResultEnvelope, HandoffResultStatus, HANDOFF_SCHEMA_VERSION};
use tengu_core::{Engine, Lens};

/// Process orchestrator control-plane commands and return `true` when handled.
///
/// Supported commands:
/// - `/assign <dependent> <cap1,cap2,...> [objective...]`
/// - `/assignments`
/// - `/unassign <handoff-id|dependent-agent-id>`
/// - `/assignments clear`
///
/// Assignment commands are available only when `capability_governance.mode=delegated`
/// and current chat agent is configured as delegated orchestrator.
pub(crate) async fn handle_control_plane_command(
    command: &str,
    config: &Config,
    agent_id: &str,
    flow_key: &str,
    state: &mut ChatLoopState,
    event_bus: &dyn EventBus,
    turn_correlation_id: &str,
) -> bool {
    if command.trim() == "/assignments clear" {
        if let Err(err) = ensure_delegated_orchestrator(config, agent_id) {
            println!("Delegated assignment cleanup denied: {}\n", err);
            return true;
        }
        if state.capability_assignments.is_empty() {
            println!("No delegated capability assignments to clear.\n");
            return true;
        }
        let removed = state.capability_assignments.drain(..).collect::<Vec<_>>();
        for assignment in &removed {
            emit_domain_event(
                event_bus,
                Some(agent_id),
                Some(&assignment.flow_key),
                Some(turn_correlation_id),
                DomainEventPayload::HandoffResultReceived(HandoffResultReceived {
                    envelope: build_assignment_revocation_result(
                        assignment,
                        "revoked by orchestrator via /assignments clear",
                    ),
                }),
            )
            .await;
        }
        println!(
            "Cleared {} delegated assignment(s) and emitted revocation events.\n",
            removed.len()
        );
        return true;
    }

    if command.trim() == "/assignments" {
        if state.capability_assignments.is_empty() {
            println!("No delegated capability assignments in this session.\n");
            return true;
        }
        println!("Delegated Capability Assignments");
        println!("─────────────────────────────");
        for (idx, assignment) in state.capability_assignments.iter().enumerate() {
            println!(
                " {}. {} -> {} [{}]",
                idx + 1,
                assignment.orchestrator_agent_id,
                assignment.dependent_agent_id,
                assignment.requested_capabilities.join(", ")
            );
            println!("    objective: {}", assignment.objective);
            println!("    handoff_id: {}", assignment.handoff_id);
        }
        println!();
        return true;
    }

    if command.trim_start().starts_with("/unassign") {
        if let Err(err) = ensure_delegated_orchestrator(config, agent_id) {
            println!("Delegated assignment cleanup denied: {}\n", err);
            return true;
        }

        let Some(target) = parse_unassign_command(command) else {
            println!("Usage: /unassign <handoff-id|dependent-agent-id>\n");
            return true;
        };

        let mut removed = Vec::new();
        state.capability_assignments.retain(|assignment| {
            let matches_target =
                assignment.handoff_id == target || assignment.dependent_agent_id == target;
            if matches_target {
                removed.push(assignment.clone());
                false
            } else {
                true
            }
        });

        if removed.is_empty() {
            println!(
                "No delegated assignment matched '{}' (handoff or dependent).\n",
                target
            );
            return true;
        }

        for assignment in &removed {
            emit_domain_event(
                event_bus,
                Some(agent_id),
                Some(&assignment.flow_key),
                Some(turn_correlation_id),
                DomainEventPayload::HandoffResultReceived(HandoffResultReceived {
                    envelope: build_assignment_revocation_result(
                        assignment,
                        "revoked by orchestrator via /unassign",
                    ),
                }),
            )
            .await;
        }
        println!(
            "Revoked {} delegated assignment(s) for '{}'.\n",
            removed.len(),
            target
        );
        return true;
    }

    if command.trim_start().starts_with("/assign") {
        let Some(parsed) = parse_assign_command(command) else {
            println!("Usage: /assign <dependent-agent-id> <cap1,cap2,...> [objective...]\n");
            return true;
        };

        let issued_at_ms = now_epoch_ms();
        match build_assignment_envelope(config, flow_key, agent_id, &parsed) {
            Ok(envelope) => {
                let record =
                    crate::CapabilityAssignmentRecord::from_envelope(&envelope, issued_at_ms);
                state.capability_assignments.push(record.clone());
                emit_domain_event(
                    event_bus,
                    Some(agent_id),
                    Some(flow_key),
                    Some(turn_correlation_id),
                    DomainEventPayload::HandoffTaskDispatched(HandoffTaskDispatched { envelope }),
                )
                .await;
                println!(
                    "Delegated assignment approved: {} -> {} [{}]\n",
                    record.orchestrator_agent_id,
                    record.dependent_agent_id,
                    record.requested_capabilities.join(", ")
                );
            }
            Err(err) => {
                let denied = HandoffResultEnvelope {
                    schema_version: HANDOFF_SCHEMA_VERSION,
                    handoff_id: uuid::Uuid::new_v4().to_string(),
                    flow_key: flow_key.to_string(),
                    from_agent_id: parsed.dependent_agent_id.clone(),
                    to_agent_id: agent_id.to_string(),
                    status: HandoffResultStatus::Denied,
                    summary: String::new(),
                    artifacts: Vec::new(),
                    output_tokens: 0,
                    error_reason: Some(err.to_string()),
                    metadata: std::collections::HashMap::new(),
                };
                emit_domain_event(
                    event_bus,
                    Some(agent_id),
                    Some(flow_key),
                    Some(turn_correlation_id),
                    DomainEventPayload::HandoffResultReceived(HandoffResultReceived {
                        envelope: denied,
                    }),
                )
                .await;
                println!("Delegated assignment denied: {}\n", err);
            }
        }
        return true;
    }

    false
}

/// Build terminal handoff-result envelope used for assignment cleanup/revocation.
fn build_assignment_revocation_result(
    assignment: &crate::CapabilityAssignmentRecord,
    reason: &str,
) -> HandoffResultEnvelope {
    HandoffResultEnvelope {
        schema_version: HANDOFF_SCHEMA_VERSION,
        handoff_id: assignment.handoff_id.clone(),
        flow_key: assignment.flow_key.clone(),
        from_agent_id: assignment.dependent_agent_id.clone(),
        to_agent_id: assignment.orchestrator_agent_id.clone(),
        status: HandoffResultStatus::Denied,
        summary: String::new(),
        artifacts: Vec::new(),
        output_tokens: 0,
        error_reason: Some(reason.to_string()),
        metadata: std::collections::HashMap::new(),
    }
}

/// Remove expired delegated assignments and emit terminal lifecycle events.
///
/// Expiry is based on assignment `issued_at_epoch_ms + ttl_secs`.
pub(crate) async fn prune_expired_capability_assignments(
    assignments: &mut Vec<crate::CapabilityAssignmentRecord>,
    ttl_secs: u64,
    event_bus: &dyn EventBus,
    actor_agent_id: &str,
    correlation_id: &str,
) -> usize {
    if assignments.is_empty() || ttl_secs == 0 {
        return 0;
    }
    let now_ms = now_epoch_ms();
    let ttl_ms = ttl_secs.saturating_mul(1_000);

    let mut expired = Vec::new();
    assignments.retain(|assignment| {
        let expires_at = assignment.issued_at_epoch_ms.saturating_add(ttl_ms);
        if now_ms >= expires_at {
            expired.push(assignment.clone());
            false
        } else {
            true
        }
    });

    for assignment in &expired {
        emit_domain_event(
            event_bus,
            Some(actor_agent_id),
            Some(&assignment.flow_key),
            Some(correlation_id),
            DomainEventPayload::HandoffResultReceived(HandoffResultReceived {
                envelope: build_assignment_revocation_result(
                    assignment,
                    "expired by retention policy",
                ),
            }),
        )
        .await;
    }

    expired.len()
}

/// Process one slash command and return `true` when loop should continue.
pub(crate) fn handle_chat_command(
    command: &str,
    state: &mut ChatLoopState,
    engine: &dyn Engine,
    agent_config: &tengu_core::config::AgentConfig,
    history_turn_limit: usize,
    compaction_policy: FlowCompactionPolicy,
) -> bool {
    match command {
        "/eco" => {
            state.active_lens = Lens::Eco;
            println!("Switched to eco lens (summaries only)\n");
            true
        }
        "/standard" => {
            state.active_lens = Lens::Standard;
            println!("Switched to standard lens (auto-expand)\n");
            true
        }
        "/precise" => {
            state.active_lens = Lens::Precise;
            println!("Switched to precise lens (full content)\n");
            true
        }
        "/cost" => {
            println!("Session Stats");
            println!("─────────────────────────────");
            println!(" Input tokens:  {}", state.total_input_tokens);
            println!(" Output tokens: {}", state.total_output_tokens);
            println!(
                " Total:         {}",
                state.total_input_tokens + state.total_output_tokens
            );
            if state.tokens_saved > 0 {
                println!();
                println!(" Saved by refiner:");
                println!("   Prompt compression: -{} tokens", state.tokens_saved);
            }
            println!();
            true
        }
        "/context" => {
            let used: usize = state
                .messages
                .iter()
                .map(|m| estimate_tokens_approx_min1(&m.content))
                .sum();
            let window = engine.context_window();
            let lens_name = state.active_lens.as_str();
            println!(
                "Context: ~{} / {} tokens ({}%)\n",
                used,
                window,
                (used * 100) / window.max(1)
            );
            println!("Lens: {}\n", lens_name);
            println!("History turn limit: {}\n", history_turn_limit);
            println!(
                "Compaction: threshold={} keep_turns={} summary_max_tokens={}\n",
                compaction_policy.threshold_tokens,
                compaction_policy.keep_turns,
                compaction_policy.summary_max_tokens
            );
            if let Some(report) = &state.last_prompt_report {
                println!("Last prompt assembly:");
                println!("  System:    {} tokens", report.system_tokens);
                println!("  Retrieval: {} tokens", report.retrieval_tokens);
                println!(
                    "    requested/effective: {}/{}",
                    report.retrieval_budget_requested, report.retrieval_budget_effective
                );
                println!("  History:   {} tokens", report.history_tokens);
                println!("    dropped messages: {}", report.dropped_history_messages);
                println!("    dropped retrieval: {}", report.dropped_retrieval_items);
                println!("  Reserved:  {} tokens", report.reserved_output_tokens);
                println!("  Output cap: {} tokens", report.output_token_cap);
                println!("  Budget:    {} tokens", report.total_input_budget);
                println!("  Flow left: {} tokens\n", report.flow_budget_remaining);
                println!(
                    "  Compaction: applied={}, compacted_messages={}\n",
                    report.compaction_applied, report.compacted_messages
                );
            }
            true
        }
        "/reset" => {
            state.reset_for_new_session();
            println!("Flow reset and rotated to a new session.\n");
            true
        }
        "/engine" => {
            let diagnostics = engine.diagnostics();
            let caps = &diagnostics.capabilities;
            println!("Current: {}/{}", agent_config.engine, agent_config.model);
            println!("Engine id: {}", diagnostics.engine_id);
            println!(
                "Configured model: {}",
                diagnostics.configured_model.as_deref().unwrap_or("n/a")
            );
            println!(
                "Endpoint: {}",
                diagnostics.endpoint.as_deref().unwrap_or("n/a")
            );
            println!(
                "Transport: {}",
                diagnostics.transport.as_deref().unwrap_or("n/a")
            );
            println!("Context window: {}\n", caps.context_window);
            println!("Output cap: {}\n", caps.max_output_tokens_per_turn);
            println!(
                "Capabilities: tools={}, streaming={}, manages_workspace={}\n",
                caps.supports_tool_use, caps.supports_streaming, caps.manages_own_workspace
            );
            true
        }
        "/help" => {
            println!("Commands:");
            println!("  /eco       — Eco lens (summaries)");
            println!("  /standard  — Standard lens (auto-expand)");
            println!("  /precise   — Precise lens (full content)");
            println!("  /engine    — Show current engine");
            println!("  /cost      — Token usage stats");
            println!("  /context   — Context window usage");
            println!("  /assign    — Delegated capability assignment (delegated mode only)");
            println!("  /assignments — List delegated assignments (session)");
            println!("  /assignments clear — Revoke and clear delegated assignments");
            println!("  /unassign  — Revoke by handoff id or dependent agent");
            println!("  /reset     — Clear conversation");
            println!("  /help      — This help\n");
            true
        }
        _ => {
            println!("Unknown command. Type /help for available commands.\n");
            true
        }
    }
}
