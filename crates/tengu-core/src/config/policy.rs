//! Runtime capability policy helpers derived from validated config.
//!
//! Potential use case:
//! Apply one shared policy contract in runtime paths (engine/tool execution)
//! instead of duplicating allow/deny logic at each call site.

use anyhow::Result;

use super::{AgentConfig, Config};
use crate::types::HandoffTaskEnvelope;

/// Decision emitted by actor-level capability governance checks.
///
/// This gate is used at runtime entry points where an agent requests direct
/// capability usage (for example tool execution from an engine turn) without
/// an explicit inter-agent handoff envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapabilityGovernanceActorDecision {
    /// Runtime uses direct user-defined agent policies (`mode=user`).
    AllowedUserMode,
    /// Runtime uses delegated mode and actor matches configured orchestrator.
    AllowedDelegatedOrchestrator { orchestrator_agent_id: String },
    /// Runtime uses delegated mode and actor is not orchestrator.
    DeniedDelegatedActor {
        actor_agent_id: String,
        delegated_orchestrator_agent: String,
    },
    /// Runtime actor id is missing or not configured.
    DeniedUnknownActor { actor_agent_id: String },
}

impl CapabilityGovernanceActorDecision {
    /// Whether capability requests from this actor are allowed at runtime.
    pub fn is_allowed(&self) -> bool {
        matches!(
            self,
            Self::AllowedUserMode | Self::AllowedDelegatedOrchestrator { .. }
        )
    }

    /// Human-readable reason used by runtime diagnostics/errors.
    pub fn reason(&self) -> String {
        match self {
            Self::AllowedUserMode => "capability governance mode=user".to_string(),
            Self::AllowedDelegatedOrchestrator {
                orchestrator_agent_id,
            } => format!(
                "capability governance mode=delegated (actor matches orchestrator '{}')",
                orchestrator_agent_id
            ),
            Self::DeniedDelegatedActor {
                actor_agent_id,
                delegated_orchestrator_agent,
            } => format!(
                "actor '{}' is not delegated orchestrator '{}'",
                actor_agent_id, delegated_orchestrator_agent
            ),
            Self::DeniedUnknownActor { actor_agent_id } => {
                format!("unknown runtime actor '{}'", actor_agent_id)
            }
        }
    }
}

/// Evaluate whether `actor_agent_id` may request direct runtime capabilities.
///
/// Policy contract:
/// - `mode=user`: any configured agent may use its own user-defined bounds.
/// - `mode=delegated`: only `delegated_orchestrator_agent` may issue direct requests.
pub fn evaluate_capability_governance_actor(
    config: &Config,
    actor_agent_id: &str,
) -> CapabilityGovernanceActorDecision {
    let actor = actor_agent_id.trim();
    if actor.is_empty() || !config.agents.contains_key(actor) {
        return CapabilityGovernanceActorDecision::DeniedUnknownActor {
            actor_agent_id: actor.to_string(),
        };
    }

    if config.capability_governance.mode.trim() != "delegated" {
        return CapabilityGovernanceActorDecision::AllowedUserMode;
    }

    let delegated = config
        .capability_governance
        .delegated_orchestrator_agent
        .as_deref()
        .map(str::trim)
        .unwrap_or_default();
    if actor == delegated {
        CapabilityGovernanceActorDecision::AllowedDelegatedOrchestrator {
            orchestrator_agent_id: delegated.to_string(),
        }
    } else {
        CapabilityGovernanceActorDecision::DeniedDelegatedActor {
            actor_agent_id: actor.to_string(),
            delegated_orchestrator_agent: delegated.to_string(),
        }
    }
}

/// Enforce actor-level capability governance and return actionable error text.
pub fn ensure_capability_governance_actor_allowed(
    config: &Config,
    actor_agent_id: &str,
) -> Result<()> {
    let decision = evaluate_capability_governance_actor(config, actor_agent_id);
    if decision.is_allowed() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "runtime capability governance denied actor '{}': {}",
            actor_agent_id,
            decision.reason()
        ))
    }
}

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

/// Decision emitted by per-agent skill policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillPolicyDecision {
    /// Skill is allowed to run.
    Allowed,
    /// Skill name is empty after trim.
    DeniedEmptyName,
    /// Skill appears in deny list.
    DeniedByDenyList,
    /// Allow list is non-empty and skill is not included.
    DeniedNotAllowListed,
}

impl SkillPolicyDecision {
    /// Whether the evaluated skill usage is allowed.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    /// Human-readable policy reason used in runtime diagnostics.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::DeniedEmptyName => "empty skill name",
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

/// Evaluate one skill identifier against per-agent `skill_policy.allow` / `skill_policy.deny`.
///
/// Policy contract:
/// - `deny` always wins over `allow`.
/// - Empty `allow` means "all skills allowed unless denied".
/// - Non-empty `allow` means skill must be explicitly listed.
pub fn evaluate_skill_policy(agent: &AgentConfig, skill_name: &str) -> SkillPolicyDecision {
    let candidate = skill_name.trim();
    if candidate.is_empty() {
        return SkillPolicyDecision::DeniedEmptyName;
    }

    if agent
        .skill_policy
        .deny
        .iter()
        .any(|value| value.trim() == candidate)
    {
        return SkillPolicyDecision::DeniedByDenyList;
    }

    if agent.skill_policy.allow.is_empty()
        || agent
            .skill_policy
            .allow
            .iter()
            .any(|value| value.trim() == candidate)
    {
        SkillPolicyDecision::Allowed
    } else {
        SkillPolicyDecision::DeniedNotAllowListed
    }
}

/// Decision emitted by per-agent tool approval evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolApprovalDecision {
    /// Approval is not required for this tool.
    AllowedNotRequired,
    /// Approval is required and present in `kit.approved`.
    AllowedByPreApproval,
    /// Approval is required but missing from `kit.approved`.
    DeniedNotApproved,
}

impl ToolApprovalDecision {
    /// Whether tool execution is approved.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::AllowedNotRequired | Self::AllowedByPreApproval)
    }

    /// Human-readable approval reason used in runtime diagnostics.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::AllowedNotRequired => "approval not required",
            Self::AllowedByPreApproval => "approved by kit.approved",
            Self::DeniedNotApproved => "tool requires explicit approval",
        }
    }
}

/// Evaluate whether a tool call passes approval requirements.
///
/// Approval is required when either:
/// - per-tool metadata marks `requires_approval`, or
/// - tool is listed in `agent.kit.approval_required`.
///
/// When approval is required, the tool must be listed in `agent.kit.approved`.
pub fn evaluate_tool_approval_policy(
    agent: &AgentConfig,
    tool_name: &str,
    metadata_requires_approval: bool,
) -> ToolApprovalDecision {
    let candidate = tool_name.trim();
    let requires_approval = metadata_requires_approval
        || agent
            .kit
            .approval_required
            .iter()
            .any(|value| value.trim() == candidate);
    if !requires_approval {
        return ToolApprovalDecision::AllowedNotRequired;
    }
    if agent
        .kit
        .approved
        .iter()
        .any(|value| value.trim() == candidate)
    {
        ToolApprovalDecision::AllowedByPreApproval
    } else {
        ToolApprovalDecision::DeniedNotApproved
    }
}

/// Decision emitted by inter-agent handoff capability policy checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffCapabilityPolicyDecision {
    /// Requested handoff capabilities are allowed.
    Allowed,
    /// Task envelope failed structural validation.
    DeniedInvalidTaskEnvelope { reason: String },
    /// Sender or receiver agent is unknown to current config.
    DeniedUnknownAgent { agent_id: String },
    /// Delegated mode requires one orchestrator; sender is not that orchestrator.
    DeniedSenderNotDelegatedOrchestrator {
        sender_agent_id: String,
        delegated_orchestrator_agent: String,
    },
    /// Capability request is denied by target-agent policy bounds.
    DeniedCapability { capability: String, reason: String },
}

impl HandoffCapabilityPolicyDecision {
    /// Whether requested handoff capabilities are allowed.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// Enforce handoff capability requests against user/delegated governance mode.
///
/// Supported capability identifiers:
/// - `tool:<name>`
/// - `skill:<name>`
/// - `engine:<provider/model>`
pub fn evaluate_handoff_capability_policy(
    config: &Config,
    task: &HandoffTaskEnvelope,
) -> HandoffCapabilityPolicyDecision {
    if let Err(err) = task.validate() {
        return HandoffCapabilityPolicyDecision::DeniedInvalidTaskEnvelope {
            reason: err.to_string(),
        };
    }

    let sender_id = task.from_agent_id.trim();
    let receiver_id = task.to_agent_id.trim();
    if !config.agents.contains_key(sender_id) {
        return HandoffCapabilityPolicyDecision::DeniedUnknownAgent {
            agent_id: sender_id.to_string(),
        };
    }
    let Some(receiver_agent) = config.agents.get(receiver_id) else {
        return HandoffCapabilityPolicyDecision::DeniedUnknownAgent {
            agent_id: receiver_id.to_string(),
        };
    };

    if config.capability_governance.mode.trim() == "delegated" {
        let delegated = config
            .capability_governance
            .delegated_orchestrator_agent
            .as_deref()
            .map(str::trim)
            .unwrap_or_default();
        if sender_id != delegated {
            return HandoffCapabilityPolicyDecision::DeniedSenderNotDelegatedOrchestrator {
                sender_agent_id: sender_id.to_string(),
                delegated_orchestrator_agent: delegated.to_string(),
            };
        }
    }

    for capability in &task.requested_capabilities {
        let capability = capability.trim();
        if capability.is_empty() {
            return HandoffCapabilityPolicyDecision::DeniedCapability {
                capability: capability.to_string(),
                reason: "capability identifier cannot be empty".to_string(),
            };
        }
        let Some((kind, value)) = capability.split_once(':') else {
            return HandoffCapabilityPolicyDecision::DeniedCapability {
                capability: capability.to_string(),
                reason: "invalid capability format (expected kind:value)".to_string(),
            };
        };
        let value = value.trim();
        if value.is_empty() {
            return HandoffCapabilityPolicyDecision::DeniedCapability {
                capability: capability.to_string(),
                reason: "capability value cannot be empty".to_string(),
            };
        }

        match kind.trim() {
            "tool" => {
                let tool_policy = evaluate_tool_policy(receiver_agent, value);
                if !tool_policy.is_allowed() {
                    return HandoffCapabilityPolicyDecision::DeniedCapability {
                        capability: capability.to_string(),
                        reason: format!("tool policy: {}", tool_policy.reason()),
                    };
                }
                let approval_policy = evaluate_tool_approval_policy(receiver_agent, value, false);
                if !approval_policy.is_allowed() {
                    return HandoffCapabilityPolicyDecision::DeniedCapability {
                        capability: capability.to_string(),
                        reason: format!("tool approval: {}", approval_policy.reason()),
                    };
                }
            }
            "skill" => {
                let skill_policy = evaluate_skill_policy(receiver_agent, value);
                if !skill_policy.is_allowed() {
                    return HandoffCapabilityPolicyDecision::DeniedCapability {
                        capability: capability.to_string(),
                        reason: format!("skill policy: {}", skill_policy.reason()),
                    };
                }
            }
            "engine" => {
                if !evaluate_engine_override_policy(receiver_agent, value).is_allowed() {
                    return HandoffCapabilityPolicyDecision::DeniedCapability {
                        capability: capability.to_string(),
                        reason: "engine is not in target allowed_engines".to_string(),
                    };
                }
            }
            other => {
                return HandoffCapabilityPolicyDecision::DeniedCapability {
                    capability: capability.to_string(),
                    reason: format!("unsupported capability kind '{}'", other),
                };
            }
        }
    }

    HandoffCapabilityPolicyDecision::Allowed
}

/// Decision for evaluating one runtime engine override request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineOverridePolicyDecision {
    Allowed,
    DeniedNotAllowListed,
}

impl EngineOverridePolicyDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }
}

/// Evaluate whether `engine_key` is allowed as runtime override for an agent.
///
/// Policy contract:
/// - Empty `allowed_engines` means "all overrides allowed".
/// - Otherwise override must match one explicit `provider/model` entry.
pub fn evaluate_engine_override_policy(
    agent: &AgentConfig,
    engine_key: &str,
) -> EngineOverridePolicyDecision {
    let normalized = engine_key.trim();
    if normalized.is_empty() {
        return EngineOverridePolicyDecision::DeniedNotAllowListed;
    }
    if agent.allowed_engines.is_empty()
        || agent
            .allowed_engines
            .iter()
            .any(|value| value.trim() == normalized)
    {
        EngineOverridePolicyDecision::Allowed
    } else {
        EngineOverridePolicyDecision::DeniedNotAllowListed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, Config};
    use crate::types::{HandoffTaskEnvelope, HANDOFF_SCHEMA_VERSION};
    use std::collections::HashMap;

    fn agent() -> AgentConfig {
        Config::default().agents.get("main").expect("main").clone()
    }

    fn handoff_task(requested_capabilities: Vec<String>) -> HandoffTaskEnvelope {
        HandoffTaskEnvelope {
            schema_version: HANDOFF_SCHEMA_VERSION,
            handoff_id: "handoff-1".to_string(),
            flow_key: "flow-1".to_string(),
            from_agent_id: "main".to_string(),
            to_agent_id: "worker".to_string(),
            objective: "Do a bounded subtask".to_string(),
            constraints: Vec::new(),
            requested_capabilities,
            context_summary: None,
            max_output_tokens: 256,
            ttl_seconds: None,
            metadata: HashMap::new(),
        }
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

    #[test]
    fn tool_approval_policy_denies_when_required_but_not_approved() {
        let mut agent = agent();
        agent.kit.approval_required = vec!["read_file".to_string()];
        agent.kit.approved.clear();
        assert_eq!(
            evaluate_tool_approval_policy(&agent, "read_file", false),
            ToolApprovalDecision::DeniedNotApproved
        );
    }

    #[test]
    fn tool_approval_policy_allows_when_required_and_preapproved() {
        let mut agent = agent();
        agent.kit.approval_required = vec!["read_file".to_string()];
        agent.kit.approved = vec!["read_file".to_string()];
        assert_eq!(
            evaluate_tool_approval_policy(&agent, "read_file", false),
            ToolApprovalDecision::AllowedByPreApproval
        );
    }

    #[test]
    fn tool_approval_policy_uses_tool_metadata_requirement() {
        let mut agent = agent();
        agent.kit.approval_required.clear();
        agent.kit.approved.clear();
        assert_eq!(
            evaluate_tool_approval_policy(&agent, "read_file", true),
            ToolApprovalDecision::DeniedNotApproved
        );
    }

    #[test]
    fn skill_policy_deny_has_precedence() {
        let mut agent = agent();
        agent.skill_policy.allow = vec!["analysis".to_string()];
        agent.skill_policy.deny = vec!["analysis".to_string()];
        assert_eq!(
            evaluate_skill_policy(&agent, "analysis"),
            SkillPolicyDecision::DeniedByDenyList
        );
    }

    #[test]
    fn engine_override_policy_uses_allowlist() {
        let mut agent = agent();
        agent.allowed_engines = vec!["openai/gpt-4o-mini".to_string()];
        assert!(!evaluate_engine_override_policy(&agent, "anthropic/claude-3-7").is_allowed());
        assert!(evaluate_engine_override_policy(&agent, "openai/gpt-4o-mini").is_allowed());
    }

    #[test]
    fn handoff_capability_policy_allows_user_mode_when_bounds_pass() {
        let mut config = Config::default();
        config.agents.insert(
            "worker".to_string(),
            AgentConfig {
                default: false,
                engine: "openai".to_string(),
                model: "gpt-4o-mini".to_string(),
                workspace: None,
                default_lens: "eco".to_string(),
                identity: Default::default(),
                flow: Default::default(),
                limits: Default::default(),
                lens: Default::default(),
                kit: crate::config::KitConfig {
                    allow: vec!["read_file".to_string()],
                    deny: vec![],
                    approval_required: vec![],
                    approved: vec![],
                },
                store: Default::default(),
                allowed_engines: vec!["openai/gpt-4o-mini".to_string()],
                sandbox: Default::default(),
                skill_policy: crate::config::SkillPolicyConfig {
                    allow: vec!["analysis".to_string()],
                    deny: vec![],
                },
            },
        );
        let task = handoff_task(vec![
            "tool:read_file".to_string(),
            "skill:analysis".to_string(),
            "engine:openai/gpt-4o-mini".to_string(),
        ]);
        assert!(evaluate_handoff_capability_policy(&config, &task).is_allowed());
    }

    #[test]
    fn handoff_capability_policy_denies_tool_outside_receiver_bounds() {
        let mut config = Config::default();
        config.agents.insert(
            "worker".to_string(),
            AgentConfig {
                default: false,
                engine: "ollama".to_string(),
                model: "llama3.2".to_string(),
                workspace: None,
                default_lens: "eco".to_string(),
                identity: Default::default(),
                flow: Default::default(),
                limits: Default::default(),
                lens: Default::default(),
                kit: Default::default(),
                store: Default::default(),
                allowed_engines: vec![],
                sandbox: Default::default(),
                skill_policy: Default::default(),
            },
        );
        let worker = config.agents.get_mut("main").expect("main");
        worker.kit.allow = vec!["read_file".to_string()];
        worker.kit.deny = vec!["shell".to_string()];
        let task = HandoffTaskEnvelope {
            from_agent_id: "worker".to_string(),
            to_agent_id: "main".to_string(),
            requested_capabilities: vec!["tool:shell".to_string()],
            ..handoff_task(vec![])
        };

        let decision = evaluate_handoff_capability_policy(&config, &task);
        assert!(matches!(
            decision,
            HandoffCapabilityPolicyDecision::DeniedCapability { .. }
        ));
    }

    #[test]
    fn handoff_capability_policy_denies_sender_outside_delegated_mode() {
        let mut config = Config::default();
        config.capability_governance.mode = "delegated".to_string();
        config.capability_governance.delegated_orchestrator_agent =
            Some("orchestrator".to_string());
        config.agents.insert(
            "worker".to_string(),
            AgentConfig {
                default: false,
                engine: "ollama".to_string(),
                model: "llama3.2".to_string(),
                workspace: None,
                default_lens: "eco".to_string(),
                identity: Default::default(),
                flow: Default::default(),
                limits: Default::default(),
                lens: Default::default(),
                kit: Default::default(),
                store: Default::default(),
                allowed_engines: vec![],
                sandbox: Default::default(),
                skill_policy: Default::default(),
            },
        );

        let task = handoff_task(vec!["tool:read_file".to_string()]);
        let decision = evaluate_handoff_capability_policy(&config, &task);
        assert!(matches!(
            decision,
            HandoffCapabilityPolicyDecision::DeniedSenderNotDelegatedOrchestrator { .. }
        ));
    }

    #[test]
    fn capability_governance_actor_allows_user_mode_for_known_agent() {
        let config = Config::default();
        let decision = evaluate_capability_governance_actor(&config, "main");
        assert!(matches!(
            decision,
            CapabilityGovernanceActorDecision::AllowedUserMode
        ));
    }

    #[test]
    fn capability_governance_actor_denies_unknown_actor() {
        let config = Config::default();
        let decision = evaluate_capability_governance_actor(&config, "unknown");
        assert!(matches!(
            decision,
            CapabilityGovernanceActorDecision::DeniedUnknownActor { .. }
        ));
    }

    #[test]
    fn capability_governance_actor_allows_delegated_orchestrator_only() {
        let mut config = Config::default();
        config.capability_governance.mode = "delegated".to_string();
        config.capability_governance.delegated_orchestrator_agent = Some("main".to_string());

        let allow = evaluate_capability_governance_actor(&config, "main");
        assert!(allow.is_allowed());

        config.agents.insert("worker".to_string(), agent());
        let deny = evaluate_capability_governance_actor(&config, "worker");
        assert!(matches!(
            deny,
            CapabilityGovernanceActorDecision::DeniedDelegatedActor { .. }
        ));
    }
}
