//! Orchestrator control-plane helpers for delegated capability assignment.
//!
//! Potential use case:
//! Parse `/assign ...` runtime commands, validate orchestrator/dependent bounds,
//! and build typed handoff envelopes constrained by user-defined policy.

use anyhow::{anyhow, Result};
use tengu_core::config::{
    evaluate_handoff_capability_policy, Config, HandoffCapabilityPolicyDecision,
};
use tengu_core::types::{HandoffTaskEnvelope, HANDOFF_SCHEMA_VERSION};

/// Parsed `/assign` command payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAssignCommand {
    /// Target dependent agent id.
    pub dependent_agent_id: String,
    /// Requested bounded capabilities (`tool:*`, `skill:*`, `engine:*`).
    pub requested_capabilities: Vec<String>,
    /// Human-readable assignment objective.
    pub objective: String,
}

/// Runtime-visible record for one approved delegated assignment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityAssignmentRecord {
    /// Stable handoff/assignment id.
    pub handoff_id: String,
    /// Runtime flow key where assignment was requested.
    pub flow_key: String,
    /// Orchestrator agent id that issued assignment.
    pub orchestrator_agent_id: String,
    /// Dependent agent id assigned by orchestrator.
    pub dependent_agent_id: String,
    /// Capability list approved by user-boundary policy.
    pub requested_capabilities: Vec<String>,
    /// Assignment objective.
    pub objective: String,
    /// Issued timestamp (epoch milliseconds).
    pub issued_at_epoch_ms: u64,
}

impl CapabilityAssignmentRecord {
    /// Build assignment record from validated handoff envelope.
    pub fn from_envelope(envelope: &HandoffTaskEnvelope, issued_at_epoch_ms: u64) -> Self {
        Self {
            handoff_id: envelope.handoff_id.clone(),
            flow_key: envelope.flow_key.clone(),
            orchestrator_agent_id: envelope.from_agent_id.clone(),
            dependent_agent_id: envelope.to_agent_id.clone(),
            requested_capabilities: envelope.requested_capabilities.clone(),
            objective: envelope.objective.clone(),
            issued_at_epoch_ms,
        }
    }
}

/// Parse `/assign` command.
///
/// Expected shape:
/// `/assign <dependent-agent-id> <cap1,cap2,...> [objective text...]`
pub fn parse_assign_command(input: &str) -> Option<ParsedAssignCommand> {
    let mut parts = input.split_whitespace();
    if parts.next()? != "/assign" {
        return None;
    }
    let dependent_agent_id = parts.next()?.trim().to_string();
    let requested_capabilities: Vec<String> = parts
        .next()?
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if dependent_agent_id.is_empty() || requested_capabilities.is_empty() {
        return None;
    }
    let objective_suffix = parts.collect::<Vec<_>>().join(" ");
    let objective = if objective_suffix.trim().is_empty() {
        format!(
            "Delegated capability assignment for '{}'",
            dependent_agent_id
        )
    } else {
        objective_suffix
    };
    Some(ParsedAssignCommand {
        dependent_agent_id,
        requested_capabilities,
        objective,
    })
}

/// Parse `/unassign` command.
///
/// Expected shape:
/// `/unassign <handoff-id|dependent-agent-id>`
pub fn parse_unassign_command(input: &str) -> Option<String> {
    let mut parts = input.split_whitespace();
    if parts.next()? != "/unassign" {
        return None;
    }
    let target = parts.next()?.trim();
    if target.is_empty() || parts.next().is_some() {
        return None;
    }
    Some(target.to_string())
}

/// Ensure delegated governance mode is enabled and caller is orchestrator.
///
/// Returns normalized delegated orchestrator id when validation passes.
pub fn ensure_delegated_orchestrator(
    config: &Config,
    orchestrator_agent_id: &str,
) -> Result<String> {
    if config.capability_governance.mode.trim() != "delegated" {
        return Err(anyhow!(
            "delegated assignment is disabled (capability_governance.mode must be 'delegated')"
        ));
    }

    let delegated = config
        .capability_governance
        .delegated_orchestrator_agent
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    if delegated.is_empty() {
        return Err(anyhow!(
            "capability_governance.delegated_orchestrator_agent is not configured"
        ));
    }
    if orchestrator_agent_id.trim() != delegated {
        return Err(anyhow!(
            "agent '{}' is not delegated orchestrator '{}'",
            orchestrator_agent_id,
            delegated
        ));
    }
    Ok(delegated.to_string())
}

/// Build and validate delegated capability-assignment envelope.
///
/// Validation sequence:
/// 1. Governance mode must be `delegated`.
/// 2. Caller must match configured delegated orchestrator.
/// 3. Dependent must be a configured non-orchestrator agent.
/// 4. Requested capabilities must pass user-boundary handoff policy checks.
pub fn build_assignment_envelope(
    config: &Config,
    flow_key: &str,
    orchestrator_agent_id: &str,
    command: &ParsedAssignCommand,
) -> Result<HandoffTaskEnvelope> {
    let delegated = ensure_delegated_orchestrator(config, orchestrator_agent_id)?;

    let dependent = command.dependent_agent_id.trim();
    if !config.agents.contains_key(dependent) {
        return Err(anyhow!("dependent agent '{}' is not configured", dependent));
    }
    if dependent == delegated {
        return Err(anyhow!(
            "dependent agent '{}' must differ from orchestrator '{}'",
            dependent,
            delegated
        ));
    }

    let envelope = HandoffTaskEnvelope {
        schema_version: HANDOFF_SCHEMA_VERSION,
        handoff_id: uuid::Uuid::new_v4().to_string(),
        flow_key: flow_key.to_string(),
        from_agent_id: orchestrator_agent_id.to_string(),
        to_agent_id: dependent.to_string(),
        objective: command.objective.clone(),
        constraints: vec!["bounded-by-user-policy".to_string()],
        requested_capabilities: command.requested_capabilities.clone(),
        context_summary: None,
        max_output_tokens: 256,
        ttl_seconds: Some(900),
        metadata: std::collections::HashMap::new(),
    };

    match evaluate_handoff_capability_policy(config, &envelope) {
        HandoffCapabilityPolicyDecision::Allowed => Ok(envelope),
        other => Err(anyhow!(
            "assignment denied by user-boundary policy: {}",
            describe_handoff_policy_decision(&other)
        )),
    }
}

/// Convert handoff policy decision to one-line reason text.
pub fn describe_handoff_policy_decision(decision: &HandoffCapabilityPolicyDecision) -> String {
    match decision {
        HandoffCapabilityPolicyDecision::Allowed => "allowed".to_string(),
        HandoffCapabilityPolicyDecision::DeniedInvalidTaskEnvelope { reason } => {
            format!("invalid envelope: {}", reason)
        }
        HandoffCapabilityPolicyDecision::DeniedUnknownAgent { agent_id } => {
            format!("unknown agent '{}'", agent_id)
        }
        HandoffCapabilityPolicyDecision::DeniedSenderNotDelegatedOrchestrator {
            sender_agent_id,
            delegated_orchestrator_agent,
        } => format!(
            "sender '{}' is not delegated orchestrator '{}'",
            sender_agent_id, delegated_orchestrator_agent
        ),
        HandoffCapabilityPolicyDecision::DeniedCapability { capability, reason } => {
            format!("capability '{}' denied: {}", capability, reason)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_worker() -> Config {
        let mut config = Config::default();
        config.capability_governance.mode = "delegated".to_string();
        config.capability_governance.delegated_orchestrator_agent = Some("main".to_string());

        let mut worker = config.agents.get("main").expect("main").clone();
        worker.default = false;
        config.agents.insert("worker".to_string(), worker);
        config
    }

    #[test]
    fn parse_assign_command_parses_minimal_shape() {
        let parsed =
            parse_assign_command("/assign worker tool:read_file do a quick check").expect("parsed");
        assert_eq!(parsed.dependent_agent_id, "worker");
        assert_eq!(parsed.requested_capabilities, vec!["tool:read_file"]);
        assert!(parsed.objective.contains("quick check"));
    }

    #[test]
    fn parse_assign_command_rejects_invalid_shapes() {
        assert!(parse_assign_command("/assign").is_none());
        assert!(parse_assign_command("/assign worker").is_none());
        assert!(parse_assign_command("/assign worker ").is_none());
    }

    #[test]
    fn parse_unassign_command_parses_minimal_shape() {
        let parsed = parse_unassign_command("/unassign h-1").expect("parsed");
        assert_eq!(parsed, "h-1");
    }

    #[test]
    fn parse_unassign_command_rejects_invalid_shapes() {
        assert!(parse_unassign_command("/unassign").is_none());
        assert!(parse_unassign_command("/unassign   ").is_none());
        assert!(parse_unassign_command("/unassign a b").is_none());
    }

    #[test]
    fn build_assignment_envelope_allows_bounded_request() {
        let config = config_with_worker();
        let command = ParsedAssignCommand {
            dependent_agent_id: "worker".to_string(),
            requested_capabilities: vec!["tool:read_file".to_string()],
            objective: "Inspect one file".to_string(),
        };
        let envelope =
            build_assignment_envelope(&config, "flow-1", "main", &command).expect("allowed");
        assert_eq!(envelope.from_agent_id, "main");
        assert_eq!(envelope.to_agent_id, "worker");
    }

    #[test]
    fn build_assignment_envelope_denies_non_orchestrator_caller() {
        let config = config_with_worker();
        let command = ParsedAssignCommand {
            dependent_agent_id: "worker".to_string(),
            requested_capabilities: vec!["tool:read_file".to_string()],
            objective: "Inspect one file".to_string(),
        };
        let err = build_assignment_envelope(&config, "flow-1", "worker", &command)
            .expect_err("must deny");
        assert!(err.to_string().contains("not delegated orchestrator"));
    }

    #[test]
    fn ensure_delegated_orchestrator_denies_when_mode_is_user() {
        let config = Config::default();
        let err = ensure_delegated_orchestrator(&config, "main").expect_err("must deny");
        assert!(err.to_string().contains("mode must be 'delegated'"));
    }

    #[test]
    fn build_assignment_envelope_denies_capability_outside_bounds() {
        let mut config = config_with_worker();
        let worker = config.agents.get_mut("worker").expect("worker");
        worker.kit.deny = vec!["shell".to_string()];

        let command = ParsedAssignCommand {
            dependent_agent_id: "worker".to_string(),
            requested_capabilities: vec!["tool:shell".to_string()],
            objective: "Run shell".to_string(),
        };
        let err =
            build_assignment_envelope(&config, "flow-1", "main", &command).expect_err("denied");
        assert!(err
            .to_string()
            .contains("assignment denied by user-boundary policy"));
    }
}
