//! Automated handoff validation runner for delegated review-gate decisions.
//!
//! Potential use case:
//! Evaluate one `review_required` dependent result with bounded checks and
//! produce an orchestrator action (`accept`/`retry`/`fail`) without manual
//! decisioning each time.

use crate::control_plane::CapabilityAssignmentRecord;
use tengu_core::config::{evaluate_handoff_capability_policy, Config};
use tengu_core::types::{HandoffTaskEnvelope, HANDOFF_SCHEMA_VERSION};
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const VALIDATION_MAX_RETRIES_DEFAULT: u32 = 2;
const VALIDATION_COMMAND_TIMEOUT_SECS_DEFAULT: u64 = 45;

/// Automated validator action for one delegated handoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AutoValidationAction {
    /// Validation checks passed and handoff can be finalized.
    Accept { report: String },
    /// Validation checks failed but retry budget remains.
    Retry { report: String },
    /// Validation checks failed and retry budget is exhausted.
    Fail { report: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ValidationCheckOutcome {
    Passed(String),
    Failed(String),
    Skipped(String),
}

/// Run role-aware validation checks and resolve automated gate action.
pub(crate) async fn run_auto_validation_for_assignment(
    config: &Config,
    assignment: &CapabilityAssignmentRecord,
) -> AutoValidationAction {
    let mut outcomes = Vec::<ValidationCheckOutcome>::new();
    let max_retries = resolve_validation_max_retries();

    outcomes.push(run_policy_recheck(config, assignment));

    if should_run_rust_workspace_checks(assignment) {
        let dependent_workspace = config
            .agents
            .get(assignment.dependent_agent_id.as_str())
            .and_then(|agent| agent.workspace.clone());
        outcomes.extend(run_rust_workspace_checks(dependent_workspace).await);
    } else {
        outcomes.push(ValidationCheckOutcome::Skipped(
            "workspace checks skipped: role/capability profile does not require code validation"
                .to_string(),
        ));
    }

    let has_failures = outcomes
        .iter()
        .any(|entry| matches!(entry, ValidationCheckOutcome::Failed(_)));
    let report = render_validation_report(assignment, max_retries, &outcomes);

    if !has_failures {
        return AutoValidationAction::Accept { report };
    }
    if assignment.validation_attempts < max_retries {
        AutoValidationAction::Retry { report }
    } else {
        AutoValidationAction::Fail { report }
    }
}

/// Decide whether this assignment needs Rust workspace checks.
///
/// Current heuristic is role/capability based:
/// - dependent id hints code roles (`software`, `backend`, `engineering`, `rust`)
/// - or assignment requests mutating/shell tool capabilities
fn should_run_rust_workspace_checks(assignment: &CapabilityAssignmentRecord) -> bool {
    let dependent = assignment.dependent_agent_id.to_ascii_lowercase();
    let role_hint = ["software", "backend", "engineering", "rust"]
        .iter()
        .any(|needle| dependent.contains(needle));
    let capability_hint = assignment.requested_capabilities.iter().any(|capability| {
        matches!(
            capability.trim(),
            "tool:write_file" | "tool:edit_file" | "tool:shell"
        )
    });
    role_hint || capability_hint
}

/// Re-run handoff policy checks against current config bounds.
fn run_policy_recheck(
    config: &Config,
    assignment: &CapabilityAssignmentRecord,
) -> ValidationCheckOutcome {
    let envelope = HandoffTaskEnvelope {
        schema_version: HANDOFF_SCHEMA_VERSION,
        handoff_id: assignment.handoff_id.clone(),
        flow_key: assignment.flow_key.clone(),
        from_agent_id: assignment.orchestrator_agent_id.clone(),
        to_agent_id: assignment.dependent_agent_id.clone(),
        objective: assignment.objective.clone(),
        constraints: vec![
            "bounded-by-user-policy".to_string(),
            "validation-gate-policy-recheck".to_string(),
        ],
        requested_capabilities: assignment.requested_capabilities.clone(),
        context_summary: None,
        max_output_tokens: 256,
        ttl_seconds: Some(900),
        metadata: std::collections::HashMap::from([("handoff_depth".to_string(), "1".to_string())]),
    };
    let decision = evaluate_handoff_capability_policy(config, &envelope);
    if decision.is_allowed() {
        ValidationCheckOutcome::Passed("policy recheck passed".to_string())
    } else {
        ValidationCheckOutcome::Failed(format!("policy recheck failed: {:?}", decision))
    }
}

/// Run Rust-centric workspace checks when applicable.
async fn run_rust_workspace_checks(
    workspace: Option<std::path::PathBuf>,
) -> Vec<ValidationCheckOutcome> {
    let Some(workspace) = workspace else {
        return vec![ValidationCheckOutcome::Skipped(
            "workspace checks skipped: dependent workspace is not configured".to_string(),
        )];
    };
    if !workspace.exists() || !workspace.is_dir() {
        return vec![ValidationCheckOutcome::Failed(format!(
            "workspace checks failed: '{}' does not exist or is not a directory",
            workspace.display()
        ))];
    }
    if !workspace.join("Cargo.toml").exists() {
        return vec![ValidationCheckOutcome::Skipped(format!(
            "workspace checks skipped: no Cargo.toml in '{}'",
            workspace.display()
        ))];
    }

    let mut results = Vec::new();
    results.push(
        run_command_check(
            &workspace,
            "cargo fmt --all --check",
            &["fmt", "--all", "--check"],
        )
        .await,
    );
    results.push(run_command_check(&workspace, "cargo test -q", &["test", "-q"]).await);
    results
}

/// Execute one bounded command check and collect compact diagnostic output.
async fn run_command_check(
    workspace: &std::path::Path,
    label: &str,
    args: &[&str],
) -> ValidationCheckOutcome {
    let timeout_secs = resolve_validation_command_timeout_secs();
    let mut cmd = Command::new("cargo");
    cmd.current_dir(workspace).args(args);

    let output = match timeout(Duration::from_secs(timeout_secs), cmd.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => {
            return ValidationCheckOutcome::Failed(format!(
                "{label} failed to start in '{}': {}",
                workspace.display(),
                err
            ));
        }
        Err(_) => {
            return ValidationCheckOutcome::Failed(format!(
                "{label} timed out after {}s in '{}'",
                timeout_secs,
                workspace.display()
            ));
        }
    };

    if output.status.success() {
        return ValidationCheckOutcome::Passed(format!("{label} passed"));
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = first_nonempty_line(&stderr)
        .or_else(|| first_nonempty_line(&stdout))
        .unwrap_or("no error output".to_string());
    ValidationCheckOutcome::Failed(format!("{label} failed: {}", detail))
}

/// Render compact line-by-line validation report for audit/user feedback.
fn render_validation_report(
    assignment: &CapabilityAssignmentRecord,
    max_retries: u32,
    outcomes: &[ValidationCheckOutcome],
) -> String {
    let mut lines = Vec::<String>::new();
    lines.push(format!(
        "handoff={} dependent={} attempt={}/{}",
        assignment.handoff_id,
        assignment.dependent_agent_id,
        assignment.validation_attempts,
        max_retries
    ));
    for outcome in outcomes {
        match outcome {
            ValidationCheckOutcome::Passed(msg) => lines.push(format!("PASS: {}", msg)),
            ValidationCheckOutcome::Failed(msg) => lines.push(format!("FAIL: {}", msg)),
            ValidationCheckOutcome::Skipped(msg) => lines.push(format!("SKIP: {}", msg)),
        }
    }
    lines.join(" | ")
}

/// Return first non-empty trimmed line from command output.
fn first_nonempty_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(ToOwned::to_owned)
}

/// Resolve max automatic validation retries from env.
fn resolve_validation_max_retries() -> u32 {
    std::env::var("TENGU_HANDOFF_VALIDATION_MAX_RETRIES")
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(VALIDATION_MAX_RETRIES_DEFAULT)
}

/// Resolve command timeout used by validator runner.
fn resolve_validation_command_timeout_secs() -> u64 {
    std::env::var("TENGU_HANDOFF_VALIDATION_CMD_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(VALIDATION_COMMAND_TIMEOUT_SECS_DEFAULT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_detection_enables_rust_checks_for_engineering_like_roles() {
        let assignment = CapabilityAssignmentRecord {
            handoff_id: "h-1".to_string(),
            flow_key: "flow-1".to_string(),
            orchestrator_agent_id: "orchestrator".to_string(),
            dependent_agent_id: "software_engineering".to_string(),
            requested_capabilities: vec!["tool:read_file".to_string()],
            objective: "inspect".to_string(),
            issued_at_epoch_ms: 1,
            validation_attempts: 1,
        };
        assert!(should_run_rust_workspace_checks(&assignment));
    }

    #[test]
    fn role_detection_enables_rust_checks_for_mutating_tool_caps() {
        let assignment = CapabilityAssignmentRecord {
            handoff_id: "h-1".to_string(),
            flow_key: "flow-1".to_string(),
            orchestrator_agent_id: "orchestrator".to_string(),
            dependent_agent_id: "content".to_string(),
            requested_capabilities: vec!["tool:write_file".to_string()],
            objective: "write".to_string(),
            issued_at_epoch_ms: 1,
            validation_attempts: 1,
        };
        assert!(should_run_rust_workspace_checks(&assignment));
    }

    #[test]
    fn report_render_contains_attempt_and_check_lines() {
        let assignment = CapabilityAssignmentRecord {
            handoff_id: "h-1".to_string(),
            flow_key: "flow-1".to_string(),
            orchestrator_agent_id: "orchestrator".to_string(),
            dependent_agent_id: "backend".to_string(),
            requested_capabilities: vec!["tool:read_file".to_string()],
            objective: "inspect".to_string(),
            issued_at_epoch_ms: 1,
            validation_attempts: 2,
        };
        let report = render_validation_report(
            &assignment,
            3,
            &[
                ValidationCheckOutcome::Passed("policy".to_string()),
                ValidationCheckOutcome::Skipped("no workspace".to_string()),
            ],
        );
        assert!(report.contains("attempt=2/3"));
        assert!(report.contains("PASS: policy"));
        assert!(report.contains("SKIP: no workspace"));
    }
}
