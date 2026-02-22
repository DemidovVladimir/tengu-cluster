//! Engine-turn and tool-call runtime execution helpers.
//!
//! Potential use case:
//! Isolate stream/event handling and tool execution policy checks from `main.rs`
//! so chat orchestration stays readable and testable.

use crate::tool_runtime::ToolRegistry;
use crate::{
    absorb_turn_usage_snapshot, apply_turn_usage_to_session_totals, as_u32_saturating,
    emit_domain_event,
};
use anyhow::Result;
use futures::StreamExt;
use std::path::PathBuf;
use tengu_core::config::{
    ensure_capability_governance_actor_allowed, evaluate_tool_approval_policy,
    evaluate_tool_policy, Config,
};
use tengu_core::events::{
    DomainEventPayload, EngineTurnCompleted, EngineTurnFailed, EventBus, ToolCallCompleted,
    ToolCallDenied, ToolCallStarted,
};
use tengu_core::token::estimate_tokens_approx_min1;
use tengu_core::types::Message;
use tengu_core::{Engine, EngineContext, ToolContext};

/// In-flight tool call assembly state for streamed tool arguments.
#[derive(Debug, Clone)]
pub(crate) struct PendingToolCall {
    pub id: String,
    pub name: String,
    pub arguments_delta: String,
}

/// Normalized tool execution outcome used by runtime rendering and audit logging.
#[derive(Debug, Clone)]
pub(crate) struct ToolExecutionOutcome {
    pub user_message: String,
    pub status: &'static str,
    pub reason: Option<String>,
    pub arguments_preview: Option<String>,
    pub result_preview: Option<String>,
}

/// Execute one engine call and collect text/usage events into session counters.
pub(crate) async fn collect_engine_response(
    engine: &dyn Engine,
    config: &Config,
    prompt_messages: &[Message],
    context: &EngineContext,
    flow_key: &str,
    agent_id: &str,
    agent_config: &tengu_core::config::AgentConfig,
    tool_registry: &ToolRegistry,
    event_bus: &dyn EventBus,
    turn_correlation_id: &str,
    total_input_tokens: &mut u32,
    total_output_tokens: &mut u32,
) -> Result<String> {
    match engine.run(prompt_messages, &[], context).await {
        Ok(mut stream) => {
            let mut response_text = String::new();
            let mut turn_usage_snapshot: Option<(u32, u32)> = None;
            let mut policy_terminal_message: Option<String> = None;
            let mut pending_tool_call: Option<PendingToolCall> = None;
            let mut tool_runtime_messages: Vec<String> = Vec::new();

            while let Some(event) = stream.next().await {
                match event {
                    tengu_core::types::StreamEvent::TextDelta { text } => {
                        response_text.push_str(&text);
                    }
                    tengu_core::types::StreamEvent::Usage {
                        input_tokens,
                        output_tokens,
                    } => {
                        absorb_turn_usage_snapshot(
                            &mut turn_usage_snapshot,
                            input_tokens,
                            output_tokens,
                        );
                    }
                    tengu_core::types::StreamEvent::Error { message } => {
                        emit_domain_event(
                            event_bus,
                            Some(agent_id),
                            Some(flow_key),
                            Some(turn_correlation_id),
                            DomainEventPayload::EngineTurnFailed(EngineTurnFailed {
                                reason: message.clone(),
                            }),
                        )
                        .await;
                        eprintln!("Engine error: {}", message);
                    }
                    tengu_core::types::StreamEvent::ToolCallStart { id, name } => {
                        if let Err(err) =
                            ensure_capability_governance_actor_allowed(config, agent_id)
                        {
                            let reason = err.to_string();
                            emit_domain_event(
                                event_bus,
                                Some(agent_id),
                                Some(flow_key),
                                Some(turn_correlation_id),
                                DomainEventPayload::ToolCallDenied(ToolCallDenied {
                                    tool_call_id: id.clone(),
                                    tool_name: name.clone(),
                                    phase: "governance".to_string(),
                                    reason: reason.clone(),
                                }),
                            )
                            .await;
                            policy_terminal_message = Some(format!(
                                "Tool call '{}' denied by capability governance: {}",
                                name, reason
                            ));
                            break;
                        }
                        emit_domain_event(
                            event_bus,
                            Some(agent_id),
                            Some(flow_key),
                            Some(turn_correlation_id),
                            DomainEventPayload::ToolCallStarted(ToolCallStarted {
                                tool_call_id: id.clone(),
                                tool_name: name.clone(),
                            }),
                        )
                        .await;
                        if pending_tool_call.is_some() {
                            emit_domain_event(
                                event_bus,
                                Some(agent_id),
                                Some(flow_key),
                                Some(turn_correlation_id),
                                DomainEventPayload::ToolCallDenied(ToolCallDenied {
                                    tool_call_id: id.clone(),
                                    tool_name: name.clone(),
                                    phase: "protocol".to_string(),
                                    reason: "runtime supports only one active tool call"
                                        .to_string(),
                                }),
                            )
                            .await;
                            policy_terminal_message = Some(
                                "Tool runtime currently supports one active tool call at a time."
                                    .to_string(),
                            );
                            break;
                        }
                        let decision = evaluate_tool_policy(agent_config, &name);
                        if !decision.is_allowed() {
                            policy_terminal_message = Some(format!(
                                "Tool call '{}' denied by policy for agent '{}': {}",
                                name,
                                agent_id,
                                decision.reason()
                            ));
                            emit_domain_event(
                                event_bus,
                                Some(agent_id),
                                Some(flow_key),
                                Some(turn_correlation_id),
                                DomainEventPayload::ToolCallDenied(ToolCallDenied {
                                    tool_call_id: id.clone(),
                                    tool_name: name.clone(),
                                    phase: "policy".to_string(),
                                    reason: decision.reason().to_string(),
                                }),
                            )
                            .await;
                            break;
                        }
                        pending_tool_call = Some(PendingToolCall {
                            id,
                            name,
                            arguments_delta: String::new(),
                        });
                    }
                    tengu_core::types::StreamEvent::ToolCallDelta {
                        id,
                        arguments_delta,
                    } => {
                        if let Some(pending) = pending_tool_call.as_mut() {
                            if pending.id != id {
                                emit_domain_event(
                                    event_bus,
                                    Some(agent_id),
                                    Some(flow_key),
                                    Some(turn_correlation_id),
                                    DomainEventPayload::ToolCallDenied(ToolCallDenied {
                                        tool_call_id: pending.id.clone(),
                                        tool_name: pending.name.clone(),
                                        phase: "protocol".to_string(),
                                        reason: format!(
                                            "delta id mismatch: expected '{}', got '{}'",
                                            pending.id, id
                                        ),
                                    }),
                                )
                                .await;
                                policy_terminal_message = Some(format!(
                                    "Tool call delta id mismatch: expected '{}', got '{}'.",
                                    pending.id, id
                                ));
                                break;
                            }
                            pending.arguments_delta.push_str(&arguments_delta);
                        }
                    }
                    tengu_core::types::StreamEvent::ToolCallEnd { id } => {
                        if let Some(pending) = pending_tool_call.take() {
                            if pending.id != id {
                                emit_domain_event(
                                    event_bus,
                                    Some(agent_id),
                                    Some(flow_key),
                                    Some(turn_correlation_id),
                                    DomainEventPayload::ToolCallDenied(ToolCallDenied {
                                        tool_call_id: pending.id.clone(),
                                        tool_name: pending.name.clone(),
                                        phase: "protocol".to_string(),
                                        reason: format!(
                                            "end id mismatch: expected '{}', got '{}'",
                                            pending.id, id
                                        ),
                                    }),
                                )
                                .await;
                                policy_terminal_message = Some(format!(
                                    "Tool call end id mismatch: expected '{}', got '{}'.",
                                    pending.id, id
                                ));
                                break;
                            }
                            let outcome = execute_tool_call(
                                &pending,
                                config,
                                agent_config,
                                tool_registry,
                                context.workspace.as_ref(),
                                agent_id,
                            )
                            .await;
                            match outcome.status {
                                "denied_approval" | "denied_policy" => {
                                    let phase = if outcome.status == "denied_approval" {
                                        "approval"
                                    } else {
                                        "policy"
                                    };
                                    emit_domain_event(
                                        event_bus,
                                        Some(agent_id),
                                        Some(flow_key),
                                        Some(turn_correlation_id),
                                        DomainEventPayload::ToolCallDenied(ToolCallDenied {
                                            tool_call_id: pending.id.clone(),
                                            tool_name: pending.name.clone(),
                                            phase: phase.to_string(),
                                            reason: outcome.reason.clone().unwrap_or_else(|| {
                                                "tool denied by runtime policy".to_string()
                                            }),
                                        }),
                                    )
                                    .await;
                                    tool_runtime_messages.push(outcome.user_message);
                                }
                                _ => {
                                    emit_domain_event(
                                        event_bus,
                                        Some(agent_id),
                                        Some(flow_key),
                                        Some(turn_correlation_id),
                                        DomainEventPayload::ToolCallCompleted(ToolCallCompleted {
                                            tool_call_id: pending.id.clone(),
                                            tool_name: pending.name.clone(),
                                            status: outcome.status.to_string(),
                                            reason: outcome.reason.clone(),
                                            arguments_preview: outcome.arguments_preview.clone(),
                                            result_preview: outcome.result_preview.clone(),
                                        }),
                                    )
                                    .await;
                                    tool_runtime_messages.push(outcome.user_message);
                                }
                            }
                        }
                    }
                    tengu_core::types::StreamEvent::Done => {}
                    _ => {}
                }
            }
            apply_turn_usage_to_session_totals(
                total_input_tokens,
                total_output_tokens,
                turn_usage_snapshot,
            );
            if let Some(message) = policy_terminal_message {
                emit_domain_event(
                    event_bus,
                    Some(agent_id),
                    Some(flow_key),
                    Some(turn_correlation_id),
                    DomainEventPayload::EngineTurnCompleted(EngineTurnCompleted {
                        input_tokens: turn_usage_snapshot.map(|(input, _)| input).unwrap_or(0),
                        output_tokens: turn_usage_snapshot.map(|(_, output)| output).unwrap_or(0),
                        response_tokens: as_u32_saturating(estimate_tokens_approx_min1(&message)),
                    }),
                )
                .await;
                return Ok(message);
            }
            if let Some(pending) = pending_tool_call {
                emit_domain_event(
                    event_bus,
                    Some(agent_id),
                    Some(flow_key),
                    Some(turn_correlation_id),
                    DomainEventPayload::ToolCallDenied(ToolCallDenied {
                        tool_call_id: pending.id.clone(),
                        tool_name: pending.name.clone(),
                        phase: "protocol".to_string(),
                        reason: "missing ToolCallEnd".to_string(),
                    }),
                )
                .await;
                return Ok(format!(
                    "Tool call '{}' did not complete (missing ToolCallEnd).",
                    pending.name
                ));
            }
            if !tool_runtime_messages.is_empty() {
                let tool_block = tool_runtime_messages.join("\n\n");
                if response_text.trim().is_empty() {
                    return Ok(tool_block);
                }
                response_text.push_str("\n\n");
                response_text.push_str(&tool_block);
            }
            emit_domain_event(
                event_bus,
                Some(agent_id),
                Some(flow_key),
                Some(turn_correlation_id),
                DomainEventPayload::EngineTurnCompleted(EngineTurnCompleted {
                    input_tokens: turn_usage_snapshot.map(|(input, _)| input).unwrap_or(0),
                    output_tokens: turn_usage_snapshot.map(|(_, output)| output).unwrap_or(0),
                    response_tokens: as_u32_saturating(estimate_tokens_approx_min1(&response_text)),
                }),
            )
            .await;
            Ok(response_text)
        }
        Err(err) => {
            emit_domain_event(
                event_bus,
                Some(agent_id),
                Some(flow_key),
                Some(turn_correlation_id),
                DomainEventPayload::EngineTurnFailed(EngineTurnFailed {
                    reason: err.to_string(),
                }),
            )
            .await;
            eprintln!("Engine error: {}\n", err);
            Ok(String::new())
        }
    }
}

/// Execute one assembled tool call through registry and return user-facing result text.
///
/// Runtime applies defense-in-depth checks before execution:
/// - capability governance actor gate (`user` / `delegated`)
/// - `kit` allow/deny policy
/// - approval policy (`kit.approval_required` + `kit.approved` + tool metadata)
pub(crate) async fn execute_tool_call(
    pending: &PendingToolCall,
    config: &Config,
    agent_config: &tengu_core::config::AgentConfig,
    tool_registry: &ToolRegistry,
    workspace: Option<&PathBuf>,
    agent_id: &str,
) -> ToolExecutionOutcome {
    if let Err(err) = ensure_capability_governance_actor_allowed(config, agent_id) {
        return ToolExecutionOutcome {
            user_message: format!(
                "[tool:{} denied]\nCapability governance denied runtime actor '{}': {}",
                pending.name, agent_id, err
            ),
            status: "denied_governance",
            reason: Some(err.to_string()),
            arguments_preview: Some(truncate_preview(&pending.arguments_delta, 200)),
            result_preview: Some(truncate_preview(&err.to_string(), 200)),
        };
    }

    let args_value = if pending.arguments_delta.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str::<serde_json::Value>(&pending.arguments_delta) {
            Ok(parsed) => parsed,
            Err(err) => {
                return ToolExecutionOutcome {
                    user_message: format!(
                        "[tool:{} parse-error]\nInvalid JSON arguments: {}",
                        pending.name, err
                    ),
                    status: "parse_error",
                    reason: Some(err.to_string()),
                    arguments_preview: Some(truncate_preview(&pending.arguments_delta, 200)),
                    result_preview: Some(truncate_preview(&err.to_string(), 200)),
                };
            }
        }
    };

    if !tool_registry.has(&pending.name) {
        return ToolExecutionOutcome {
            user_message: format!("[tool:{} error]\nTool is not registered.", pending.name),
            status: "not_registered",
            reason: Some("tool is not registered".to_string()),
            arguments_preview: Some(truncate_preview(&pending.arguments_delta, 200)),
            result_preview: Some("tool is not registered".to_string()),
        };
    }

    let policy_decision = evaluate_tool_policy(agent_config, &pending.name);
    if !policy_decision.is_allowed() {
        return ToolExecutionOutcome {
            user_message: format!(
                "[tool:{} denied]\nTool blocked by policy for agent '{}': {}",
                pending.name,
                agent_id,
                policy_decision.reason()
            ),
            status: "denied_policy",
            reason: Some(policy_decision.reason().to_string()),
            arguments_preview: Some(truncate_preview(&pending.arguments_delta, 200)),
            result_preview: Some(truncate_preview(policy_decision.reason(), 200)),
        };
    }

    let policy_metadata = tool_registry
        .policy_metadata(&pending.name)
        .unwrap_or_default();
    let approval_decision = evaluate_tool_approval_policy(
        agent_config,
        &pending.name,
        policy_metadata.requires_approval,
    );
    if !approval_decision.is_allowed() {
        return ToolExecutionOutcome {
            user_message: format!(
                "[tool:{} denied]\nTool requires approval. Add '{}' to agents.{}.kit.approved in config.",
                pending.name, pending.name, agent_id
            ),
            status: "denied_approval",
            reason: Some(approval_decision.reason().to_string()),
            arguments_preview: Some(truncate_preview(&pending.arguments_delta, 200)),
            result_preview: Some(truncate_preview(approval_decision.reason(), 200)),
        };
    }

    let Some(workspace) = workspace else {
        return ToolExecutionOutcome {
            user_message: format!(
                "[tool:{} error]\nWorkspace is not configured for this agent.",
                pending.name
            ),
            status: "workspace_missing",
            reason: Some("workspace is not configured".to_string()),
            arguments_preview: Some(truncate_preview(&pending.arguments_delta, 200)),
            result_preview: Some("workspace is not configured".to_string()),
        };
    };

    let tool_ctx = ToolContext {
        workspace: workspace.clone(),
        agent_id: agent_id.to_string(),
    };
    match tool_registry
        .execute(&pending.name, args_value, &tool_ctx)
        .await
    {
        Ok(output) if output.is_error => ToolExecutionOutcome {
            user_message: format!("[tool:{} error]\n{}", pending.name, output.content),
            status: "error",
            reason: Some("tool returned error output".to_string()),
            arguments_preview: Some(truncate_preview(&pending.arguments_delta, 200)),
            result_preview: Some(truncate_preview(&output.content, 400)),
        },
        Ok(output) => ToolExecutionOutcome {
            user_message: format!("[tool:{} ok]\n{}", pending.name, output.content),
            status: "ok",
            reason: None,
            arguments_preview: Some(truncate_preview(&pending.arguments_delta, 200)),
            result_preview: Some(truncate_preview(&output.content, 400)),
        },
        Err(err) => ToolExecutionOutcome {
            user_message: format!("[tool:{} error]\n{}", pending.name, err),
            status: "exec_error",
            reason: Some(err.to_string()),
            arguments_preview: Some(truncate_preview(&pending.arguments_delta, 200)),
            result_preview: Some(truncate_preview(&err.to_string(), 400)),
        },
    }
}

/// Build compact single-line preview for audit payload fields.
fn truncate_preview(value: &str, max_chars: usize) -> String {
    let compact = value
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if compact.len() <= max_chars {
        compact
    } else {
        format!("{}…", compact.chars().take(max_chars).collect::<String>())
    }
}
