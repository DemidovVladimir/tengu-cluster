//! Runtime capability policy helpers derived from validated config.
//!
//! Potential use case:
//! Apply one shared policy contract in runtime paths (engine/tool execution)
//! instead of duplicating allow/deny logic at each call site.

use anyhow::Result;

use super::AgentConfig;

/// Decision emitted by engine allowlist policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnginePolicyDecision {
    /// Selected `engine/model` is allowed.
    Allowed,
    /// Selected `engine/model` is not listed in `allowed_engines`.
    DeniedNotAllowListed { selected: String },
}

impl EnginePolicyDecision {
    /// Whether the evaluated engine selection is allowed.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// Decision emitted by per-agent tool policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolPolicyDecision {
    /// Tool is allowed to run.
    Allowed,
    /// Tool name is empty after trim.
    DeniedEmptyName,
    /// Tool appears in deny list.
    DeniedByDenyList,
    /// Allow list is non-empty and tool is not included.
    DeniedNotAllowListed,
}

impl ToolPolicyDecision {
    /// Whether the evaluated tool call is allowed.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    /// Human-readable policy reason used in runtime diagnostics.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::DeniedEmptyName => "empty tool name",
            Self::DeniedByDenyList => "blocked by deny list",
            Self::DeniedNotAllowListed => "not listed in allow list",
        }
    }
}

/// Build normalized `provider/model` identifier for policy checks.
pub fn engine_key(engine: &str, model: &str) -> String {
    format!("{}/{}", engine.trim(), model.trim())
}

/// Evaluate whether selected agent engine/model passes `allowed_engines`.
///
/// Policy contract:
/// - Empty `allowed_engines` means "allow all engines".
/// - Non-empty `allowed_engines` means selected `engine/model` must match one entry exactly.
pub fn evaluate_engine_policy(agent: &AgentConfig) -> EnginePolicyDecision {
    if agent.allowed_engines.is_empty() {
        return EnginePolicyDecision::Allowed;
    }
    let selected = engine_key(&agent.engine, &agent.model);
    if agent
        .allowed_engines
        .iter()
        .any(|value| value.trim() == selected)
    {
        EnginePolicyDecision::Allowed
    } else {
        EnginePolicyDecision::DeniedNotAllowListed { selected }
    }
}

/// Enforce engine policy and return actionable runtime error on violation.
pub fn ensure_engine_allowed(agent_id: &str, agent: &AgentConfig) -> Result<()> {
    match evaluate_engine_policy(agent) {
        EnginePolicyDecision::Allowed => Ok(()),
        EnginePolicyDecision::DeniedNotAllowListed { selected } => Err(anyhow::anyhow!(
            "agents.{agent_id} selected engine/model '{selected}' is denied by runtime policy (allowed_engines)"
        )),
    }
}

/// Evaluate one tool call name against per-agent `kit.allow` / `kit.deny`.
///
/// Policy contract:
/// - `deny` always wins over `allow`.
/// - Empty `allow` means "all tools allowed unless denied".
/// - Non-empty `allow` means tool must be explicitly listed.
pub fn evaluate_tool_policy(agent: &AgentConfig, tool_name: &str) -> ToolPolicyDecision {
    let candidate = tool_name.trim();
    if candidate.is_empty() {
        return ToolPolicyDecision::DeniedEmptyName;
    }

    if agent.kit.deny.iter().any(|value| value.trim() == candidate) {
        return ToolPolicyDecision::DeniedByDenyList;
    }

    if agent.kit.allow.is_empty()
        || agent
            .kit
            .allow
            .iter()
            .any(|value| value.trim() == candidate)
    {
        ToolPolicyDecision::Allowed
    } else {
        ToolPolicyDecision::DeniedNotAllowListed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, Config};

    fn agent() -> AgentConfig {
        Config::default().agents.get("main").expect("main").clone()
    }

    #[test]
    fn engine_policy_allows_any_when_allowlist_empty() {
        let mut agent = agent();
        agent.allowed_engines.clear();
        assert!(evaluate_engine_policy(&agent).is_allowed());
    }

    #[test]
    fn engine_policy_denies_non_allowlisted_selection() {
        let mut agent = agent();
        agent.allowed_engines = vec!["openai/gpt-4o-mini".to_string()];
        let decision = evaluate_engine_policy(&agent);
        assert!(matches!(
            decision,
            EnginePolicyDecision::DeniedNotAllowListed { .. }
        ));
    }

    #[test]
    fn tool_policy_deny_has_precedence() {
        let mut agent = agent();
        agent.kit.allow = vec!["shell".to_string()];
        agent.kit.deny = vec!["shell".to_string()];
        assert_eq!(
            evaluate_tool_policy(&agent, "shell"),
            ToolPolicyDecision::DeniedByDenyList
        );
    }

    #[test]
    fn tool_policy_requires_allowlist_match_when_present() {
        let mut agent = agent();
        agent.kit.allow = vec!["read_file".to_string()];
        agent.kit.deny.clear();
        assert_eq!(
            evaluate_tool_policy(&agent, "write_file"),
            ToolPolicyDecision::DeniedNotAllowListed
        );
        assert_eq!(
            evaluate_tool_policy(&agent, "read_file"),
            ToolPolicyDecision::Allowed
        );
    }
}
