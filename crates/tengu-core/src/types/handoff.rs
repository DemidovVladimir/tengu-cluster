//! Inter-agent handoff contracts for orchestrator/dependent coordination.
//!
//! Potential use case:
//! The orchestrator dispatches a typed task envelope to a dependent agent and
//! receives a typed result envelope that can be validated and audited.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Current schema version for handoff envelopes.
pub const HANDOFF_SCHEMA_VERSION: u16 = 1;

/// Typed task envelope sent from one agent to another.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandoffTaskEnvelope {
    /// Envelope schema version for compatibility checks.
    pub schema_version: u16,
    /// Stable unique handoff identifier.
    pub handoff_id: String,
    /// Flow key used to correlate multi-agent work under one user request.
    pub flow_key: String,
    /// Sender agent id (typically orchestrator).
    pub from_agent_id: String,
    /// Receiver agent id (typically dependent specialist).
    pub to_agent_id: String,
    /// Human-readable task objective.
    pub objective: String,
    /// Optional constraints the receiver must respect.
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Requested capabilities/tools for this handoff.
    #[serde(default)]
    pub requested_capabilities: Vec<String>,
    /// Optional compact context shared with the receiver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_summary: Option<String>,
    /// Hard output cap for result payload.
    pub max_output_tokens: u32,
    /// Optional TTL in seconds for task validity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u32>,
    /// Optional metadata for implementation-specific routing/trace fields.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

impl HandoffTaskEnvelope {
    /// Validate structural and policy-relevant invariants for task envelopes.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != HANDOFF_SCHEMA_VERSION {
            return Err(anyhow!(
                "unsupported handoff task schema_version={} (expected {})",
                self.schema_version,
                HANDOFF_SCHEMA_VERSION
            ));
        }
        if self.handoff_id.trim().is_empty() {
            return Err(anyhow!("handoff_id cannot be empty"));
        }
        if self.flow_key.trim().is_empty() {
            return Err(anyhow!("flow_key cannot be empty"));
        }
        if self.from_agent_id.trim().is_empty() || self.to_agent_id.trim().is_empty() {
            return Err(anyhow!("from_agent_id/to_agent_id cannot be empty"));
        }
        if self.from_agent_id.trim() == self.to_agent_id.trim() {
            return Err(anyhow!("from_agent_id and to_agent_id must differ"));
        }
        if self.objective.trim().is_empty() {
            return Err(anyhow!("objective cannot be empty"));
        }
        if self.max_output_tokens == 0 {
            return Err(anyhow!("max_output_tokens must be greater than 0"));
        }
        if matches!(self.ttl_seconds, Some(0)) {
            return Err(anyhow!("ttl_seconds must be greater than 0 when set"));
        }
        validate_nonempty_unique_entries("requested_capabilities", &self.requested_capabilities)?;
        validate_nonempty_entries("constraints", &self.constraints)?;
        Ok(())
    }
}

/// Result status for a handoff execution.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HandoffResultStatus {
    /// Receiver accepted but has not yet finished execution.
    Accepted,
    /// Receiver completed one run and now waits for orchestrator validation.
    ReviewRequired,
    /// Receiver completed successfully.
    Completed,
    /// Receiver failed while executing the task.
    Failed,
    /// Receiver denied the task due to policy/capability/runtime limits.
    Denied,
}

impl HandoffResultStatus {
    /// Whether status represents a terminal handoff state.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Denied)
    }
}

/// Validation decision emitted by orchestrator/validator for one dependent result.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum HandoffValidationDecision {
    /// Accept dependent output as reusable downstream artifact.
    Accept,
    /// Request dependent to rerun same objective.
    Retry,
    /// Request dependent to rerun with objective adjustments.
    Rework,
    /// Reject dependent output and mark handoff failed.
    Fail,
}

impl HandoffValidationDecision {
    /// Parse command token into typed validation decision.
    pub fn parse(input: &str) -> Option<Self> {
        match input.trim() {
            "accept" => Some(Self::Accept),
            "retry" => Some(Self::Retry),
            "rework" => Some(Self::Rework),
            "fail" => Some(Self::Fail),
            _ => None,
        }
    }

    /// Canonical kebab-case representation used in metadata/audit strings.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Retry => "retry",
            Self::Rework => "rework",
            Self::Fail => "fail",
        }
    }

    /// Whether this decision requests one more dependent execution cycle.
    pub fn requires_redispatch(self) -> bool {
        matches!(self, Self::Retry | Self::Rework)
    }
}

/// Optional artifact reference returned by a dependent agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandoffArtifactRef {
    /// Logical artifact id/name.
    pub id: String,
    /// Artifact path/reference.
    pub path: String,
    /// Optional descriptive label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Typed result envelope returned by receiver back to sender.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HandoffResultEnvelope {
    /// Envelope schema version for compatibility checks.
    pub schema_version: u16,
    /// Stable handoff identifier copied from task envelope.
    pub handoff_id: String,
    /// Flow key copied from task envelope.
    pub flow_key: String,
    /// Result sender agent id (typically dependent specialist).
    pub from_agent_id: String,
    /// Result receiver agent id (typically orchestrator).
    pub to_agent_id: String,
    /// Execution status.
    pub status: HandoffResultStatus,
    /// Human-readable summary/result payload.
    pub summary: String,
    /// Optional structured artifact references.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<HandoffArtifactRef>,
    /// Approximate output token count for budgeting checks.
    pub output_tokens: u32,
    /// Optional failure/denial reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_reason: Option<String>,
    /// Optional metadata for implementation-specific trace fields.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub metadata: HashMap<String, String>,
}

impl HandoffResultEnvelope {
    /// Validate result envelope and consistency against source task envelope.
    pub fn validate_against(&self, task: &HandoffTaskEnvelope) -> Result<()> {
        if self.schema_version != HANDOFF_SCHEMA_VERSION {
            return Err(anyhow!(
                "unsupported handoff result schema_version={} (expected {})",
                self.schema_version,
                HANDOFF_SCHEMA_VERSION
            ));
        }
        if self.handoff_id.trim() != task.handoff_id.trim() {
            return Err(anyhow!("handoff_id mismatch between task and result"));
        }
        if self.flow_key.trim() != task.flow_key.trim() {
            return Err(anyhow!("flow_key mismatch between task and result"));
        }
        if self.from_agent_id.trim() != task.to_agent_id.trim()
            || self.to_agent_id.trim() != task.from_agent_id.trim()
        {
            return Err(anyhow!(
                "result direction mismatch (expected {} -> {})",
                task.to_agent_id,
                task.from_agent_id
            ));
        }
        if self.output_tokens > task.max_output_tokens {
            return Err(anyhow!(
                "result output_tokens={} exceeds task max_output_tokens={}",
                self.output_tokens,
                task.max_output_tokens
            ));
        }
        if matches!(
            self.status,
            HandoffResultStatus::Completed | HandoffResultStatus::ReviewRequired
        ) && self.summary.trim().is_empty()
        {
            return Err(anyhow!(
                "completed/review-required handoff result requires non-empty summary"
            ));
        }
        if matches!(
            self.status,
            HandoffResultStatus::Failed | HandoffResultStatus::Denied
        ) && self
            .error_reason
            .as_deref()
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            return Err(anyhow!(
                "failed/denied handoff result requires non-empty error_reason"
            ));
        }
        validate_artifacts(&self.artifacts)?;
        Ok(())
    }
}

fn validate_nonempty_entries(path: &str, entries: &[String]) -> Result<()> {
    for (idx, entry) in entries.iter().enumerate() {
        if entry.trim().is_empty() {
            return Err(anyhow!("{path}[{idx}] cannot be empty"));
        }
    }
    Ok(())
}

fn validate_nonempty_unique_entries(path: &str, entries: &[String]) -> Result<()> {
    let mut seen = HashSet::<String>::new();
    for (idx, entry) in entries.iter().enumerate() {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            return Err(anyhow!("{path}[{idx}] cannot be empty"));
        }
        if !seen.insert(trimmed.to_string()) {
            return Err(anyhow!("{path} contains duplicate '{}'", trimmed));
        }
    }
    Ok(())
}

fn validate_artifacts(entries: &[HandoffArtifactRef]) -> Result<()> {
    for (idx, artifact) in entries.iter().enumerate() {
        if artifact.id.trim().is_empty() {
            return Err(anyhow!("artifacts[{idx}].id cannot be empty"));
        }
        if artifact.path.trim().is_empty() {
            return Err(anyhow!("artifacts[{idx}].path cannot be empty"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> HandoffTaskEnvelope {
        HandoffTaskEnvelope {
            schema_version: HANDOFF_SCHEMA_VERSION,
            handoff_id: "handoff-1".to_string(),
            flow_key: "flow-1".to_string(),
            from_agent_id: "orchestrator".to_string(),
            to_agent_id: "engineering".to_string(),
            objective: "Design architecture options".to_string(),
            constraints: vec!["keep cost under budget".to_string()],
            requested_capabilities: vec!["read_file".to_string()],
            context_summary: Some("Current repo has event bus baseline".to_string()),
            max_output_tokens: 512,
            ttl_seconds: Some(300),
            metadata: HashMap::new(),
        }
    }

    fn completed_result() -> HandoffResultEnvelope {
        HandoffResultEnvelope {
            schema_version: HANDOFF_SCHEMA_VERSION,
            handoff_id: "handoff-1".to_string(),
            flow_key: "flow-1".to_string(),
            from_agent_id: "engineering".to_string(),
            to_agent_id: "orchestrator".to_string(),
            status: HandoffResultStatus::Completed,
            summary: "Provided two architecture options".to_string(),
            artifacts: vec![HandoffArtifactRef {
                id: "arch-draft".to_string(),
                path: "docs/architecture_draft.md".to_string(),
                label: None,
            }],
            output_tokens: 320,
            error_reason: None,
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn validation_decision_parser_supports_all_actions() {
        assert_eq!(
            HandoffValidationDecision::parse("accept"),
            Some(HandoffValidationDecision::Accept)
        );
        assert_eq!(
            HandoffValidationDecision::parse("retry"),
            Some(HandoffValidationDecision::Retry)
        );
        assert_eq!(
            HandoffValidationDecision::parse("rework"),
            Some(HandoffValidationDecision::Rework)
        );
        assert_eq!(
            HandoffValidationDecision::parse("fail"),
            Some(HandoffValidationDecision::Fail)
        );
        assert!(HandoffValidationDecision::parse("unknown").is_none());
    }

    #[test]
    fn review_required_result_requires_summary() {
        let task = task();
        let mut result = completed_result();
        result.status = HandoffResultStatus::ReviewRequired;
        result.summary.clear();
        let err = result
            .validate_against(&task)
            .expect_err("expected validation error");
        assert!(err.to_string().contains("review-required"));
    }

    #[test]
    fn task_validation_rejects_invalid_sender_receiver() {
        let mut envelope = task();
        envelope.to_agent_id = "orchestrator".to_string();
        let err = envelope.validate().expect_err("expected validation error");
        assert!(err.to_string().contains("must differ"));
    }

    #[test]
    fn task_validation_rejects_duplicate_capabilities() {
        let mut envelope = task();
        envelope.requested_capabilities = vec!["read_file".to_string(), "read_file".to_string()];
        let err = envelope.validate().expect_err("expected validation error");
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn result_validation_rejects_direction_mismatch() {
        let task = task();
        let mut result = completed_result();
        result.from_agent_id = "marketing".to_string();
        let err = result
            .validate_against(&task)
            .expect_err("expected validation error");
        assert!(err.to_string().contains("direction mismatch"));
    }

    #[test]
    fn result_validation_rejects_token_overflow() {
        let task = task();
        let mut result = completed_result();
        result.output_tokens = 2_048;
        let err = result
            .validate_against(&task)
            .expect_err("expected validation error");
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn result_validation_accepts_completed_payload() {
        let task = task();
        let result = completed_result();
        result
            .validate_against(&task)
            .expect("result envelope should be valid");
    }

    #[test]
    fn failed_result_requires_error_reason() {
        let task = task();
        let mut result = completed_result();
        result.status = HandoffResultStatus::Failed;
        result.summary.clear();
        result.error_reason = None;
        let err = result
            .validate_against(&task)
            .expect_err("expected validation error");
        assert!(err.to_string().contains("error_reason"));
    }
}
