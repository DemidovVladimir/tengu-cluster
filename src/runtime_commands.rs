//! Runtime slash-command handlers for chat/control-plane flows.
//!
//! Potential use case:
//! Keep `main.rs` focused on orchestration while command parsing/mutation logic
//! lives in one place that is easier to test and evolve.

use crate::handoff_validator::{run_auto_validation_for_assignment, AutoValidationAction};
use crate::{
    build_assignment_envelope, emit_domain_event, ensure_delegated_orchestrator, now_epoch_ms,
    parse_assign_command, parse_unassign_command, ChatLoopState, FlowCompactionPolicy,
};
use tengu_core::config::Config;
use tengu_core::events::{
    DomainEventPayload, EventBus, HandoffResultReceived, HandoffTaskDispatched,
};
use tengu_core::token::estimate_tokens_approx_min1;
use tengu_core::types::{
    HandoffResultEnvelope, HandoffResultStatus, HandoffTaskEnvelope, HandoffValidationDecision,
    HANDOFF_SCHEMA_VERSION,
};
use tengu_core::{Engine, Lens};

/// Default output cap used when redispatching delegated tasks from validation gate.
const REDISPATCH_MAX_OUTPUT_TOKENS: u32 = 256;
/// Default TTL used when redispatching delegated tasks from validation gate.
const REDISPATCH_TTL_SECS: u32 = 900;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedHandoffCommand {
    Pending,
    Auto {
        handoff_id: String,
        note: Option<String>,
    },
    Decide {
        decision: HandoffValidationDecision,
        handoff_id: String,
        note: Option<String>,
    },
}

/// Process orchestrator control-plane commands and return `true` when handled.
///
/// Supported commands:
/// - `/assign <dependent> <cap1,cap2,...> [objective...]`
/// - `/assignments`
/// - `/unassign <handoff-id|dependent-agent-id>`
/// - `/assignments clear`
/// - `/handoff pending`
/// - `/handoff auto <handoff-id> [note...]`
/// - `/handoff <accept|retry|rework|fail> <handoff-id> [note...]`
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

    if command.trim_start().starts_with("/handoff") {
        if let Err(err) = ensure_delegated_orchestrator(config, agent_id) {
            println!("Delegated handoff validation denied: {}\n", err);
            return true;
        }

        let Some(parsed) = parse_handoff_command(command) else {
            println!(
                "Usage: /handoff pending | /handoff auto <handoff-id> [note...] | /handoff <accept|retry|rework|fail> <handoff-id> [note...]\n"
            );
            return true;
        };

        match parsed {
            ParsedHandoffCommand::Pending => {
                if state.capability_assignments.is_empty() {
                    println!("No delegated handoffs waiting in this session.\n");
                    return true;
                }
                println!("Delegated Handoff Validation Queue");
                println!("─────────────────────────────");
                for (idx, assignment) in state.capability_assignments.iter().enumerate() {
                    println!(
                        " {}. handoff_id={} dependent={} attempts={}",
                        idx + 1,
                        assignment.handoff_id,
                        assignment.dependent_agent_id,
                        assignment.validation_attempts
                    );
                    println!("    objective: {}", assignment.objective);
                }
                println!();
                return true;
            }
            ParsedHandoffCommand::Auto { handoff_id, note } => {
                let Some(assignment_idx) = state
                    .capability_assignments
                    .iter()
                    .position(|assignment| assignment.handoff_id == handoff_id)
                else {
                    println!(
                        "Unknown handoff '{}' in active delegated assignments.\n",
                        handoff_id
                    );
                    return true;
                };

                let mut evaluation_input = state.capability_assignments[assignment_idx].clone();
                evaluation_input.validation_attempts =
                    evaluation_input.validation_attempts.saturating_add(1);
                state.capability_assignments[assignment_idx].validation_attempts =
                    evaluation_input.validation_attempts;

                match run_auto_validation_for_assignment(config, &evaluation_input).await {
                    AutoValidationAction::Accept { report } => {
                        let assignment = state.capability_assignments.remove(assignment_idx);
                        let summary = compose_handoff_note(note.as_deref(), &report);
                        let result = build_handoff_validation_result(
                            &assignment,
                            HandoffValidationDecision::Accept,
                            Some(&summary),
                            agent_id,
                        );
                        emit_domain_event(
                            event_bus,
                            Some(agent_id),
                            Some(&assignment.flow_key),
                            Some(turn_correlation_id),
                            DomainEventPayload::HandoffResultReceived(HandoffResultReceived {
                                envelope: result,
                            }),
                        )
                        .await;
                        println!(
                            "Handoff '{}' accepted by auto validation.\n{}\n",
                            assignment.handoff_id, report
                        );
                    }
                    AutoValidationAction::Retry { report } => {
                        if state.delegated_execution_paused {
                            println!(
                                "Auto validation suggested retry, but delegated execution is force-stopped.\n{}\n",
                                report
                            );
                            return true;
                        }

                        let assignment = &mut state.capability_assignments[assignment_idx];
                        assignment.issued_at_epoch_ms = now_epoch_ms();
                        let guidance = compose_handoff_note(note.as_deref(), &report);
                        let objective = format!(
                            "{}\n\nRetry guidance from orchestrator:\n{}",
                            assignment.objective, guidance
                        );
                        let envelope = build_assignment_redispatch_task(
                            assignment,
                            objective,
                            HandoffValidationDecision::Retry,
                        );
                        emit_domain_event(
                            event_bus,
                            Some(agent_id),
                            Some(&assignment.flow_key),
                            Some(turn_correlation_id),
                            DomainEventPayload::HandoffTaskDispatched(HandoffTaskDispatched {
                                envelope,
                            }),
                        )
                        .await;
                        println!(
                            "Handoff '{}' queued for retry by auto validation.\n{}\n",
                            assignment.handoff_id, report
                        );
                    }
                    AutoValidationAction::Fail { report } => {
                        let assignment = state.capability_assignments.remove(assignment_idx);
                        let reason = compose_handoff_note(note.as_deref(), &report);
                        let result = build_handoff_validation_result(
                            &assignment,
                            HandoffValidationDecision::Fail,
                            Some(&reason),
                            agent_id,
                        );
                        emit_domain_event(
                            event_bus,
                            Some(agent_id),
                            Some(&assignment.flow_key),
                            Some(turn_correlation_id),
                            DomainEventPayload::HandoffResultReceived(HandoffResultReceived {
                                envelope: result,
                            }),
                        )
                        .await;
                        println!(
                            "Handoff '{}' failed by auto validation.\n{}\n",
                            assignment.handoff_id, report
                        );
                    }
                }
                return true;
            }
            ParsedHandoffCommand::Decide {
                decision,
                handoff_id,
                note,
            } => {
                let Some(assignment_idx) = state
                    .capability_assignments
                    .iter()
                    .position(|assignment| assignment.handoff_id == handoff_id)
                else {
                    println!(
                        "Unknown handoff '{}' in active delegated assignments.\n",
                        handoff_id
                    );
                    return true;
                };

                if decision.requires_redispatch() && state.delegated_execution_paused {
                    println!(
                        "Delegated execution is force-stopped. Restart runtime before /handoff {}.\n",
                        decision.as_str()
                    );
                    return true;
                }

                match decision {
                    HandoffValidationDecision::Accept => {
                        let assignment = state.capability_assignments.remove(assignment_idx);
                        let result = build_handoff_validation_result(
                            &assignment,
                            decision,
                            note.as_deref(),
                            agent_id,
                        );
                        emit_domain_event(
                            event_bus,
                            Some(agent_id),
                            Some(&assignment.flow_key),
                            Some(turn_correlation_id),
                            DomainEventPayload::HandoffResultReceived(HandoffResultReceived {
                                envelope: result,
                            }),
                        )
                        .await;
                        println!(
                            "Handoff '{}' accepted. Downstream use is now allowed.\n",
                            assignment.handoff_id
                        );
                    }
                    HandoffValidationDecision::Fail => {
                        let assignment = state.capability_assignments.remove(assignment_idx);
                        let result = build_handoff_validation_result(
                            &assignment,
                            decision,
                            note.as_deref(),
                            agent_id,
                        );
                        emit_domain_event(
                            event_bus,
                            Some(agent_id),
                            Some(&assignment.flow_key),
                            Some(turn_correlation_id),
                            DomainEventPayload::HandoffResultReceived(HandoffResultReceived {
                                envelope: result,
                            }),
                        )
                        .await;
                        println!(
                            "Handoff '{}' marked failed by validation gate.\n",
                            assignment.handoff_id
                        );
                    }
                    HandoffValidationDecision::Retry => {
                        let assignment = &mut state.capability_assignments[assignment_idx];
                        assignment.issued_at_epoch_ms = now_epoch_ms();
                        let objective = match note.as_deref() {
                            Some(guidance) => format!(
                                "{}\n\nRetry guidance from orchestrator:\n{}",
                                assignment.objective, guidance
                            ),
                            None => assignment.objective.clone(),
                        };
                        let envelope = build_assignment_redispatch_task(
                            assignment,
                            objective,
                            HandoffValidationDecision::Retry,
                        );
                        emit_domain_event(
                            event_bus,
                            Some(agent_id),
                            Some(&assignment.flow_key),
                            Some(turn_correlation_id),
                            DomainEventPayload::HandoffTaskDispatched(HandoffTaskDispatched {
                                envelope,
                            }),
                        )
                        .await;
                        println!("Handoff '{}' queued for retry.\n", assignment.handoff_id);
                    }
                    HandoffValidationDecision::Rework => {
                        let Some(updated_objective) = note else {
                            println!("Usage: /handoff rework <handoff-id> <new objective...>\n");
                            return true;
                        };
                        let assignment = &mut state.capability_assignments[assignment_idx];
                        assignment.objective = updated_objective.clone();
                        assignment.issued_at_epoch_ms = now_epoch_ms();
                        let envelope = build_assignment_redispatch_task(
                            assignment,
                            updated_objective,
                            HandoffValidationDecision::Rework,
                        );
                        emit_domain_event(
                            event_bus,
                            Some(agent_id),
                            Some(&assignment.flow_key),
                            Some(turn_correlation_id),
                            DomainEventPayload::HandoffTaskDispatched(HandoffTaskDispatched {
                                envelope,
                            }),
                        )
                        .await;
                        println!(
                            "Handoff '{}' queued for rework with updated objective.\n",
                            assignment.handoff_id
                        );
                    }
                }
                return true;
            }
        }
    }

    if command.trim_start().starts_with("/assign") {
        if state.delegated_execution_paused {
            println!(
                "Delegated execution is force-stopped. Restart runtime to re-enable /assign.\n"
            );
            return true;
        }

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

/// Parse `/handoff ...` command.
///
/// Expected shapes:
/// - `/handoff pending`
/// - `/handoff auto <handoff-id> [note...]`
/// - `/handoff <accept|retry|rework|fail> <handoff-id> [note...]`
fn parse_handoff_command(input: &str) -> Option<ParsedHandoffCommand> {
    let mut parts = input.split_whitespace();
    if parts.next()? != "/handoff" {
        return None;
    }

    let action = parts.next()?;
    if action == "pending" {
        return if parts.next().is_none() {
            Some(ParsedHandoffCommand::Pending)
        } else {
            None
        };
    }
    if action == "auto" {
        let handoff_id = parts.next()?.trim().to_string();
        if handoff_id.is_empty() {
            return None;
        }
        let note = parts.collect::<Vec<_>>().join(" ");
        let note = if note.trim().is_empty() {
            None
        } else {
            Some(note)
        };
        return Some(ParsedHandoffCommand::Auto { handoff_id, note });
    }

    let decision = HandoffValidationDecision::parse(action)?;
    let handoff_id = parts.next()?.trim().to_string();
    if handoff_id.is_empty() {
        return None;
    }
    let note = parts.collect::<Vec<_>>().join(" ");
    let note = if note.trim().is_empty() {
        None
    } else {
        Some(note)
    };
    Some(ParsedHandoffCommand::Decide {
        decision,
        handoff_id,
        note,
    })
}

/// Combine optional operator note with automated validation report output.
fn compose_handoff_note(note: Option<&str>, report: &str) -> String {
    let note = note.map(str::trim).filter(|entry| !entry.is_empty());
    if report.trim().is_empty() {
        return note.unwrap_or_default().to_string();
    }
    match note {
        Some(note) => format!("{}\n\n{}", note, report),
        None => report.to_string(),
    }
}

/// Build terminal handoff-result envelope used for assignment cleanup/revocation.
///
/// This helper is shared by multiple cleanup paths:
/// - manual revocation (`/unassign`, `/assignments clear`)
/// - TTL expiry cleanup
/// - emergency delegated worker stop (`/stopall`)
pub(crate) fn build_assignment_revocation_result(
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

/// Build one handoff result emitted by orchestrator validation decision.
fn build_handoff_validation_result(
    assignment: &crate::CapabilityAssignmentRecord,
    decision: HandoffValidationDecision,
    note: Option<&str>,
    validator_agent_id: &str,
) -> HandoffResultEnvelope {
    let (status, summary, output_tokens, error_reason) = match decision {
        HandoffValidationDecision::Accept => {
            let summary = note
                .unwrap_or("accepted by orchestrator validation gate")
                .to_string();
            (
                HandoffResultStatus::Completed,
                summary.clone(),
                estimate_tokens_approx_min1(&summary) as u32,
                None,
            )
        }
        HandoffValidationDecision::Fail => (
            HandoffResultStatus::Failed,
            String::new(),
            0,
            Some(
                note.unwrap_or("failed by orchestrator validation gate")
                    .to_string(),
            ),
        ),
        // Retry/Rework emit redispatch task envelopes, not terminal results.
        HandoffValidationDecision::Retry | HandoffValidationDecision::Rework => (
            HandoffResultStatus::Failed,
            String::new(),
            0,
            Some("internal validation command misuse".to_string()),
        ),
    };

    HandoffResultEnvelope {
        schema_version: HANDOFF_SCHEMA_VERSION,
        handoff_id: assignment.handoff_id.clone(),
        flow_key: assignment.flow_key.clone(),
        from_agent_id: assignment.dependent_agent_id.clone(),
        to_agent_id: assignment.orchestrator_agent_id.clone(),
        status,
        summary,
        artifacts: Vec::new(),
        output_tokens,
        error_reason,
        metadata: std::collections::HashMap::from([
            (
                "validation_decision".to_string(),
                decision.as_str().to_string(),
            ),
            (
                "validator_agent_id".to_string(),
                validator_agent_id.to_string(),
            ),
        ]),
    }
}

/// Build redispatch task envelope for `retry`/`rework` validation decisions.
fn build_assignment_redispatch_task(
    assignment: &crate::CapabilityAssignmentRecord,
    objective: String,
    decision: HandoffValidationDecision,
) -> HandoffTaskEnvelope {
    HandoffTaskEnvelope {
        schema_version: HANDOFF_SCHEMA_VERSION,
        handoff_id: assignment.handoff_id.clone(),
        flow_key: assignment.flow_key.clone(),
        from_agent_id: assignment.orchestrator_agent_id.clone(),
        to_agent_id: assignment.dependent_agent_id.clone(),
        objective,
        constraints: vec![
            "bounded-by-user-policy".to_string(),
            "validation-gate-redispatch".to_string(),
        ],
        requested_capabilities: assignment.requested_capabilities.clone(),
        context_summary: None,
        max_output_tokens: REDISPATCH_MAX_OUTPUT_TOKENS,
        ttl_seconds: Some(REDISPATCH_TTL_SECS),
        metadata: std::collections::HashMap::from([
            ("handoff_depth".to_string(), "1".to_string()),
            (
                "validation_decision".to_string(),
                decision.as_str().to_string(),
            ),
        ]),
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
            println!("  /handoff pending — List delegated handoffs awaiting validation");
            println!("  /handoff auto <handoff-id> [note...]");
            println!("  /handoff <accept|retry|rework|fail> <handoff-id> [note...]");
            println!("  /unassign  — Revoke by handoff id or dependent agent");
            println!("  /stopall   — Force-stop delegated workers and revoke assignments");
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

#[cfg(test)]
mod tests {
    use super::*;

    fn assignment() -> crate::CapabilityAssignmentRecord {
        crate::CapabilityAssignmentRecord {
            handoff_id: "h-1".to_string(),
            flow_key: "flow-1".to_string(),
            orchestrator_agent_id: "orchestrator".to_string(),
            dependent_agent_id: "worker".to_string(),
            requested_capabilities: vec!["tool:read_file".to_string()],
            objective: "inspect files".to_string(),
            issued_at_epoch_ms: 1_000,
            validation_attempts: 0,
        }
    }

    #[test]
    fn parse_handoff_command_supports_pending_and_decisions() {
        assert_eq!(
            parse_handoff_command("/handoff pending"),
            Some(ParsedHandoffCommand::Pending)
        );
        assert_eq!(
            parse_handoff_command("/handoff accept h-1 good"),
            Some(ParsedHandoffCommand::Decide {
                decision: HandoffValidationDecision::Accept,
                handoff_id: "h-1".to_string(),
                note: Some("good".to_string()),
            })
        );
        assert_eq!(
            parse_handoff_command("/handoff retry h-1"),
            Some(ParsedHandoffCommand::Decide {
                decision: HandoffValidationDecision::Retry,
                handoff_id: "h-1".to_string(),
                note: None,
            })
        );
        assert_eq!(
            parse_handoff_command("/handoff auto h-1 run checks"),
            Some(ParsedHandoffCommand::Auto {
                handoff_id: "h-1".to_string(),
                note: Some("run checks".to_string()),
            })
        );
        assert!(parse_handoff_command("/handoff").is_none());
        assert!(parse_handoff_command("/handoff unknown h-1").is_none());
    }

    #[test]
    fn build_handoff_validation_result_marks_accept_as_completed() {
        let assignment = assignment();
        let result = build_handoff_validation_result(
            &assignment,
            HandoffValidationDecision::Accept,
            Some("approved output"),
            "orchestrator",
        );

        assert_eq!(result.status, HandoffResultStatus::Completed);
        assert_eq!(result.summary, "approved output");
        assert!(result.error_reason.is_none());
        assert_eq!(
            result
                .metadata
                .get("validation_decision")
                .map(String::as_str),
            Some("accept")
        );
    }

    #[test]
    fn build_assignment_redispatch_task_carries_validation_decision_metadata() {
        let assignment = assignment();
        let envelope = build_assignment_redispatch_task(
            &assignment,
            "retry objective".to_string(),
            HandoffValidationDecision::Retry,
        );

        assert_eq!(envelope.handoff_id, "h-1");
        assert_eq!(envelope.objective, "retry objective");
        assert_eq!(
            envelope
                .metadata
                .get("validation_decision")
                .map(String::as_str),
            Some("retry")
        );
    }
}
